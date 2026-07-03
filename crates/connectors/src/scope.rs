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

#[cfg(test)]
mod tests {
    use super::*;

    fn set<'a>(ids: &[&'a str]) -> BTreeSet<&'a str> {
        ids.iter().copied().collect()
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
