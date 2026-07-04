//! Per-folder OneDrive sync **mode** + the pure **scope-ownership** rule (S-OM.7).
//!
//! OneDrive on mobile runs three per-folder modes (online / sync / offline). Modes
//! 2 & 3 sync only the configured folders via a per-folder `driveItem` delta. Because
//! `/me/drive/items/{id}/delta` is **recursive over the whole subtree**, configuring
//! a parent *and* a child folder as separate scopes overlaps — the parent delta
//! reports the child's items too. [`owning_scope`] resolves that overlap: the
//! **deepest** active ancestor owns an item, so each item belongs to exactly one
//! active scope.
//!
//! This is a connectors-local, in-memory runtime type (**no serde**). The config side
//! (S-OM.4 / #650) owns the persisted `OneDriveMode`; the eventual unification is a
//! `From<core::OneDriveMode>` bridge with matching variants. In S-OM.7 itself the enum
//! is nearly vestigial — [`owning_scope`] is **mode-agnostic** (it works on folder ids
//! alone); the mode only matters where the sync driver branches on sync-vs-offline
//! (mostly S-OM.9 / #655).

use isyncyou_core::{OneDriveMode, OneDriveModes};
use std::collections::BTreeSet;

/// Per-folder OneDrive sync mode (runtime mirror of the future `core::OneDriveMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Live listing, nothing stored (account default).
    Online,
    /// Metadata cached via a scoped delta, bodies lazy.
    Sync,
    /// Fully materialized, editable, bidirectional.
    Offline,
}

/// A configured scope root: a folder id plus its explicit mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderScope {
    pub folder_id: String,
    pub mode: Mode,
}

/// The deepest active scope that owns an item, or `None` if no ancestor is active.
///
/// `ancestry` is the item's own id first, then its parent, grandparent, … toward the
/// drive root (**deepest first**). `active_scopes` are the configured scope-root
/// folder ids. Walking `ancestry` front-to-back, the **first** id found in
/// `active_scopes` is the deepest active ancestor — that scope owns the item. A scope
/// root owns itself (its own id leads its ancestry).
pub fn owning_scope<'a>(ancestry: &[&str], active_scopes: &BTreeSet<&'a str>) -> Option<&'a str> {
    ancestry
        .iter()
        .find_map(|id| active_scopes.get(*id).copied())
}

/// Bridge the persisted per-folder mode (config side, S-OM.4 / #650) to the
/// connectors-local runtime [`Mode`]. The variants match one-to-one; this is the
/// unification the module doc anticipates.
impl From<OneDriveMode> for Mode {
    fn from(m: OneDriveMode) -> Self {
        match m {
            OneDriveMode::Online => Mode::Online,
            OneDriveMode::Sync => Mode::Sync,
            OneDriveMode::Offline => Mode::Offline,
        }
    }
}

/// Build the connectors [`FolderScope`] list from an account's persisted OneDrive mode
/// map (#650). Only folders with an explicit **non-`Online`** mode become scopes:
/// `Sync` and `Offline` folders are synced via a per-folder delta, whereas `Online`
/// folders stay live-only (no stored scope). The account `default_mode` is not itself a
/// scope root — S-OM.9 offline sync operates on the explicitly-configured folders; a
/// whole-drive `default_mode` other than `Online` remains the desktop `sync_once`
/// concern, not the mobile scoped offline pass.
pub fn scopes_from_modes(modes: Option<&OneDriveModes>) -> Vec<FolderScope> {
    let mut out = Vec::new();
    if let Some(m) = modes {
        for (folder_id, mode) in &m.folder_modes {
            if *mode == OneDriveMode::Online {
                continue; // live-only: nothing to sync/materialize
            }
            out.push(FolderScope {
                folder_id: folder_id.clone(),
                mode: (*mode).into(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set<'a>(ids: &[&'a str]) -> BTreeSet<&'a str> {
        ids.iter().copied().collect()
    }

    #[test]
    fn from_onedrive_mode_bridges_each_variant() {
        assert_eq!(Mode::from(OneDriveMode::Online), Mode::Online);
        assert_eq!(Mode::from(OneDriveMode::Sync), Mode::Sync);
        assert_eq!(Mode::from(OneDriveMode::Offline), Mode::Offline);
    }

    #[test]
    fn scopes_from_modes_keeps_non_online_folders_only() {
        let mut modes = OneDriveModes::default(); // default_mode = Online
        modes
            .folder_modes
            .insert("F_off".to_string(), OneDriveMode::Offline);
        modes
            .folder_modes
            .insert("F_sync".to_string(), OneDriveMode::Sync);
        modes
            .folder_modes
            .insert("F_on".to_string(), OneDriveMode::Online);
        let scopes = scopes_from_modes(Some(&modes));
        // Online folder is excluded; Sync + Offline become scopes with bridged modes.
        assert_eq!(scopes.len(), 2);
        assert!(scopes
            .iter()
            .any(|s| s.folder_id == "F_off" && s.mode == Mode::Offline));
        assert!(scopes
            .iter()
            .any(|s| s.folder_id == "F_sync" && s.mode == Mode::Sync));
        assert!(scopes.iter().all(|s| s.folder_id != "F_on"));
        // None (no configured modes) yields no scopes.
        assert!(scopes_from_modes(None).is_empty());
    }

    #[test]
    fn deepest_active_ancestor_wins_on_overlap() {
        // Parent P (sync) and child C (offline) both active; file Y lives under C.
        // ancestry of Y: [Y, C, P, root]. Deepest active ancestor is C.
        let active = set(&["P", "C"]);
        assert_eq!(owning_scope(&["Y", "C", "P", "root"], &active), Some("C"));
    }

    #[test]
    fn scope_root_owns_itself() {
        let active = set(&["P", "C"]);
        // The child scope root C resolves to itself, not to the parent P.
        assert_eq!(owning_scope(&["C", "P", "root"], &active), Some("C"));
        // The parent scope root P resolves to itself.
        assert_eq!(owning_scope(&["P", "root"], &active), Some("P"));
    }

    #[test]
    fn deeply_nested_item_resolves_to_its_single_active_root() {
        // Only S is active; a file several levels below it still resolves to S.
        let active = set(&["S"]);
        assert_eq!(
            owning_scope(&["file", "sub2", "sub1", "S", "root"], &active),
            Some("S")
        );
    }

    #[test]
    fn no_active_ancestor_returns_none() {
        let active = set(&["P", "C"]);
        assert_eq!(owning_scope(&["Y", "U", "root"], &active), None);
        // Empty active set never owns anything.
        assert_eq!(owning_scope(&["Y", "U"], &set(&[])), None);
    }

    #[test]
    fn disjoint_scopes_each_own_their_own_subtree() {
        let active = set(&["A", "B"]);
        assert_eq!(owning_scope(&["a1", "A", "root"], &active), Some("A"));
        assert_eq!(owning_scope(&["b1", "B", "root"], &active), Some("B"));
    }
}
