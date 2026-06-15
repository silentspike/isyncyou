//! Privileged **fanotify** change-source — the mount-wide server backend
//! (issue #331, plan §5.2). The unprivileged desktop default is the inotify
//! [`crate::watch`] watcher.
//!
//! fanotify needs `CAP_SYS_ADMIN`. We initialize the group in **FID mode**
//! (`FAN_REPORT_DFID_NAME`) and mark the whole **filesystem**
//! (`FAN_MARK_FILESYSTEM`) containing the sync root, so a single mark reports
//! create / modify / move / delete for the entire tree without per-directory
//! watches and without inotify's `max_user_watches` ceiling. Directory-entry
//! events carry a parent-directory file handle plus the entry name (no open fd);
//! we resolve the path via `open_by_handle_at(2)` + `/proc/self/fd`.
//!
//! Raw events are folded through the shared [`crate::watcher::Coalescer`] by the
//! [`crate::source::ChangeSource`] impl (move pairing + queue-overflow handling
//! reused). Like inotify, this only lets the engine react quickly; the periodic
//! [`crate::reconcile()`] diff stays the source of truth, so a dropped or
//! unresolvable event merely costs one extra (idempotent) reconcile.

use crate::watcher::RawEvent;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// `fanotify_event_metadata` is a fixed 24-byte header; info-TLV records follow.
const META_SIZE: usize = 24;
const OFF_EVENT_LEN: usize = 0;
const OFF_VERS: usize = 4;
const OFF_META_LEN: usize = 6;
const OFF_MASK: usize = 8;
const OFF_FD: usize = 16;

/// A resolved directory-entry reference parsed from a `*_DFID_NAME` info record:
/// the parent directory's opaque file handle plus the entry's name.
struct Fid {
    handle_type: i32,
    handle: Vec<u8>,
    name: String,
}

/// A running fanotify watch over the filesystem containing the marked directory.
pub struct FanotifyWatcher {
    /// The fanotify group fd (events are read from here).
    fd: OwnedFd,
    /// A directory fd on the watched filesystem (opened `O_RDONLY`, not `O_PATH`),
    /// reused as the mount reference for `open_by_handle_at`.
    mount_fd: OwnedFd,
    /// Monotonic source of synthetic move cookies (fanotify FID events carry no
    /// inotify-style cookie). Globally unique so renames never mis-pair across
    /// poll reads in the same coalescer window.
    next_cookie: AtomicU32,
}

