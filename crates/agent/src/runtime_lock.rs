//! An exclusive advisory file lock (#639).
//!
//! Held for the lifetime of the returned guard (dropping it, including on process exit/crash,
//! releases the lock via the closed fd). Used to serialize product credential transactions and
//! onboarding-journal transactions across processes. A competing transaction that cannot take its
//! domain lock fails closed rather than racing encrypted store updates.

use std::path::Path;

/// An acquired exclusive lock. Releasing happens on drop (the OS releases the advisory lock when
/// the underlying file descriptor is closed).
#[derive(Debug)]
pub struct FileLock {
    _file: std::fs::File,
}

impl FileLock {
    /// Acquire an exclusive, non-blocking advisory lock on `path` (creating it owner-only). Returns
    /// `Ok(None)` if another holder already owns it (the caller must fail closed), `Err` on I/O error.
    #[cfg(unix)]
    pub fn try_acquire_exclusive(path: &Path) -> std::io::Result<Option<FileLock>> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "runtime lock is not an owner-controlled regular file",
            ));
        }
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        Self::try_lock_file(file)
    }

    #[cfg(not(unix))]
    pub fn try_acquire_exclusive(path: &Path) -> std::io::Result<Option<FileLock>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Self::try_lock_file(file)
    }

    fn try_lock_file(file: std::fs::File) -> std::io::Result<Option<FileLock>> {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Some(FileLock { _file: file })),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FileLock;

    #[test]
    fn second_holder_is_refused_and_release_on_drop_allows_reacquire() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".product-runtime.lock");
        let first = FileLock::try_acquire_exclusive(&path).unwrap();
        assert!(first.is_some(), "first holder acquires");
        // a second concurrent holder must be refused (fail closed)
        assert!(
            FileLock::try_acquire_exclusive(&path).unwrap().is_none(),
            "second holder must be refused while the first is held"
        );
        drop(first);
        // after release, it can be re-acquired
        assert!(FileLock::try_acquire_exclusive(&path).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_lock_path_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::fs::write(&target, b"not a lock").unwrap();
        let path = tmp.path().join(".product-runtime.lock");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(FileLock::try_acquire_exclusive(&path).is_err());
    }
}
