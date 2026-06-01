//! `isyncyou-core` — engine core logic (sync-state machine, conflict engine,
//! guards, recovery, scheduling). Pure, deterministic building blocks layered on
//! top of `store`, `pathmap` and `graph`.
//!
//! This module currently provides the per-item [`sync_state`] automaton.

pub mod sync_state;

pub use sync_state::{SyncEvent, SyncState};
