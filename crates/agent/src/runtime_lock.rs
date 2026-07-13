//! A best-effort exclusive advisory file lock (#639).
//!
//! Held for the lifetime of the returned guard (dropping it, including on process exit/crash,
//! releases the lock via the closed fd). Used to serialize the product runtime across processes:
//! a second product runtime that cannot take the lock **fails closed** rather than racing the
//! credential store / activation record.

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
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC)
            .open(path)?;
        // LOCK_EX | LOCK_NB: exclusive, do not block — a held lock returns EWOULDBLOCK.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(FileLock { _file: file }));
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => Ok(None),
            _ => Err(err),
        }
    }

    #[cfg(not(unix))]
    pub fn try_acquire_exclusive(path: &Path) -> std::io::Result<Option<FileLock>> {
        // No portable advisory lock; create-new gives a coarse single-holder guard.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(file) => Ok(Some(FileLock { _file: file })),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(all(test, unix))]
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
}
