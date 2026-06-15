//! Privileged **fanotify** change-source — the mount-wide server backend from
//! plan §5.2 (the unprivileged desktop default is the inotify [`crate::watch`]
//! watcher). fanotify needs `CAP_SYS_ADMIN`, sees a whole marked subtree without
//! per-directory watches, and has no inotify-style queue overflow.
//!
//! Like the inotify accelerator, this only lets the engine react quickly; the
//! periodic [`crate::reconcile()`] diff stays the source of truth. Events are
//! reported as [`RawEvent`]s, resolving each event fd to a path via
//! `/proc/self/fd/<fd>` (then closing it).

use crate::watcher::RawEvent;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::Duration;

/// A running fanotify watch over a marked directory subtree.
pub struct FanotifyWatcher {
    fd: OwnedFd,
}

impl FanotifyWatcher {
    /// Initialize fanotify and mark `dir` for modify / close-after-write events on
    /// the directory and its direct children (`FAN_EVENT_ON_CHILD`). For a whole
    /// subtree the server marks the mount instead (`FAN_MARK_MOUNT`, plan §5.2).
    /// Returns `PermissionDenied` when not run with `CAP_SYS_ADMIN`.
    pub fn mark_dir(dir: &Path) -> io::Result<Self> {
        // SAFETY: a plain syscall with valid constant flags; returns an fd or -1.
        let raw = unsafe {
            libc::fanotify_init(
                libc::FAN_CLASS_NOTIF | libc::FAN_CLOEXEC | libc::FAN_NONBLOCK,
                libc::O_RDONLY as u32,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh, valid, owned fd from fanotify_init.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let cpath = std::ffi::CString::new(dir.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        // SAFETY: valid group fd + NUL-terminated path; FAN_MARK_ADD on a directory.
        let rc = unsafe {
            libc::fanotify_mark(
                fd.as_raw_fd(),
                libc::FAN_MARK_ADD,
                libc::FAN_MODIFY | libc::FAN_CLOSE_WRITE | libc::FAN_EVENT_ON_CHILD,
                libc::AT_FDCWD,
                cpath.as_ptr(),
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(FanotifyWatcher { fd })
    }

    /// Poll for raw events up to `timeout`. An empty vec means nothing happened in
    /// the window. The [`crate::source::ChangeSource`] impl wraps this through the
    /// shared [`crate::watcher::Coalescer`] to produce coalesced `FsChange`s.
    pub fn poll_raw(&self, timeout: Duration) -> Vec<RawEvent> {
        let mut pfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: a single valid pollfd for `timeout`.
        let n = unsafe { libc::poll(&mut pfd, 1, ms) };
        if n <= 0 || pfd.revents & libc::POLLIN == 0 {
            return Vec::new();
        }
        let mut buf = [0u8; 8192];
        // SAFETY: read fanotify records into our stack buffer.
        let len = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        if len <= 0 {
            return Vec::new();
        }
        self.parse(&buf[..len as usize])
    }

    /// Parse a buffer of `fanotify_event_metadata` records into events, resolving
    /// and closing each event fd.
    fn parse(&self, buf: &[u8]) -> Vec<RawEvent> {
        let meta_size = std::mem::size_of::<libc::fanotify_event_metadata>();
        let mut out = Vec::new();
        let mut off = 0usize;
        while off + meta_size <= buf.len() {
            // SAFETY: the kernel writes correctly-aligned `fanotify_event_metadata`
            // records back-to-back; `off` is advanced by each record's `event_len`.
            let meta = unsafe { &*(buf.as_ptr().add(off) as *const libc::fanotify_event_metadata) };
            let event_len = meta.event_len as usize;
            if meta.vers != libc::FANOTIFY_METADATA_VERSION || event_len < meta_size {
                break;
            }
            if meta.fd >= 0 {
                if let Ok(path) = std::fs::read_link(format!("/proc/self/fd/{}", meta.fd)) {
                    out.push(RawEvent::Modified(path.to_string_lossy().into_owned()));
                }
                // SAFETY: the kernel handed us this fd; we own and must close it.
                unsafe { libc::close(meta.fd) };
            }
            off += event_len;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fanotify needs root (CAP_SYS_ADMIN). Run as root to exercise it; otherwise
    /// the test self-skips (mirrors the env-gated live tests). Verified manually
    /// under sudo (see the PR notes).
    #[test]
    fn fanotify_reports_a_write_when_root() {
        // SAFETY: geteuid never fails.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skip fanotify_reports_a_write_when_root: not root");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // Even as root, an unprivileged container may lack CAP_SYS_ADMIN and the
        // fanotify mark fails with EPERM — skip rather than fail in that case.
        let w = match FanotifyWatcher::mark_dir(dir.path()) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("skip fanotify_reports_a_write_when_root: mark failed ({e})");
                return;
            }
        };
        let f = dir.path().join("note.txt");
        std::fs::write(&f, b"hello").unwrap(); // open+write+close -> FAN_CLOSE_WRITE
        let events = w.poll_raw(Duration::from_secs(2));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, RawEvent::Modified(p) if p.ends_with("note.txt"))),
            "expected a modify event for note.txt, got {events:?}"
        );
    }
}
