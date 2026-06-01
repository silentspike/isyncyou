//! Per-item sync-state automaton.
//!
//! Each tracked item carries a [`SyncState`]. Detected changes and operation
//! outcomes are fed in as [`SyncEvent`]s; [`SyncState::on`] returns the next
//! state. The function is **total** — any (state, event) pair that has no
//! meaningful transition is a no-op (the state is returned unchanged) — so the
//! engine never panics on an unexpected event ordering.
//!
//! States mirror the plan (§5.1). The string forms ([`SyncState::as_str`] /
//! [`SyncState::parse`]) are what the `store` persists in the `sync_state` column.

use serde::{Deserialize, Serialize};

/// The state of a single item in the sync pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    /// In sync; nothing to do.
    Clean,
    /// Local copy changed; needs upload.
    LocalDirty,
    /// Remote copy changed; needs download.
    RemoteDirty,
    /// Both sides changed since last sync.
    BothDirty,
    /// Needs conflict resolution (e.g. keep-both copy).
    Conflict,
    /// Locally removed; deletion must be propagated to the cloud.
    DeletePending,
    /// Removed on the cloud; local copy must be moved to trash.
    TrashPending,
    /// Upload in progress / staged.
    UploadStaged,
    /// Download in progress / staged.
    DownloadStaged,
    /// A retryable error occurred; will be retried.
    ErrorRetryable,
    /// A fatal error occurred; needs operator/recovery action.
    ErrorFatal,
}

/// Inputs that drive the automaton.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncEvent {
    LocalModified,
    RemoteModified,
    LocalRemoved,
    RemoteRemoved,
    UploadStarted,
    UploadFinished,
    DownloadStarted,
    DownloadFinished,
    /// A pending deletion/trash operation was applied.
    DeleteFinished,
    /// The conflict engine resolved the conflict.
    ConflictResolved,
    RetryableError,
    FatalError,
    /// A retryable/fatal error was cleared; re-scan will re-derive the state.
    Recovered,
}

impl SyncState {
    /// Apply an event, returning the next state. Total (unhandled pairs are no-ops).
    pub fn on(self, event: SyncEvent) -> SyncState {
        use SyncEvent::*;
        use SyncState::*;
        match (self, event) {
            // ---- from Clean ----
            (Clean, LocalModified) => LocalDirty,
            (Clean, RemoteModified) => RemoteDirty,
            (Clean, LocalRemoved) => DeletePending,
            (Clean, RemoteRemoved) => TrashPending,

            // ---- LocalDirty ----
            (LocalDirty, RemoteModified) => BothDirty,
            (LocalDirty, UploadStarted) => UploadStaged,
            (LocalDirty, LocalRemoved) => DeletePending,
            (LocalDirty, RemoteRemoved) => Conflict, // local edit vs remote delete
            (LocalDirty, RetryableError) => ErrorRetryable,
            (LocalDirty, FatalError) => ErrorFatal,

            // ---- RemoteDirty ----
            (RemoteDirty, LocalModified) => BothDirty,
            (RemoteDirty, DownloadStarted) => DownloadStaged,
            (RemoteDirty, RemoteRemoved) => TrashPending,
            (RemoteDirty, LocalRemoved) => Conflict, // remote edit vs local delete
            (RemoteDirty, RetryableError) => ErrorRetryable,
            (RemoteDirty, FatalError) => ErrorFatal,

            // ---- BothDirty ----
            (BothDirty, ConflictResolved) => Clean,
            (BothDirty, LocalRemoved) => Conflict,
            (BothDirty, RemoteRemoved) => Conflict,
            (BothDirty, RetryableError) => ErrorRetryable,
            (BothDirty, FatalError) => ErrorFatal,

            // ---- UploadStaged ----
            (UploadStaged, UploadFinished) => Clean,
            (UploadStaged, LocalModified) => LocalDirty, // changed mid-upload -> stale
            (UploadStaged, RemoteModified) => BothDirty,
            (UploadStaged, RetryableError) => ErrorRetryable,
            (UploadStaged, FatalError) => ErrorFatal,

            // ---- DownloadStaged ----
            (DownloadStaged, DownloadFinished) => Clean,
            (DownloadStaged, RemoteModified) => RemoteDirty, // changed mid-download -> redo
            (DownloadStaged, LocalModified) => BothDirty,
            (DownloadStaged, RetryableError) => ErrorRetryable,
            (DownloadStaged, FatalError) => ErrorFatal,

            // ---- DeletePending ----
            (DeletePending, DeleteFinished) => Clean,
            (DeletePending, RemoteModified) => Conflict, // delete vs remote edit
            (DeletePending, RetryableError) => ErrorRetryable,
            (DeletePending, FatalError) => ErrorFatal,

            // ---- TrashPending ----
            (TrashPending, DeleteFinished) => Clean,
            (TrashPending, LocalModified) => Conflict, // remote delete vs local edit
            (TrashPending, RetryableError) => ErrorRetryable,
            (TrashPending, FatalError) => ErrorFatal,

            // ---- Conflict ----
            (Conflict, ConflictResolved) => Clean,
            (Conflict, FatalError) => ErrorFatal,

            // ---- ErrorRetryable ----
            (ErrorRetryable, Recovered) => Clean,
            (ErrorRetryable, FatalError) => ErrorFatal,

            // ---- ErrorFatal ----
            (ErrorFatal, Recovered) => Clean,

            // anything else: no-op
            (s, _) => s,
        }
    }