impl FanotifyWatcher {
    /// Initialize fanotify in FID mode and mark the **whole filesystem** that
    /// `dir` lives on for create/modify/move/delete (recursive by construction).
    /// Returns `PermissionDenied` without `CAP_SYS_ADMIN`, or another error on a
    /// kernel too old for FID mode / a filesystem without file-handle support —
    /// the selector treats any error as "fanotify unavailable" and falls back to
    /// inotify.
    pub fn mark_dir(dir: &Path) -> io::Result<Self> {
        // FID mode: directory-entry events report a dir handle + name instead of
        // an open fd, which is what lets create/delete/move be reported at all.
        // SAFETY: a syscall with constant flags; returns an fd or -1.
        let raw = unsafe {
            libc::fanotify_init(
                libc::FAN_CLASS_NOTIF
                    | libc::FAN_CLOEXEC
                    | libc::FAN_NONBLOCK
                    | libc::FAN_REPORT_DFID_NAME,
                libc::O_RDONLY as u32,
            )
        };
        if raw < 0 {
            // EPERM (no CAP_SYS_ADMIN) / EINVAL / ENOSYS (kernel < 5.9) → fall back.
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh, valid, owned fd from fanotify_init.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        let cpath = CString::new(dir.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;

        // A directory fd on the target fs, reused as the mount reference for
        // open_by_handle_at. It must be a *real* fd: an O_PATH fd makes
        // open_by_handle_at fail with EBADF (verified against the kernel), so we
        // open it O_RDONLY.
        // SAFETY: NUL-terminated path; returns an fd or -1.
        let mraw = unsafe {
            libc::open(
                cpath.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if mraw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fresh owned fd from open.
        let mount_fd = unsafe { OwnedFd::from_raw_fd(mraw) };

        // Mount-wide mark. FAN_ONDIR so directory create/delete/rename fire too.
        let base_mask = libc::FAN_CREATE
            | libc::FAN_DELETE
            | libc::FAN_MOVED_FROM
            | libc::FAN_MOVED_TO
            | libc::FAN_MODIFY
            | libc::FAN_ONDIR;
        // FAN_RENAME (single old+new event) needs kernel >= 5.17; if rejected we
        // drop it and rely on the separate FAN_MOVED_FROM/TO events instead.
        // SAFETY: valid group fd + NUL-terminated path.
        let rc = unsafe {
            libc::fanotify_mark(
                fd.as_raw_fd(),
                libc::FAN_MARK_ADD | libc::FAN_MARK_FILESYSTEM,
                base_mask | libc::FAN_RENAME,
                libc::AT_FDCWD,
                cpath.as_ptr(),
            )
        };
        if rc < 0 {
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINVAL) | Some(libc::EOPNOTSUPP) => {
                    // Older kernel: retry without FAN_RENAME.
                    // SAFETY: as above.
                    let rc2 = unsafe {
                        libc::fanotify_mark(
                            fd.as_raw_fd(),
                            libc::FAN_MARK_ADD | libc::FAN_MARK_FILESYSTEM,
                            base_mask,
                            libc::AT_FDCWD,
                            cpath.as_ptr(),
                        )
                    };
                    if rc2 < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                _ => return Err(err),
            }
        }

        Ok(FanotifyWatcher {
            fd,
            mount_fd,
            next_cookie: AtomicU32::new(1),
        })
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
        parse_events(
            &buf[..len as usize],
            |fid| self.resolve_handle(fid),
            || self.next_cookie.fetch_add(1, Ordering::Relaxed),
        )
    }

    /// Resolve a parsed directory-entry reference to an absolute path:
    /// `open_by_handle_at` the parent dir, read its `/proc/self/fd` link, join the
    /// entry name. Returns `None` if the object is already gone (ESTALE/ENOENT) or
    /// the handle can't be opened — the reconciler then catches it.
    fn resolve_handle(&self, fid: &Fid) -> Option<String> {
        let nbytes = fid.handle.len();
        // file_handle = { handle_bytes: u32, handle_type: i32, f_handle: [u8] }.
        // Over-allocate as u32 words for 4-byte alignment, then fill the byte view.
        let words = (8 + nbytes).div_ceil(4).max(2);
        let mut fh = vec![0u32; words];
        fh[0] = nbytes as u32;
        fh[1] = fid.handle_type as u32;
        // SAFETY: `fh` owns `words*4` bytes; we write only within that range.
        let bytes =
            unsafe { std::slice::from_raw_parts_mut(fh.as_mut_ptr() as *mut u8, words * 4) };
        bytes[8..8 + nbytes].copy_from_slice(&fid.handle);

        // SAFETY: `fh` is a correctly-laid-out, 4-byte-aligned file_handle; mount_fd
        // is a live fd on the same filesystem.
        let dirfd = unsafe {
            libc::open_by_handle_at(
                self.mount_fd.as_raw_fd(),
                fh.as_mut_ptr() as *mut libc::file_handle,
                libc::O_PATH,
            )
        };
        if dirfd < 0 {
            return None;
        }
        let link = std::fs::read_link(format!("/proc/self/fd/{dirfd}"));
        // SAFETY: we own `dirfd` from open_by_handle_at and must close it.
        unsafe { libc::close(dirfd) };
        let dir = link.ok()?;
        // An entry name of "." (or empty) means the marked dir itself.
        let full = if fid.name == "." || fid.name.is_empty() {
            dir
        } else {
            dir.join(&fid.name)
        };
        Some(full.to_string_lossy().into_owned())
    }
}

#[inline]
fn rd_u16(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2)
        .and_then(|s| s.try_into().ok())
        .map(u16::from_ne_bytes)
}
#[inline]
fn rd_u32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_ne_bytes)
}
#[inline]
fn rd_i32(b: &[u8], o: usize) -> Option<i32> {
    b.get(o..o + 4)
        .and_then(|s| s.try_into().ok())
        .map(i32::from_ne_bytes)
}
#[inline]
fn rd_u64(b: &[u8], o: usize) -> Option<u64> {
    b.get(o..o + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_ne_bytes)
}

