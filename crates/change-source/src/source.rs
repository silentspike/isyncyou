//! Common change-source abstraction + backend selection (issue #331).
//!
//! Both the unprivileged inotify accelerator ([`FsWatcher`]) and the privileged
//! mount-wide fanotify backend ([`crate::fanotify::FanotifyWatcher`]) emit the
//! same coalesced [`FsChange`]s, so consumers are backend-agnostic. The fanotify
//! backend is selected only when it is **opted in** (`change_source = "ebpf"` /
//! `"fanotify"`) AND the process is **privileged** AND fanotify actually
//! initializes; otherwise we fall back to inotify. Either way the periodic
//! [`crate::reconcile()`] diff stays the source of truth — an empty `poll` is
//! always a safe "reconcile fully" signal, so a missed or dropped event only
//! costs one extra (idempotent) reconcile.

use crate::watch::FsWatcher;
use crate::watcher::FsChange;
use isyncyou_core::config::ChangeSource as ChangeSourceKind;
use isyncyou_core::config::SyncConfig;
use std::path::Path;
use std::time::Duration;

#[cfg(target_os = "linux")]
use crate::fanotify::FanotifyWatcher;

/// A live change-detection accelerator. `poll` blocks up to `idle` for the first
/// change, batches a `debounce` window, and returns the coalesced [`FsChange`]s
/// (empty = nothing happened, or an overflow swallowed the batch → reconcile).
pub trait ChangeSource {
    fn poll(&mut self, idle: Duration, debounce: Duration) -> Vec<FsChange>;
}

impl ChangeSource for FsWatcher {
    fn poll(&mut self, idle: Duration, debounce: Duration) -> Vec<FsChange> {
        // The inherent method already coalesces; just forward.
        FsWatcher::poll(self, idle, debounce)
    }
}

#[cfg(target_os = "linux")]
impl ChangeSource for FanotifyWatcher {
    /// Mirror [`FsWatcher::poll`]: read raw fanotify events through the shared
    /// [`crate::watcher::Coalescer`] (which pairs moves and turns a queue overflow
    /// into a full-rescan signal), batching a `debounce` window after the first.
    fn poll(&mut self, idle: Duration, debounce: Duration) -> Vec<FsChange> {
        use crate::watcher::Coalescer;
        use std::time::Instant;

        let mut c = Coalescer::new();
        let first = self.poll_raw(idle);
        if first.is_empty() {
            return Vec::new();
        }
        for ev in first {
            c.push(ev);
        }
        let deadline = Instant::now() + debounce;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let more = self.poll_raw(remaining);
            if more.is_empty() {
                break;
            }
            for ev in more {
                c.push(ev);
            }
        }
        c.drain()
    }
}

/// The live accelerator a consumer holds. Static dispatch + clean `cfg`-gating
/// (no `dyn`); on non-Linux only the inotify variant exists.
pub enum Watcher {
    Inotify(FsWatcher),
    #[cfg(target_os = "linux")]
    Fanotify(FanotifyWatcher),
}

impl ChangeSource for Watcher {
    fn poll(&mut self, idle: Duration, debounce: Duration) -> Vec<FsChange> {
        match self {
            Watcher::Inotify(w) => w.poll(idle, debounce),
            #[cfg(target_os = "linux")]
            Watcher::Fanotify(w) => w.poll(idle, debounce),
        }
    }
}

/// The pure backend decision (the unit-testable AC-2 seam). No side effects, no
/// syscalls — drive it with booleans in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// No live watcher; rely on the periodic reconciler alone.
    ReconcileOnly,
    /// Unprivileged inotify accelerator.
    Inotify,
    /// Privileged mount-wide fanotify backend.
    Fanotify,
}

/// Pure policy: fanotify is chosen only for the opted-in `Ebpf` source when the
/// process is privileged AND a fanotify probe succeeded; otherwise inotify.
pub fn decide(cs: ChangeSourceKind, privileged: bool, fanotify_available: bool) -> Decision {
    match cs {
        ChangeSourceKind::ReconcileOnly => Decision::ReconcileOnly,
        ChangeSourceKind::Inotify => Decision::Inotify,
        ChangeSourceKind::Ebpf => {
            if privileged && fanotify_available {
                Decision::Fanotify
            } else {
                Decision::Inotify
            }
        }
    }
}

