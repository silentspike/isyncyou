//! Verified-handle, bounded reads for progressive archive search.

use crate::envelope::{
    body_envelope_required_for_process, open_with_registered_body_key, parse_envelope_header_v1,
    BODY_ENVELOPE_HEADER_LEN, BODY_ENVELOPE_MAGIC,
};
use std::path::Path;

pub const MAX_DEEP_PLAINTEXT_BYTES: u64 = 2_097_152;
pub const MAX_DEEP_ENVELOPE_BYTES: u64 = 2_097_696;
const READ_CHUNK_BYTES: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundedArchiveBodyError {
    InvalidRoot,
    InvalidRelativePath,
    UnsafeComponent,
    UnsafeMetadata,
    TooLarge,
    MalformedEnvelope,
    MissingEnvelope,
    Decrypt,
    Cancelled,
    Io,
    UnsupportedPlatform,
}

impl std::fmt::Display for BoundedArchiveBodyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidRoot => "archive root is invalid",
            Self::InvalidRelativePath => "archive body locator is invalid",
            Self::UnsafeComponent => "archive path component is unsafe",
            Self::UnsafeMetadata => "archive body metadata is unsafe",
            Self::TooLarge => "archive body exceeds the deep-read limit",
            Self::MalformedEnvelope => "archive body envelope is malformed",
            Self::MissingEnvelope => "archive body envelope is required",
            Self::Decrypt => "archive body could not be decrypted",
            Self::Cancelled => "archive body read was cancelled",
            Self::Io => "archive body is unavailable",
            Self::UnsupportedPlatform => "verified archive body reads are unavailable",
        })
    }
}

impl std::error::Error for BoundedArchiveBodyError {}

/// Open and read one body without ever reopening a validated path.
pub fn read_bounded_archive_body(
    root: &Path,
    relative: &Path,
    should_interrupt: &(dyn Fn() -> bool + Send + Sync),
) -> Result<Vec<u8>, BoundedArchiveBodyError> {
    platform::read(root, relative, should_interrupt)
}

fn decode_bounded(raw: Vec<u8>, physical_len: u64) -> Result<Vec<u8>, BoundedArchiveBodyError> {
    if raw.starts_with(BODY_ENVELOPE_MAGIC) {
        let header = parse_envelope_header_v1(
            raw.get(..BODY_ENVELOPE_HEADER_LEN)
                .ok_or(BoundedArchiveBodyError::MalformedEnvelope)?,
            true,
        )
        .map_err(|_| BoundedArchiveBodyError::MalformedEnvelope)?;
        if header.plaintext_len > MAX_DEEP_PLAINTEXT_BYTES
            || header.expected_envelope_len > MAX_DEEP_ENVELOPE_BYTES
        {
            return Err(BoundedArchiveBodyError::TooLarge);
        }
        if header.expected_envelope_len != physical_len || raw.len() as u64 != physical_len {
            return Err(BoundedArchiveBodyError::MalformedEnvelope);
        }
        return open_with_registered_body_key(&raw).map_err(|_| BoundedArchiveBodyError::Decrypt);
    }
    if body_envelope_required_for_process() {
        return Err(BoundedArchiveBodyError::MissingEnvelope);
    }
    if physical_len > MAX_DEEP_PLAINTEXT_BYTES || raw.len() as u64 != physical_len {
        return Err(BoundedArchiveBodyError::TooLarge);
    }
    Ok(raw)
}

