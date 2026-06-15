//! `isyncyou-change-source` — pluggable change detection.
//!
//! Filesystem events (inotify on the desktop, eBPF/fanotify on the server) are an
//! accelerator; the [`reconcile()`] periodic diff is the source of truth. The
//! [`watch`] module is the live inotify (etc.) accelerator that feeds the
//! coalescer behind that same interface.

#[cfg(target_os = "linux")]
pub mod fanotify;
pub mod reconcile;
pub mod source;
pub mod watch;
pub mod watcher;

#[cfg(target_os = "linux")]
pub use fanotify::FanotifyWatcher;
pub use reconcile::{reconcile, Entry, ReconcileChange};
pub use source::{decide, select_change_source, ChangeSource, Decision, Watcher};
pub use watch::FsWatcher;
pub use watcher::{Coalescer, FsChange, RawEvent};