/// Whether this process can use fanotify (needs `CAP_SYS_ADMIN`). Root always
/// qualifies; a non-root daemon with ambient/file `CAP_SYS_ADMIN` is detected via
/// the effective capability set in `/proc/self/status`. The real proof is the
/// fanotify init itself (EPERM → fall back), so this is only a fast pre-filter.
#[cfg(target_os = "linux")]
fn is_privileged() -> bool {
    // SAFETY: geteuid never fails.
    if unsafe { libc::geteuid() } == 0 {
        return true;
    }
    has_cap_sys_admin_effective()
}

#[cfg(not(target_os = "linux"))]
fn is_privileged() -> bool {
    false
}

/// `CAP_SYS_ADMIN` (bit 21) set in this process's effective capability set, read
/// from the `CapEff:` line of `/proc/self/status` (dependency-free, no FFI).
#[cfg(target_os = "linux")]
fn has_cap_sys_admin_effective() -> bool {
    const CAP_SYS_ADMIN: u32 = 21;
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return false;
    };
    for line in status.lines() {
        if let Some(hex) = line.strip_prefix("CapEff:") {
            if let Ok(bits) = u64::from_str_radix(hex.trim(), 16) {
                return (bits >> CAP_SYS_ADMIN) & 1 == 1;
            }
        }
    }
    false
}

/// Build the live accelerator for `cfg`/`root`, or `None` for ReconcileOnly (or
/// when no watcher can start). Never fails: a fanotify error downgrades to
/// inotify, and an inotify error downgrades to reconcile-only.
pub fn select_change_source(cfg: &SyncConfig, root: &Path) -> Option<Watcher> {
    let privileged = is_privileged();

    // Probe fanotify only when it could win (opted in + privileged); a real init
    // attempt is the only honest availability signal (EPERM / old kernel / etc.).
    #[cfg(target_os = "linux")]
    {
        if cfg.change_source == ChangeSourceKind::Ebpf && privileged {
            match FanotifyWatcher::mark_dir(root) {
                Ok(w) => {
                    debug_assert_eq!(
                        decide(cfg.change_source, privileged, true),
                        Decision::Fanotify
                    );
                    return Some(Watcher::Fanotify(w));
                }
                Err(e) => {
                    eprintln!("change-source: fanotify unavailable ({e}); falling back to inotify");
                }
            }
        }
    }

    // No fanotify (not opted in / unprivileged / init failed): decide the rest.
    // `fanotify_available = false` here, so decide() never returns Fanotify; the
    // Fanotify arm is unreachable but handled as inotify for total safety.
    match decide(cfg.change_source, privileged, false) {
        Decision::ReconcileOnly => None,
        Decision::Inotify | Decision::Fanotify => match FsWatcher::start(root) {
            Ok(w) => Some(Watcher::Inotify(w)),
            Err(e) => {
                eprintln!("change-source: inotify start failed ({e}); reconcile-only");
                None
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isyncyou_core::config::ChangeSource as Cs;

    #[test]
    fn decide_reconcile_only_is_invariant() {
        for &privileged in &[false, true] {
            for &available in &[false, true] {
                assert_eq!(
                    decide(Cs::ReconcileOnly, privileged, available),
                    Decision::ReconcileOnly
                );
            }
        }
    }

    #[test]
    fn decide_inotify_is_invariant() {
        for &privileged in &[false, true] {
            for &available in &[false, true] {
                assert_eq!(
                    decide(Cs::Inotify, privileged, available),
                    Decision::Inotify
                );
            }
        }
    }

    #[test]
    fn decide_ebpf_needs_privilege_and_availability() {
        // Only privileged + available selects fanotify; every other corner falls
        // back to inotify (AC-2).
        assert_eq!(decide(Cs::Ebpf, false, false), Decision::Inotify);
        assert_eq!(decide(Cs::Ebpf, false, true), Decision::Inotify);
        assert_eq!(decide(Cs::Ebpf, true, false), Decision::Inotify);
        assert_eq!(decide(Cs::Ebpf, true, true), Decision::Fanotify);
    }
}