/// Parse one info-TLV's `*_DFID_NAME` payload into a [`Fid`]. Layout after the
/// 4-byte info header: `fsid` (8 bytes), then a `file_handle`
/// (`handle_bytes: u32`, `handle_type: i32`, `f_handle: [u8; handle_bytes]`),
/// then the NUL-terminated entry name.
fn parse_fid_tlv(tlv: &[u8]) -> Option<Fid> {
    // 4 header + 8 fsid + 4 handle_bytes + 4 handle_type = 20 bytes minimum.
    if tlv.len() < 20 {
        return None;
    }
    let handle_bytes = rd_u32(tlv, 12)? as usize;
    let handle_type = rd_i32(tlv, 16)?;
    let h_start = 20usize;
    let h_end = h_start.checked_add(handle_bytes)?;
    if h_end > tlv.len() {
        return None;
    }
    let handle = tlv[h_start..h_end].to_vec();
    let name_region = &tlv[h_end..];
    let name_end = name_region
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(name_region.len());
    let name = String::from_utf8_lossy(&name_region[..name_end]).into_owned();
    Some(Fid {
        handle_type,
        handle,
        name,
    })
}

/// Walk a buffer of `fanotify_event_metadata` records (each followed by info
/// TLVs) into [`RawEvent`]s. `resolve` turns a parsed [`Fid`] into a path (a
/// syscall in the real backend, a stub in tests); `next_cookie` mints synthetic
/// move cookies. Pure with respect to those two injected effects, so the byte
/// arithmetic is unit-testable without privilege.
fn parse_events<R, C>(buf: &[u8], mut resolve: R, mut next_cookie: C) -> Vec<RawEvent>
where
    R: FnMut(&Fid) -> Option<String>,
    C: FnMut() -> u32,
{
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + META_SIZE <= buf.len() {
        let Some(event_len) = rd_u32(buf, off + OFF_EVENT_LEN).map(|v| v as usize) else {
            break;
        };
        let vers = buf[off + OFF_VERS];
        if vers != libc::FANOTIFY_METADATA_VERSION
            || event_len < META_SIZE
            || off + event_len > buf.len()
        {
            break;
        }
        let mask = rd_u64(buf, off + OFF_MASK).unwrap_or(0);
        let fd = rd_i32(buf, off + OFF_FD).unwrap_or(libc::FAN_NOFD);
        // FID mode delivers FAN_NOFD; close a stray fd if one ever slips through.
        if fd >= 0 {
            // SAFETY: a kernel-handed fd we own.
            unsafe { libc::close(fd) };
        }

        if mask & libc::FAN_Q_OVERFLOW != 0 {
            out.push(RawEvent::QueueOverflow);
            off += event_len;
            continue;
        }

        // Walk the info TLVs in [meta_len, event_len), collecting the records we use.
        let meta_len = rd_u16(buf, off + OFF_META_LEN).map_or(META_SIZE, |v| v as usize);
        let mut inner = off + meta_len.max(META_SIZE);
        let end = off + event_len;
        let mut dfid: Option<Fid> = None;
        let mut old_fid: Option<Fid> = None;
        let mut new_fid: Option<Fid> = None;
        while inner + 4 <= end {
            let info_type = buf[inner];
            let Some(tlv_len) = rd_u16(buf, inner + 2).map(|v| v as usize) else {
                break;
            };
            if tlv_len < 4 || inner + tlv_len > end {
                break;
            }
            let tlv = &buf[inner..inner + tlv_len];
            if info_type == libc::FAN_EVENT_INFO_TYPE_DFID_NAME {
                dfid = parse_fid_tlv(tlv);
            } else if info_type == libc::FAN_EVENT_INFO_TYPE_OLD_DFID_NAME {
                old_fid = parse_fid_tlv(tlv);
            } else if info_type == libc::FAN_EVENT_INFO_TYPE_NEW_DFID_NAME {
                new_fid = parse_fid_tlv(tlv);
            }
            inner += tlv_len;
        }

        // Classify by mask. The kernel can coalesce several ops on one object into
        // a single record (e.g. DELETE|MOVED_TO when a file is renamed-in then
        // removed within the window), so order the checks by the net end-state at
        // the reported path: an explicit rename first, then deletion, then a move
        // out/in, then create, then modify. The reconciler stays authoritative, so
        // even a sub-optimal pick on a coalesced mask only costs one extra rescan.
        if mask & libc::FAN_RENAME != 0 {
            // A single rename event carries both OLD and NEW dir-fid+name.
            let old = old_fid.as_ref().and_then(&mut resolve);
            let new = new_fid.as_ref().and_then(&mut resolve);
            match (old, new) {
                (Some(o), Some(n)) => {
                    // Same synthetic cookie → the Coalescer pairs them into Renamed.
                    let cookie = next_cookie();
                    out.push(RawEvent::MovedFrom { cookie, path: o });
                    out.push(RawEvent::MovedTo { cookie, path: n });
                }
                (Some(o), None) => out.push(RawEvent::MovedFrom {
                    cookie: next_cookie(),
                    path: o,
                }),
                (None, Some(n)) => out.push(RawEvent::MovedTo {
                    cookie: next_cookie(),
                    path: n,
                }),
                (None, None) => {}
            }
        } else if mask & libc::FAN_DELETE != 0 {
            if let Some(p) = dfid.as_ref().and_then(&mut resolve) {
                out.push(RawEvent::Deleted(p));
            }
        } else if mask & libc::FAN_MOVED_FROM != 0 {
            if let Some(p) = dfid.as_ref().and_then(&mut resolve) {
                // Fresh (unmatched) cookie → Coalescer degrades it to a Delete
                // (parity with the inotify backend's rename→delete+create).
                out.push(RawEvent::MovedFrom {
                    cookie: next_cookie(),
                    path: p,
                });
            }
        } else if mask & libc::FAN_MOVED_TO != 0 {
            if let Some(p) = dfid.as_ref().and_then(&mut resolve) {
                out.push(RawEvent::MovedTo {
                    cookie: next_cookie(),
                    path: p,
                });
            }
        } else if mask & libc::FAN_CREATE != 0 {
            if let Some(p) = dfid.as_ref().and_then(&mut resolve) {
                out.push(RawEvent::Created(p));
            }
        } else if mask & libc::FAN_MODIFY != 0 {
            if let Some(p) = dfid.as_ref().and_then(&mut resolve) {
                out.push(RawEvent::Modified(p));
            }
        }

        off += event_len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watcher::{Coalescer, FsChange};

    /// Build one `*_DFID_NAME` info TLV (info_type 2 / 10 / 12).
    fn fid_tlv(info_type: u8, handle: &[u8], name: &str) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 8]); // fsid
        body.extend_from_slice(&(handle.len() as u32).to_ne_bytes()); // handle_bytes
        body.extend_from_slice(&1i32.to_ne_bytes()); // handle_type
        body.extend_from_slice(handle); // f_handle
        body.extend_from_slice(name.as_bytes());
        body.push(0); // NUL
        let len = (4 + body.len()) as u16; // whole TLV incl. 4-byte header
        let mut tlv = vec![info_type, 0];
        tlv.extend_from_slice(&len.to_ne_bytes());
        tlv.extend_from_slice(&body);
        tlv
    }

    /// Build one event record: 24-byte metadata (vers=3, fd=FAN_NOFD) + TLVs.
    fn record(mask: u64, tlvs: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = tlvs.iter().flatten().copied().collect();
        let event_len = (META_SIZE + body.len()) as u32;
        let mut r = Vec::new();
        r.extend_from_slice(&event_len.to_ne_bytes()); // event_len
        r.push(libc::FANOTIFY_METADATA_VERSION); // vers
        r.push(0); // reserved
        r.extend_from_slice(&(META_SIZE as u16).to_ne_bytes()); // metadata_len
        r.extend_from_slice(&mask.to_ne_bytes()); // mask
        r.extend_from_slice(&libc::FAN_NOFD.to_ne_bytes()); // fd = -1
        r.extend_from_slice(&0i32.to_ne_bytes()); // pid
        r.extend_from_slice(&body);
        r
    }

    /// Stub resolver: the path is just `/root/<name>` (the handle is ignored).
    fn stub(fid: &Fid) -> Option<String> {
        Some(format!("/root/{}", fid.name))
    }

    fn parse(buf: &[u8]) -> Vec<RawEvent> {
        let mut c = 0u32;
        parse_events(buf, stub, || {
            c += 1;
            c
        })
    }

    const H: &[u8] = &[0xde, 0xad, 0xbe, 0xef];

    #[test]
    fn parse_modify() {
        let buf = record(
            libc::FAN_MODIFY,
            &[fid_tlv(libc::FAN_EVENT_INFO_TYPE_DFID_NAME, H, "note.txt")],
        );
        assert_eq!(
            parse(&buf),
            vec![RawEvent::Modified("/root/note.txt".into())]
        );
    }

    #[test]
    fn parse_create_and_delete() {
        let create = record(
            libc::FAN_CREATE | libc::FAN_ONDIR,
            &[fid_tlv(libc::FAN_EVENT_INFO_TYPE_DFID_NAME, H, "new")],
        );
        assert_eq!(parse(&create), vec![RawEvent::Created("/root/new".into())]);
        let delete = record(
            libc::FAN_DELETE,
            &[fid_tlv(libc::FAN_EVENT_INFO_TYPE_DFID_NAME, H, "old")],
        );
        assert_eq!(parse(&delete), vec![RawEvent::Deleted("/root/old".into())]);
    }

    #[test]
    fn parse_overflow_skips_tlvs() {
        // An overflow record carries no usable fid; we emit QueueOverflow only.
        let buf = record(libc::FAN_Q_OVERFLOW, &[]);
        assert_eq!(parse(&buf), vec![RawEvent::QueueOverflow]);
    }

    #[test]
    fn parse_rename_pairs_with_shared_cookie() {
        let buf = record(
            libc::FAN_RENAME,
            &[
                fid_tlv(libc::FAN_EVENT_INFO_TYPE_OLD_DFID_NAME, H, "a"),
                fid_tlv(libc::FAN_EVENT_INFO_TYPE_NEW_DFID_NAME, H, "b"),
            ],
        );
        let evs = parse(&buf);
        // MovedFrom + MovedTo carry the SAME cookie...
        match (&evs[0], &evs[1]) {
            (
                RawEvent::MovedFrom {
                    cookie: c1,
                    path: from,
                },
                RawEvent::MovedTo {
                    cookie: c2,
                    path: to,
                },
            ) => {
                assert_eq!(c1, c2);
                assert_eq!(from, "/root/a");
                assert_eq!(to, "/root/b");
            }
            other => panic!("expected a paired move, got {other:?}"),
        }
        // ...so the real Coalescer folds them into a single Renamed.
        let mut coal = Coalescer::new();
        for e in evs {
            coal.push(e);
        }
        assert_eq!(
            coal.drain(),
            vec![FsChange::Renamed {
                from: "/root/a".into(),
                to: "/root/b".into()
            }]
        );
    }

    #[test]
    fn parse_unpaired_move_degrades_to_delete() {
        // A lone FAN_MOVED_FROM (no matching MOVED_TO) gets a fresh cookie, so the
        // Coalescer degrades it to a Delete (parity with the inotify backend).
        let buf = record(
            libc::FAN_MOVED_FROM,
            &[fid_tlv(libc::FAN_EVENT_INFO_TYPE_DFID_NAME, H, "gone")],
        );
        let evs = parse(&buf);
        assert!(matches!(evs[0], RawEvent::MovedFrom { .. }));
        let mut coal = Coalescer::new();
        for e in evs {
            coal.push(e);
        }
        assert_eq!(coal.drain(), vec![FsChange::Deleted("/root/gone".into())]);
    }

    #[test]
    fn parse_two_records_back_to_back() {
        let mut buf = record(
            libc::FAN_CREATE,
            &[fid_tlv(libc::FAN_EVENT_INFO_TYPE_DFID_NAME, H, "one")],
        );
        buf.extend(record(
            libc::FAN_DELETE,
            &[fid_tlv(libc::FAN_EVENT_INFO_TYPE_DFID_NAME, H, "two")],
        ));
        assert_eq!(
            parse(&buf),
            vec![
                RawEvent::Created("/root/one".into()),
                RawEvent::Deleted("/root/two".into()),
            ]
        );
    }

    #[test]
    fn parse_truncated_buffer_stops_cleanly() {
        // event_len claims more bytes than the buffer holds → stop, no panic.
        let mut buf = record(
            libc::FAN_CREATE,
            &[fid_tlv(libc::FAN_EVENT_INFO_TYPE_DFID_NAME, H, "x")],
        );
        buf.truncate(buf.len() - 5);
        assert!(parse(&buf).is_empty());
    }

    #[test]
    fn parse_bad_version_breaks() {
        let mut buf = record(
            libc::FAN_CREATE,
            &[fid_tlv(libc::FAN_EVENT_INFO_TYPE_DFID_NAME, H, "x")],
        );
        buf[OFF_VERS] = 99; // wrong metadata version
        assert!(parse(&buf).is_empty());
    }

    /// fanotify needs root (`CAP_SYS_ADMIN`). Run as root to exercise the live
    /// path; otherwise self-skip (mirrors the env-gated live tests). The rigorous
    /// parser coverage is the non-privileged tests above; this confirms the kernel
    /// actually delivers the four kinds mount-wide.
    #[test]
    fn fanotify_reports_crud_when_root() {
        // SAFETY: geteuid never fails.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skip fanotify_reports_crud_when_root: not root");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // Even as root, a container may lack CAP_SYS_ADMIN or the fs may not export
        // file handles — skip rather than fail.
        let w = match FanotifyWatcher::mark_dir(dir.path()) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("skip fanotify_reports_crud_when_root: mark failed ({e})");
                return;
            }
        };
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, b"hello").unwrap(); // create + modify
        std::fs::rename(&a, &b).unwrap(); // move a -> b
        std::fs::remove_file(&b).unwrap(); // delete

        // Drain a couple of windows (a filesystem mark is noisy; collect ours).
        let mut events = Vec::new();
        for _ in 0..4 {
            events.extend(w.poll_raw(Duration::from_millis(500)));
        }
        let mentions = |needle: &str| {
            events.iter().any(|e| match e {
                RawEvent::Created(p)
                | RawEvent::Modified(p)
                | RawEvent::Deleted(p)
                | RawEvent::MovedFrom { path: p, .. }
                | RawEvent::MovedTo { path: p, .. } => p.ends_with(needle),
                RawEvent::QueueOverflow => false,
            })
        };
        assert!(
            mentions("a.txt") && mentions("b.txt"),
            "expected events for a.txt and b.txt, got {events:?}"
        );
    }
}
