//! `isyncyou-core` — engine core logic (sync-state machine, conflict engine,
//! guards, recovery, scheduling). Pure, deterministic building blocks layered on
//! top of `store`, `pathmap` and `graph`.
//!
//! Currently: the per-item [`sync_state`] automaton and the [`conflict`] engine.

pub mod config;
pub mod conflict;
pub mod guard;
pub mod recovery;
pub mod sync_state;

pub use config::{AccountConfig, ChangeSource, Config, DeleteGuardConfig, SyncConfig};
pub use conflict::{
    classify, compare_versions, conflict_copy_name, resolve, Change, ConflictKind, ConflictPolicy,
    Resolution, Versus,
};
pub use guard::{DeleteGuard, Direction, GuardVerdict};
pub use recovery::{atomic_write, HealthStatus, Journal, JournalEntry, SelfCheck};
pub use sync_state::{SyncEvent, SyncState};
