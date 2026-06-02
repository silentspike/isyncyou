//! `isyncyou-change-source` — pluggable change detection.
//!
//! Filesystem events (inotify on the desktop, eBPF/fanotify on the server) are an
//! accelerator; the [`reconcile`] periodic diff is the source of truth. This crate
//! currently provides that authoritative reconciler; the event watchers are added
//! behind the same interface.

pub mod reconcile;
pub mod watcher;

pub use reconcile::{reconcile, Entry, ReconcileChange};
pub use watcher::{Coalescer, FsChange, RawEvent};