    /// Stable string used by the store's `sync_state` column.
    pub fn as_str(self) -> &'static str {
        use SyncState::*;
        match self {
            Clean => "clean",
            LocalDirty => "local_dirty",
            RemoteDirty => "remote_dirty",
            BothDirty => "both_dirty",
            Conflict => "conflict",
            DeletePending => "delete_pending",
            TrashPending => "trash_pending",
            UploadStaged => "upload_staged",
            DownloadStaged => "download_staged",
            ErrorRetryable => "error_retryable",
            ErrorFatal => "error_fatal",
        }
    }

    /// Parse a stored string back into a [`SyncState`].
    pub fn parse(s: &str) -> Option<SyncState> {
        use SyncState::*;
        Some(match s {
            "clean" => Clean,
            "local_dirty" => LocalDirty,
            "remote_dirty" => RemoteDirty,
            "both_dirty" => BothDirty,
            "conflict" => Conflict,
            "delete_pending" => DeletePending,
            "trash_pending" => TrashPending,
            "upload_staged" => UploadStaged,
            "download_staged" => DownloadStaged,
            "error_retryable" => ErrorRetryable,
            "error_fatal" => ErrorFatal,
            _ => return None,
        })
    }

    /// Whether the item needs work (anything that is not `Clean`).
    pub fn is_pending(self) -> bool {
        self != SyncState::Clean
    }

    /// Whether the item is blocked on an error.
    pub fn is_error(self) -> bool {
        matches!(self, SyncState::ErrorRetryable | SyncState::ErrorFatal)
    }
}

#[cfg(test)]
mod tests {
    use super::SyncEvent::*;
    use super::SyncState::*;
    use super::*;

    /// Apply a sequence of events from a start state.
    fn run(start: SyncState, events: &[SyncEvent]) -> SyncState {
        events.iter().fold(start, |s, &e| s.on(e))
    }

    #[test]
    fn happy_upload_path() {
        assert_eq!(
            run(Clean, &[LocalModified, UploadStarted, UploadFinished]),
            Clean
        );
    }

    #[test]
    fn happy_download_path() {
        assert_eq!(
            run(Clean, &[RemoteModified, DownloadStarted, DownloadFinished]),
            Clean
        );
    }

    #[test]
    fn concurrent_edits_become_both_dirty_then_resolve() {
        assert_eq!(Clean.on(LocalModified).on(RemoteModified), BothDirty);
        assert_eq!(Clean.on(RemoteModified).on(LocalModified), BothDirty);
        assert_eq!(BothDirty.on(ConflictResolved), Clean);
    }

    #[test]
    fn local_delete_propagates() {
        assert_eq!(run(Clean, &[LocalRemoved, DeleteFinished]), Clean);
        assert_eq!(Clean.on(LocalRemoved), DeletePending);
    }

    #[test]
    fn remote_delete_trashes_local() {
        assert_eq!(Clean.on(RemoteRemoved), TrashPending);
        assert_eq!(run(Clean, &[RemoteRemoved, DeleteFinished]), Clean);
    }

    #[test]
    fn edit_delete_conflicts() {
        assert_eq!(Clean.on(LocalModified).on(RemoteRemoved), Conflict);
        assert_eq!(Clean.on(RemoteModified).on(LocalRemoved), Conflict);
        assert_eq!(Clean.on(LocalRemoved).on(RemoteModified), Conflict);
        assert_eq!(Clean.on(RemoteRemoved).on(LocalModified), Conflict);
    }

    #[test]
    fn change_mid_transfer_invalidates() {
        assert_eq!(UploadStaged.on(LocalModified), LocalDirty);
        assert_eq!(UploadStaged.on(RemoteModified), BothDirty);
        assert_eq!(DownloadStaged.on(RemoteModified), RemoteDirty);
        assert_eq!(DownloadStaged.on(LocalModified), BothDirty);
    }

    #[test]
    fn error_recovery_cycle() {
        assert_eq!(LocalDirty.on(RetryableError), ErrorRetryable);
        assert_eq!(ErrorRetryable.on(Recovered), Clean);
        assert_eq!(ErrorRetryable.on(FatalError), ErrorFatal);
        assert_eq!(ErrorFatal.on(Recovered), Clean);
    }

    #[test]
    fn unknown_transitions_are_noops() {
        assert_eq!(Clean.on(UploadFinished), Clean);
        assert_eq!(Clean.on(ConflictResolved), Clean);
        assert_eq!(Conflict.on(LocalModified), Conflict);
    }

    #[test]
    fn helpers() {
        assert!(!Clean.is_pending());
        assert!(LocalDirty.is_pending());
        assert!(ErrorFatal.is_error());
        assert!(!Conflict.is_error());
    }

    #[test]
    fn string_roundtrip_for_all_states() {
        for s in [
            Clean,
            LocalDirty,
            RemoteDirty,
            BothDirty,
            Conflict,
            DeletePending,
            TrashPending,
            UploadStaged,
            DownloadStaged,
            ErrorRetryable,
            ErrorFatal,
        ] {
            assert_eq!(SyncState::parse(s.as_str()), Some(s), "roundtrip {s:?}");
        }
        assert_eq!(SyncState::parse("nonsense"), None);
    }

    #[test]
    fn serde_uses_snake_case_matching_as_str() {
        let json = serde_json::to_string(&LocalDirty).unwrap();
        assert_eq!(json, "\"local_dirty\"");
        let back: SyncState = serde_json::from_str("\"both_dirty\"").unwrap();
        assert_eq!(back, BothDirty);
    }
}
