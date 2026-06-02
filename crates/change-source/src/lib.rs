//! `isyncyou-change-source` — pluggable change detection.
//!
//! Filesystem events (inotify on the desktop, eBPF/fanotify on the server) are an
//! accelerator; the [`reconcile`] periodic diff is the source of truth. The
//! [`watch`] module is the live inotify (etc.) accelerator that feeds the
//! coalescer behind that same interface.

pub mod reconcile;
pub mod watch;
pub mod watcher;

pub use reconcile::{reconcile, Entry, ReconcileChange};
pub use watch::FsWatcher;
pub use watcher::{Coalescer, FsChange, RawEvent};