fn validated_physical_limit(
    prefix: &[u8],
    physical_len: u64,
) -> Result<u64, BoundedArchiveBodyError> {
    if prefix.starts_with(BODY_ENVELOPE_MAGIC) {
        let header = parse_envelope_header_v1(
            prefix
                .get(..BODY_ENVELOPE_HEADER_LEN)
                .ok_or(BoundedArchiveBodyError::MalformedEnvelope)?,
            true,
        )
        .map_err(|_| BoundedArchiveBodyError::MalformedEnvelope)?;
        if header.plaintext_len > MAX_DEEP_PLAINTEXT_BYTES
            || header.expected_envelope_len > MAX_DEEP_ENVELOPE_BYTES
        {
            return Err(BoundedArchiveBodyError::TooLarge);
        }
        if header.expected_envelope_len != physical_len {
            return Err(BoundedArchiveBodyError::MalformedEnvelope);
        }
        Ok(MAX_DEEP_ENVELOPE_BYTES)
    } else if physical_len > MAX_DEEP_PLAINTEXT_BYTES {
        Err(BoundedArchiveBodyError::TooLarge)
    } else {
        Ok(MAX_DEEP_PLAINTEXT_BYTES)
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::ffi::{CStr, CString, OsStr};
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;

    const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
    const RESOLVE_NO_SYMLINKS: u64 = 0x04;
    const RESOLVE_BENEATH: u64 = 0x08;

    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }

    fn cstring(component: &OsStr) -> Result<CString, BoundedArchiveBodyError> {
        CString::new(component.as_bytes()).map_err(|_| BoundedArchiveBodyError::InvalidRelativePath)
    }

    fn owned_fd(raw: RawFd) -> Result<OwnedFd, BoundedArchiveBodyError> {
        if raw < 0 {
            return Err(BoundedArchiveBodyError::Io);
        }
        // SAFETY: successful open/openat returns a newly owned descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    }

    fn open_dir(parent: RawFd, name: &CStr) -> Result<OwnedFd, BoundedArchiveBodyError> {
        // SAFETY: `name` is NUL-terminated and `parent` stays open for the call.
        let raw = unsafe {
            libc::openat(
                parent,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        owned_fd(raw).map_err(|_| BoundedArchiveBodyError::UnsafeComponent)
    }

    fn root_handle(root: &Path) -> Result<OwnedFd, BoundedArchiveBodyError> {
        if !root.is_absolute() {
            return Err(BoundedArchiveBodyError::InvalidRoot);
        }
        let slash = c"/";
        // SAFETY: constant C string is valid.
        let raw = unsafe {
            libc::open(
                slash.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        let mut current = owned_fd(raw).map_err(|_| BoundedArchiveBodyError::InvalidRoot)?;
        let mut saw_component = false;
        for component in root.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) if name != OsStr::new("") => {
                    current = open_dir(current.as_raw_fd(), &cstring(name)?)?;
                    saw_component = true;
                }
                _ => return Err(BoundedArchiveBodyError::InvalidRoot),
            }
        }
        if !saw_component && root != Path::new("/") {
            return Err(BoundedArchiveBodyError::InvalidRoot);
        }
        Ok(current)
    }

    fn relative_components(relative: &Path) -> Result<Vec<CString>, BoundedArchiveBodyError> {
        if relative.as_os_str().is_empty() || relative.is_absolute() {
            return Err(BoundedArchiveBodyError::InvalidRelativePath);
        }
        let mut components = Vec::new();
        for component in relative.components() {
            match component {
                Component::Normal(name) if !name.is_empty() => components.push(cstring(name)?),
                _ => return Err(BoundedArchiveBodyError::InvalidRelativePath),
            }
        }
        if components.is_empty() || components.len() > 64 {
            return Err(BoundedArchiveBodyError::InvalidRelativePath);
        }
        Ok(components)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn try_openat2(
        root: RawFd,
        relative: &Path,
    ) -> Result<Option<OwnedFd>, BoundedArchiveBodyError> {
        let path = CString::new(relative.as_os_str().as_bytes())
            .map_err(|_| BoundedArchiveBodyError::InvalidRelativePath)?;
        let how = OpenHow {
            flags: (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
            mode: 0,
            resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS,
        };
        // SAFETY: arguments point to initialized storage and the root descriptor is live.
        let raw = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                root,
                path.as_ptr(),
                &how,
                std::mem::size_of::<OpenHow>(),
            ) as RawFd
        };
        if raw >= 0 {
            return owned_fd(raw).map(Some);
        }
        let errno = std::io::Error::last_os_error().raw_os_error();
        if errno == Some(libc::ENOSYS) || errno == Some(libc::EINVAL) {
            return Ok(None);
        }
        Err(BoundedArchiveBodyError::UnsafeComponent)
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn try_openat2(
        _root: RawFd,
        _relative: &Path,
    ) -> Result<Option<OwnedFd>, BoundedArchiveBodyError> {
        Ok(None)
    }

    fn open_component_fallback(
        root: RawFd,
        components: &[CString],
    ) -> Result<OwnedFd, BoundedArchiveBodyError> {
        let mut current: Option<OwnedFd> = None;
        let mut parent = root;
        for (index, component) in components.iter().enumerate() {
            let final_component = index + 1 == components.len();
            let flags = libc::O_RDONLY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | if final_component {
                    0
                } else {
                    libc::O_DIRECTORY
                };
            // SAFETY: component is NUL-terminated and parent remains live.
            let raw = unsafe { libc::openat(parent, component.as_ptr(), flags) };
            current = Some(owned_fd(raw).map_err(|_| BoundedArchiveBodyError::UnsafeComponent)?);
            parent = current.as_ref().unwrap().as_raw_fd();
        }
        current.ok_or(BoundedArchiveBodyError::InvalidRelativePath)
    }

    fn validate_metadata(fd: RawFd) -> Result<u64, BoundedArchiveBodyError> {
        // SAFETY: zeroed `stat` is initialized by successful fstat.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fd is live and stat points to writable storage.
        if unsafe { libc::fstat(fd, &mut stat) } != 0 {
            return Err(BoundedArchiveBodyError::Io);
        }
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG
            || stat.st_uid != unsafe { libc::geteuid() }
            || stat.st_mode & 0o022 != 0
            || stat.st_nlink != 1
            || stat.st_size < 0
        {
            return Err(BoundedArchiveBodyError::UnsafeMetadata);
        }
        Ok(stat.st_size as u64)
    }

    fn read_same_handle(
        fd: OwnedFd,
        physical_len: u64,
        should_interrupt: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<Vec<u8>, BoundedArchiveBodyError> {
        let mut prefix = [0u8; BODY_ENVELOPE_HEADER_LEN];
        // SAFETY: the descriptor and destination buffer are valid for this call.
        let prefix_read =
            unsafe { libc::pread(fd.as_raw_fd(), prefix.as_mut_ptr().cast(), prefix.len(), 0) };
        if prefix_read < 0 {
            return Err(BoundedArchiveBodyError::Io);
        }
        let maximum = validated_physical_limit(&prefix[..prefix_read as usize], physical_len)?;
        let capacity =
            usize::try_from(physical_len).map_err(|_| BoundedArchiveBodyError::TooLarge)?;
        let mut bytes = Vec::with_capacity(capacity);
        let mut file = File::from(fd);
        loop {
            if should_interrupt() {
                return Err(BoundedArchiveBodyError::Cancelled);
            }
            let remaining_probe = maximum
                .checked_add(1)
                .and_then(|limit| limit.checked_sub(bytes.len() as u64))
                .ok_or(BoundedArchiveBodyError::TooLarge)?;
            if remaining_probe == 0 {
                return Err(BoundedArchiveBodyError::TooLarge);
            }
            let amount = READ_CHUNK_BYTES.min(remaining_probe as usize);
            let mut chunk = vec![0u8; amount];
            let read = file
                .read(&mut chunk)
                .map_err(|_| BoundedArchiveBodyError::Io)?;
            if should_interrupt() {
                return Err(BoundedArchiveBodyError::Cancelled);
            }
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
            if bytes.len() as u64 > maximum {
                return Err(BoundedArchiveBodyError::TooLarge);
            }
        }
        decode_bounded(bytes, physical_len)
    }

    pub(super) fn read(
        root: &Path,
        relative: &Path,
        should_interrupt: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<Vec<u8>, BoundedArchiveBodyError> {
        if should_interrupt() {
            return Err(BoundedArchiveBodyError::Cancelled);
        }
        let root = root_handle(root)?;
        let components = relative_components(relative)?;
        let target = match try_openat2(root.as_raw_fd(), relative)? {
            Some(target) => target,
            None => open_component_fallback(root.as_raw_fd(), &components)?,
        };
        let physical_len = validate_metadata(target.as_raw_fd())?;
        read_same_handle(target, physical_len, should_interrupt)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::envelope::{
        reset_body_envelope_requirement_for_tests, reset_body_keys_for_tests, seal, set_body_key,
        BodyKey,
    };
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::Mutex;

    static ENVELOPE_STATE: Mutex<()> = Mutex::new(());
    const KEY: BodyKey = [42; 32];

    fn fixture(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("mail")).unwrap();
        let relative = std::path::PathBuf::from("mail/body.bin");
        std::fs::write(root.path().join(&relative), bytes).unwrap();
        (root, relative)
    }

    #[test]
    fn archive_deep_body_open_is_no_follow_fstat_and_cap_plus_one() {
        let (root, relative) = fixture(b"bounded body");
        let bytes = read_bounded_archive_body(root.path(), &relative, &|| false).unwrap();
        assert_eq!(bytes, b"bounded body");
    }

    #[test]
    fn archive_deep_body_rejects_unowned_or_group_world_writable_file() {
        let (root, relative) = fixture(b"unsafe");
        let path = root.path().join(&relative);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o622)).unwrap();
        assert_eq!(
            read_bounded_archive_body(root.path(), &relative, &|| false),
            Err(BoundedArchiveBodyError::UnsafeMetadata)
        );
    }

    #[test]
    fn archive_deep_body_rejects_unix_hardlink_before_read() {
        let (root, relative) = fixture(b"linked");
        std::fs::hard_link(
            root.path().join(&relative),
            root.path().join("mail/second.bin"),
        )
        .unwrap();
        assert_eq!(
            read_bounded_archive_body(root.path(), &relative, &|| false),
            Err(BoundedArchiveBodyError::UnsafeMetadata)
        );
    }

    #[test]
    fn archive_deep_body_rejects_exact_cap_plus_one_before_decrypt() {
        let bytes = vec![b'x'; MAX_DEEP_PLAINTEXT_BYTES as usize + 1];
        let (root, relative) = fixture(&bytes);
        assert_eq!(
            read_bounded_archive_body(root.path(), &relative, &|| false),
            Err(BoundedArchiveBodyError::TooLarge)
        );
    }

    #[test]
    fn archive_deep_envelope_exact_max_matches_checked_serialization_formula() {
        let _state = ENVELOPE_STATE.lock().unwrap();
        reset_body_keys_for_tests();
        reset_body_envelope_requirement_for_tests();
        set_body_key(7, KEY);
        let plaintext = vec![b'x'; MAX_DEEP_PLAINTEXT_BYTES as usize];
        let sealed = seal(&plaintext, &KEY, 7);
        assert_eq!(sealed.len() as u64, MAX_DEEP_ENVELOPE_BYTES);
        let (root, relative) = fixture(&sealed);
        assert_eq!(
            read_bounded_archive_body(root.path(), &relative, &|| false).unwrap(),
            plaintext
        );
        reset_body_keys_for_tests();
    }

    #[test]
    fn archive_deep_envelope_rejects_declared_plaintext_one_over_before_allocation() {
        let mut header = [0u8; BODY_ENVELOPE_HEADER_LEN];
        header[..4].copy_from_slice(BODY_ENVELOPE_MAGIC);
        header[4] = 1;
        header[12..16].copy_from_slice(&65_536u32.to_be_bytes());
        header[16..24].copy_from_slice(&(MAX_DEEP_PLAINTEXT_BYTES + 1).to_be_bytes());
        let (root, relative) = fixture(&header);
        assert_eq!(
            read_bounded_archive_body(root.path(), &relative, &|| false),
            Err(BoundedArchiveBodyError::TooLarge)
        );
    }

    #[test]
    fn archive_deep_envelope_rejects_truncated_or_trailing_ciphertext() {
        let _state = ENVELOPE_STATE.lock().unwrap();
        let sealed = seal(b"body", &KEY, 1);
        let cases = vec![
            sealed[..sealed.len() - 1].to_vec(),
            [sealed.as_slice(), b"x"].concat(),
        ];
        for bytes in cases {
            let (root, relative) = fixture(&bytes);
            assert_eq!(
                read_bounded_archive_body(root.path(), &relative, &|| false),
                Err(BoundedArchiveBodyError::MalformedEnvelope)
            );
        }
    }

    #[test]
    fn archive_deep_malformed_isye_never_falls_back_to_plaintext() {
        let (root, relative) = fixture(b"ISYE malformed");
        assert_eq!(
            read_bounded_archive_body(root.path(), &relative, &|| false),
            Err(BoundedArchiveBodyError::MalformedEnvelope)
        );
    }

    #[test]
    fn archive_deep_required_envelope_rejects_plaintext_before_body_allocation() {
        let _state = ENVELOPE_STATE.lock().unwrap();
        reset_body_envelope_requirement_for_tests();
        crate::envelope::require_body_envelope_for_process();
        let (root, relative) = fixture(b"plaintext");
        assert_eq!(
            read_bounded_archive_body(root.path(), &relative, &|| false),
            Err(BoundedArchiveBodyError::MissingEnvelope)
        );
        reset_body_envelope_requirement_for_tests();
    }

    #[test]
    fn archive_deep_unix_rejects_symlink_and_magiclink_in_every_component() {
        let root = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        std::fs::write(target.path().join("body"), b"secret").unwrap();
        symlink(target.path(), root.path().join("linked")).unwrap();
        assert_eq!(
            read_bounded_archive_body(root.path(), Path::new("linked/body"), &|| false),
            Err(BoundedArchiveBodyError::UnsafeComponent)
        );
    }

    #[test]
    fn archive_deep_unix_rejects_symlink_in_configured_root_ancestor() {
        let outer = tempfile::tempdir().unwrap();
        let actual = tempfile::tempdir().unwrap();
        std::fs::create_dir(actual.path().join("mail")).unwrap();
        std::fs::write(actual.path().join("mail/body"), b"secret").unwrap();
        symlink(actual.path(), outer.path().join("root-link")).unwrap();
        assert_eq!(
            read_bounded_archive_body(
                &outer.path().join("root-link"),
                Path::new("mail/body"),
                &|| false
            ),
            Err(BoundedArchiveBodyError::UnsafeComponent)
        );
    }

    #[test]
    fn archive_cancellation_after_blocked_read_prevents_next_chunk_or_provider_call() {
        let (root, relative) = fixture(&vec![b'x'; READ_CHUNK_BYTES * 2]);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let result = read_bounded_archive_body(root.path(), &relative, &|| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 2
        });
        assert_eq!(result, Err(BoundedArchiveBodyError::Cancelled));
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Component;
    use std::ptr::{null, null_mut};
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        FileBasicInformation, FileStandardInformation, NtCreateFile, NtQueryInformationFile,
        FILE_BASIC_INFORMATION, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
        FILE_OPEN_REPARSE_POINT, FILE_STANDARD_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, LocalFree, HANDLE, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE, UNICODE_STRING,
    };
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        CreateWellKnownSid, EqualSid, GetAce, GetLengthSid, GetTokenInformation, IsValidSid,
        TokenUser, WinAuthenticatedUserSid, WinBuiltinUsersSid, WinWorldSid,
        ACE_INHERITED_OBJECT_TYPE_PRESENT, ACE_OBJECT_TYPE_PRESENT, ACL, DACL_SECURITY_INFORMATION,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        ReadFile, SetFilePointerEx, DELETE, FILE_APPEND_DATA, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_BEGIN, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_WRITE_DATA, READ_CONTROL, SYNCHRONIZE, WRITE_DAC, WRITE_OWNER,
    };
    use windows_sys::Win32::System::SystemServices::{
        ACCESS_ALLOWED_ACE_TYPE, ACCESS_ALLOWED_CALLBACK_ACE_TYPE,
        ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE, ACCESS_ALLOWED_COMPOUND_ACE_TYPE,
        ACCESS_ALLOWED_OBJECT_ACE_TYPE,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    const GENERIC_WRITE_MASK: u32 = 0x4000_0000;
    const GENERIC_ALL_MASK: u32 = 0x1000_0000;
    const MAX_SID_BYTES: usize = 68;

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: this wrapper uniquely owns a successful NtCreateFile handle.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    struct LocalSecurityDescriptor(PSECURITY_DESCRIPTOR);

    impl Drop for LocalSecurityDescriptor {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: GetSecurityInfo returns a LocalAlloc-backed descriptor.
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
    }

    fn validate_component(component: &[u16]) -> Result<(), BoundedArchiveBodyError> {
        if component.is_empty()
            || component == [b'.' as u16]
            || component == [b'.' as u16, b'.' as u16]
            || component.len() > 255
            || component.iter().any(|unit| {
                *unit == 0
                    || *unit < 0x20
                    || matches!(
                        *unit,
                        b'<' as u16
                            | b'>' as u16
                            | b':' as u16
                            | b'"' as u16
                            | b'/' as u16
                            | b'\\' as u16
                            | b'|' as u16
                            | b'?' as u16
                            | b'*' as u16
                    )
            })
        {
            return Err(BoundedArchiveBodyError::InvalidRelativePath);
        }
        Ok(())
    }

    fn split_root(root: &Path) -> Result<Vec<Vec<u16>>, BoundedArchiveBodyError> {
        let wide = root.as_os_str().encode_wide().collect::<Vec<_>>();
        if wide.is_empty()
            || wide
                .iter()
                .any(|unit| *unit == 0 || *unit < 0x20 || *unit == b'/' as u16)
        {
            return Err(BoundedArchiveBodyError::InvalidRoot);
        }
        let slash = b'\\' as u16;
        let colon = b':' as u16;
        let is_drive = wide.len() >= 3
            && (wide[0] as u8).is_ascii_alphabetic()
            && wide[1] == colon
            && wide[2] == slash;
        let is_unc = wide.len() >= 5 && wide[0] == slash && wide[1] == slash;
        if !is_drive && !is_unc {
            return Err(BoundedArchiveBodyError::InvalidRoot);
        }
        if wide.starts_with(&[slash, slash, b'?' as u16, slash])
            || wide.starts_with(&[slash, slash, b'.' as u16, slash])
            || wide.starts_with(&[slash, b'?' as u16, b'?' as u16, slash])
        {
            return Err(BoundedArchiveBodyError::InvalidRoot);
        }
        let start = if is_drive { 3 } else { 2 };
        let mut parts = wide[start..]
            .split(|unit| *unit == slash)
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        if parts.last().is_some_and(Vec::is_empty) {
            parts.pop();
        }
        if parts.iter().any(Vec::is_empty) {
            return Err(BoundedArchiveBodyError::InvalidRoot);
        }
        for part in &parts {
            validate_component(part).map_err(|_| BoundedArchiveBodyError::InvalidRoot)?;
        }
        if is_unc {
            if parts.len() < 2 {
                return Err(BoundedArchiveBodyError::InvalidRoot);
            }
        }
        Ok(parts)
    }

    fn nt_root_name(root: &Path) -> Result<Vec<u16>, BoundedArchiveBodyError> {
        let wide = root.as_os_str().encode_wide().collect::<Vec<_>>();
        let parts = split_root(root)?;
        let slash = b'\\' as u16;
        let is_drive = wide.get(1) == Some(&(b':' as u16));
        let mut result = "\\??\\".encode_utf16().collect::<Vec<_>>();
        if is_drive {
            result.extend_from_slice(&wide[..3]);
        } else {
            result.extend("UNC\\".encode_utf16());
        }
        for (index, part) in parts.iter().enumerate() {
            if index > 0 || is_drive {
                if result.last() != Some(&slash) {
                    result.push(slash);
                }
            }
            result.extend_from_slice(part);
        }
        Ok(result)
    }

    fn relative_components(relative: &Path) -> Result<Vec<Vec<u16>>, BoundedArchiveBodyError> {
        if relative.as_os_str().is_empty() || relative.is_absolute() {
            return Err(BoundedArchiveBodyError::InvalidRelativePath);
        }
        let mut components = Vec::new();
        for component in relative.components() {
            let Component::Normal(value) = component else {
                return Err(BoundedArchiveBodyError::InvalidRelativePath);
            };
            let wide = value.encode_wide().collect::<Vec<_>>();
            validate_component(&wide)?;
            components.push(wide);
        }
        if components.is_empty() || components.len() > 64 {
            return Err(BoundedArchiveBodyError::InvalidRelativePath);
        }
        Ok(components)
    }

    fn nt_open(
        parent: HANDLE,
        name: &mut [u16],
        directory: bool,
    ) -> Result<OwnedHandle, BoundedArchiveBodyError> {
        let byte_len = name
            .len()
            .checked_mul(2)
            .and_then(|value| u16::try_from(value).ok())
            .ok_or(BoundedArchiveBodyError::InvalidRelativePath)?;
        let unicode = UNICODE_STRING {
            Length: byte_len,
            MaximumLength: byte_len,
            Buffer: name.as_mut_ptr(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: parent,
            ObjectName: &unicode,
            Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
            SecurityDescriptor: null(),
            SecurityQualityOfService: null(),
        };
        let mut handle: HANDLE = null_mut();
        let mut io = IO_STATUS_BLOCK::default();
        let options = FILE_OPEN_REPARSE_POINT
            | FILE_SYNCHRONOUS_IO_NONALERT
            | if directory {
                FILE_DIRECTORY_FILE
            } else {
                FILE_NON_DIRECTORY_FILE
            };
        // SAFETY: all pointers reference initialized storage for the duration of the call.
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                FILE_READ_DATA | FILE_READ_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE,
                &attributes,
                &mut io,
                null(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_OPEN,
                options,
                null(),
                0,
            )
        };
        if status < 0 || handle.is_null() {
            return Err(BoundedArchiveBodyError::UnsafeComponent);
        }
        let handle = OwnedHandle(handle);
        let (basic, _) = query_metadata(handle.0)?;
        if basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(BoundedArchiveBodyError::UnsafeComponent);
        }
        Ok(handle)
    }

    fn query_metadata(
        handle: HANDLE,
    ) -> Result<(FILE_BASIC_INFORMATION, FILE_STANDARD_INFORMATION), BoundedArchiveBodyError> {
        let mut io = IO_STATUS_BLOCK::default();
        let mut basic = FILE_BASIC_INFORMATION::default();
        // SAFETY: handle is live and the typed destination has the advertised size.
        let basic_status = unsafe {
            NtQueryInformationFile(
                handle,
                &mut io,
                (&mut basic as *mut FILE_BASIC_INFORMATION).cast(),
                std::mem::size_of::<FILE_BASIC_INFORMATION>() as u32,
                FileBasicInformation,
            )
        };
        let mut standard = FILE_STANDARD_INFORMATION::default();
        // SAFETY: handle is live and the typed destination has the advertised size.
        let standard_status = unsafe {
            NtQueryInformationFile(
                handle,
                &mut io,
                (&mut standard as *mut FILE_STANDARD_INFORMATION).cast(),
                std::mem::size_of::<FILE_STANDARD_INFORMATION>() as u32,
                FileStandardInformation,
            )
        };
        if basic_status < 0 || standard_status < 0 {
            return Err(BoundedArchiveBodyError::Io);
        }
        Ok((basic, standard))
    }

    fn process_user_sid() -> Result<(OwnedHandle, Vec<u8>, PSID), BoundedArchiveBodyError> {
        let mut token: HANDLE = null_mut();
        // SAFETY: output points to an initialized handle slot.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(BoundedArchiveBodyError::UnsafeMetadata);
        }
        let token = OwnedHandle(token);
        let mut required = 0u32;
        // The first call intentionally obtains the exact buffer size.
        unsafe {
            GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut required);
        }
        if required < std::mem::size_of::<TOKEN_USER>() as u32 || required > 64 * 1024 {
            return Err(BoundedArchiveBodyError::UnsafeMetadata);
        }
        let mut buffer = vec![0u8; required as usize];
        // SAFETY: buffer is writable for the exact requested length.
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(BoundedArchiveBodyError::UnsafeMetadata);
        }
        let user_sid = unsafe { (*(buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid };
        Ok((token, buffer, user_sid))
    }

    fn well_known_sid(kind: i32) -> Result<([u8; MAX_SID_BYTES], u32), BoundedArchiveBodyError> {
        let mut bytes = [0u8; MAX_SID_BYTES];
        let mut length = bytes.len() as u32;
        // SAFETY: destination is writable and length describes its capacity.
        if unsafe { CreateWellKnownSid(kind, null_mut(), bytes.as_mut_ptr().cast(), &mut length) }
            == 0
        {
            return Err(BoundedArchiveBodyError::UnsafeMetadata);
        }
        Ok((bytes, length))
    }

    fn read_ace_u32(
        raw: *const u8,
        ace_size: usize,
        offset: usize,
    ) -> Result<u32, BoundedArchiveBodyError> {
        if offset
            .checked_add(std::mem::size_of::<u32>())
            .is_none_or(|end| end > ace_size)
        {
            return Err(BoundedArchiveBodyError::UnsafeMetadata);
        }
        // SAFETY: the checked range is inside the ACE. ACE fields are not guaranteed
        // to have Rust alignment, so the integer is read unaligned.
        Ok(unsafe { std::ptr::read_unaligned(raw.add(offset).cast::<u32>()) })
    }

    fn allow_ace_mask_and_sid(
        raw: *mut c_void,
        header: &windows_sys::Win32::Security::ACE_HEADER,
    ) -> Result<Option<(u32, PSID)>, BoundedArchiveBodyError> {
        let ace_type = u32::from(header.AceType);
        let ace_size = usize::from(header.AceSize);
        let raw = raw.cast::<u8>();
        let sid_offset = match ace_type {
            ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE => 8usize,
            ACCESS_ALLOWED_OBJECT_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE => {
                let flags = read_ace_u32(raw, ace_size, 8)?;
                if flags & !(ACE_OBJECT_TYPE_PRESENT | ACE_INHERITED_OBJECT_TYPE_PRESENT) != 0 {
                    return Err(BoundedArchiveBodyError::UnsafeMetadata);
                }
                12usize
                    .checked_add(if flags & ACE_OBJECT_TYPE_PRESENT != 0 {
                        16
                    } else {
                        0
                    })
                    .and_then(|offset| {
                        offset.checked_add(if flags & ACE_INHERITED_OBJECT_TYPE_PRESENT != 0 {
                            16
                        } else {
                            0
                        })
                    })
                    .ok_or(BoundedArchiveBodyError::UnsafeMetadata)?
            }
            // Compound allow ACEs contain two SIDs and are obsolete for file ACLs. Do
            // not guess which principal receives authority.
            ACCESS_ALLOWED_COMPOUND_ACE_TYPE => {
                return Err(BoundedArchiveBodyError::UnsafeMetadata)
            }
            _ => return Ok(None),
        };
        let mask = read_ace_u32(raw, ace_size, 4)?;
        if sid_offset >= ace_size {
            return Err(BoundedArchiveBodyError::UnsafeMetadata);
        }
        // SAFETY: sid_offset is inside this ACE. IsValidSid validates the variable
        // structure before GetLengthSid or EqualSid inspect it further.
        let sid = unsafe { raw.add(sid_offset).cast_mut().cast::<c_void>() };
        if unsafe { IsValidSid(sid) } == 0 {
            return Err(BoundedArchiveBodyError::UnsafeMetadata);
        }
        let sid_length = usize::try_from(unsafe { GetLengthSid(sid) })
            .map_err(|_| BoundedArchiveBodyError::UnsafeMetadata)?;
        if sid_length == 0 || sid_length > ace_size - sid_offset {
            return Err(BoundedArchiveBodyError::UnsafeMetadata);
        }
        Ok(Some((mask, sid)))
    }

    fn validate_owner_and_dacl(handle: HANDLE) -> Result<(), BoundedArchiveBodyError> {
        let mut owner: PSID = null_mut();
        let mut dacl: *mut ACL = null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: GetSecurityInfo initializes the requested owner, DACL, and descriptor pointers.
        let status = unsafe {
            GetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut descriptor,
            )
        };
        if status != 0 || owner.is_null() || dacl.is_null() || descriptor.is_null() {
            return Err(BoundedArchiveBodyError::UnsafeMetadata);
        }
        let _descriptor = LocalSecurityDescriptor(descriptor);
        let (_token, _token_bytes, user_sid) = process_user_sid()?;
        // SAFETY: both SIDs come from validated Windows security APIs.
        if unsafe { EqualSid(owner, user_sid) } == 0 {
            return Err(BoundedArchiveBodyError::UnsafeMetadata);
        }

        let (world, _) = well_known_sid(WinWorldSid)?;
        let (authenticated, _) = well_known_sid(WinAuthenticatedUserSid)?;
        let (users, _) = well_known_sid(WinBuiltinUsersSid)?;
        let dangerous_mask = FILE_WRITE_DATA
            | FILE_APPEND_DATA
            | DELETE
            | WRITE_DAC
            | WRITE_OWNER
            | GENERIC_WRITE_MASK
            | GENERIC_ALL_MASK;
        let ace_count = unsafe { (*dacl).AceCount };
        for index in 0..u32::from(ace_count) {
            let mut raw: *mut c_void = null_mut();
            // SAFETY: index is bounded by the ACL's AceCount.
            if unsafe { GetAce(dacl, index, &mut raw) } == 0 || raw.is_null() {
                return Err(BoundedArchiveBodyError::UnsafeMetadata);
            }
            let header = unsafe { &*(raw.cast::<windows_sys::Win32::Security::ACE_HEADER>()) };
            let Some((mask, sid)) = allow_ace_mask_and_sid(raw, header)? else {
                continue;
            };
            if mask & dangerous_mask == 0 {
                continue;
            }
            // SAFETY: allow_ace_mask_and_sid validated the complete SID range.
            let broad = unsafe {
                EqualSid(sid, world.as_ptr().cast_mut().cast())
                    | EqualSid(sid, authenticated.as_ptr().cast_mut().cast())
                    | EqualSid(sid, users.as_ptr().cast_mut().cast())
            };
            if broad != 0 {
                return Err(BoundedArchiveBodyError::UnsafeMetadata);
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn install_test_broad_object_allow_ace(
        path: &Path,
    ) -> Result<(), BoundedArchiveBodyError> {
        use windows_sys::Win32::Security::Authorization::SetNamedSecurityInfoW;
        use windows_sys::Win32::Security::{
            AddAccessAllowedAceEx, AddAccessAllowedObjectAce, InitializeAcl, ACL_REVISION_DS,
        };

        let (_token, _user_buffer, user_sid) = process_user_sid()?;
        let (world, _) = well_known_sid(WinWorldSid)?;
        let mut acl_storage = vec![0u32; 256];
        let acl = acl_storage.as_mut_ptr().cast::<ACL>();
        let acl_bytes = u32::try_from(acl_storage.len() * std::mem::size_of::<u32>())
            .map_err(|_| BoundedArchiveBodyError::UnsafeMetadata)?;
        if unsafe { InitializeAcl(acl, acl_bytes, ACL_REVISION_DS) } == 0
            || unsafe { AddAccessAllowedAceEx(acl, ACL_REVISION_DS, 0, GENERIC_ALL_MASK, user_sid) }
                == 0
            || unsafe {
                AddAccessAllowedObjectAce(
                    acl,
                    ACL_REVISION_DS,
                    0,
                    FILE_WRITE_DATA,
                    null(),
                    null(),
                    world.as_ptr().cast_mut().cast(),
                )
            } == 0
        {
            return Err(BoundedArchiveBodyError::UnsafeMetadata);
        }
        let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        wide.push(0);
        let status = unsafe {
            SetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                acl,
                null_mut(),
            )
        };
        if status != 0 {
            return Err(BoundedArchiveBodyError::UnsafeMetadata);
        }
        Ok(())
    }

    fn read_same_handle(
        handle: HANDLE,
        physical_len: u64,
        should_interrupt: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<Vec<u8>, BoundedArchiveBodyError> {
        let mut prefix = [0u8; BODY_ENVELOPE_HEADER_LEN];
        let mut prefix_read = 0u32;
        // SAFETY: the synchronous handle is live and prefix is writable.
        if unsafe {
            ReadFile(
                handle,
                prefix.as_mut_ptr(),
                prefix.len() as u32,
                &mut prefix_read,
                null_mut(),
            )
        } == 0
        {
            return Err(BoundedArchiveBodyError::Io);
        }
        // SAFETY: rewinds the same synchronous handle; no path is reopened.
        if unsafe { SetFilePointerEx(handle, 0, null_mut(), FILE_BEGIN) } == 0 {
            return Err(BoundedArchiveBodyError::Io);
        }
        let maximum = validated_physical_limit(&prefix[..prefix_read as usize], physical_len)?;
        let capacity =
            usize::try_from(physical_len).map_err(|_| BoundedArchiveBodyError::TooLarge)?;
        let mut bytes = Vec::with_capacity(capacity);
        while bytes.len() as u64 <= maximum {
            if should_interrupt() {
                return Err(BoundedArchiveBodyError::Cancelled);
            }
            let remaining = maximum
                .checked_add(1)
                .and_then(|limit| limit.checked_sub(bytes.len() as u64))
                .ok_or(BoundedArchiveBodyError::TooLarge)?;
            if remaining == 0 {
                return Err(BoundedArchiveBodyError::TooLarge);
            }
            let amount = READ_CHUNK_BYTES.min(remaining as usize);
            let mut chunk = vec![0u8; amount];
            let mut read = 0u32;
            // SAFETY: handle is synchronous and the buffer is writable for `amount` bytes.
            if unsafe {
                ReadFile(
                    handle,
                    chunk.as_mut_ptr(),
                    amount as u32,
                    &mut read,
                    null_mut(),
                )
            } == 0
            {
                return Err(BoundedArchiveBodyError::Io);
            }
            if should_interrupt() {
                return Err(BoundedArchiveBodyError::Cancelled);
            }
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read as usize]);
            if bytes.len() as u64 > maximum {
                return Err(BoundedArchiveBodyError::TooLarge);
            }
        }
        decode_bounded(bytes, physical_len)
    }

    pub(super) fn read(
        root: &Path,
        relative: &Path,
        should_interrupt: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<Vec<u8>, BoundedArchiveBodyError> {
        if should_interrupt() {
            return Err(BoundedArchiveBodyError::Cancelled);
        }
        let mut root_name = nt_root_name(root)?;
        let root_handle = nt_open(null_mut(), &mut root_name, true)
            .map_err(|_| BoundedArchiveBodyError::InvalidRoot)?;
        let components = relative_components(relative)?;
        let component_count = components.len();
        let mut parent = root_handle;
        for (index, mut component) in components.into_iter().enumerate() {
            let final_component = index + 1 == component_count;
            let child = nt_open(parent.0, &mut component, !final_component)?;
            parent = child;
        }
        let (basic, standard) = query_metadata(parent.0)?;
        if standard.Directory
            || standard.NumberOfLinks != 1
            || standard.EndOfFile < 0
            || basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return Err(BoundedArchiveBodyError::UnsafeMetadata);
        }
        validate_owner_and_dacl(parent.0)?;
        read_same_handle(parent.0, standard.EndOfFile as u64, should_interrupt)
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::os::windows::fs::symlink_file;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fixture(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("mail")).unwrap();
        let relative = std::path::PathBuf::from("mail/body.bin");
        std::fs::write(root.path().join(&relative), bytes).unwrap();
        (root, relative)
    }

    #[test]
    fn archive_deep_windows_root_open_reads_owner_controlled_regular_file() {
        let (root, relative) = fixture(b"bounded body");
        assert_eq!(
            read_bounded_archive_body(root.path(), &relative, &|| false).unwrap(),
            b"bounded body"
        );
    }

    #[test]
    fn archive_deep_windows_rejects_ancestor_and_final_reparse_points() {
        let outer = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        std::fs::create_dir(target.path().join("mail")).unwrap();
        std::fs::write(target.path().join("mail/body.bin"), b"secret").unwrap();
        let junction = outer.path().join("root-junction");
        let status = Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&junction)
            .arg(target.path())
            .status()
            .unwrap();
        assert!(status.success(), "failed to create test directory junction");
        assert_eq!(
            read_bounded_archive_body(&junction, Path::new("mail/body.bin"), &|| false),
            Err(BoundedArchiveBodyError::InvalidRoot)
        );

        let configured_root = junction.join("configured-root");
        std::fs::create_dir(target.path().join("configured-root")).unwrap();
        std::fs::create_dir(target.path().join("configured-root/mail")).unwrap();
        std::fs::write(
            target.path().join("configured-root/mail/body.bin"),
            b"secret",
        )
        .unwrap();
        assert!(matches!(
            read_bounded_archive_body(&configured_root, Path::new("mail/body.bin"), &|| false),
            Err(BoundedArchiveBodyError::InvalidRoot | BoundedArchiveBodyError::UnsafeComponent)
        ));

        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("mail")).unwrap();
        let final_link = root.path().join("mail/body.bin");
        symlink_file(target.path().join("mail/body.bin"), &final_link).unwrap();
        assert_eq!(
            read_bounded_archive_body(root.path(), Path::new("mail/body.bin"), &|| false),
            Err(BoundedArchiveBodyError::UnsafeComponent)
        );
    }

    #[test]
    fn archive_deep_windows_uses_same_verified_handle_after_path_replacement() {
        let (root, relative) = fixture(b"verified original");
        let target = root.path().join(&relative);
        let moved = root.path().join("mail/original.bin");
        let calls = AtomicUsize::new(0);
        let bytes = read_bounded_archive_body(root.path(), &relative, &|| {
            if calls.fetch_add(1, Ordering::SeqCst) == 1 {
                std::fs::rename(&target, &moved).unwrap();
                std::fs::write(&target, b"replacement").unwrap();
            }
            false
        })
        .unwrap();
        assert_eq!(bytes, b"verified original");
        assert_eq!(std::fs::read(target).unwrap(), b"replacement");
    }

    #[test]
    fn archive_deep_body_rejects_windows_hardlink_before_read() {
        let (root, relative) = fixture(b"linked");
        std::fs::hard_link(
            root.path().join(&relative),
            root.path().join("mail/second.bin"),
        )
        .unwrap();
        assert_eq!(
            read_bounded_archive_body(root.path(), &relative, &|| false),
            Err(BoundedArchiveBodyError::UnsafeMetadata)
        );
    }

    #[test]
    fn archive_deep_windows_validates_owner_and_rejects_broad_write_acl() {
        let (root, relative) = fixture(b"unsafe acl");
        let path = root.path().join(&relative);
        let status = Command::new("icacls")
            .arg(&path)
            .args(["/grant", "*S-1-1-0:(W)"])
            .status()
            .unwrap();
        assert!(status.success(), "failed to create test broad-write ACL");
        assert_eq!(
            read_bounded_archive_body(root.path(), &relative, &|| false),
            Err(BoundedArchiveBodyError::UnsafeMetadata)
        );
    }

    #[test]
    fn archive_deep_windows_rejects_nonstandard_broad_allow_ace() {
        let (root, relative) = fixture(b"unsafe object acl");
        platform::install_test_broad_object_allow_ace(&root.path().join(&relative)).unwrap();
        assert_eq!(
            read_bounded_archive_body(root.path(), &relative, &|| false),
            Err(BoundedArchiveBodyError::UnsafeMetadata)
        );
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::*;

    pub(super) fn read(
        _root: &Path,
        _relative: &Path,
        _should_interrupt: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<Vec<u8>, BoundedArchiveBodyError> {
        Err(BoundedArchiveBodyError::UnsupportedPlatform)
    }
}
