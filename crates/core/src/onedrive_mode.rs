//! OneDrive per-folder mode policy (#onedrive-mobile / #650) — a pure, deterministic
//! resolution of the three modes (online / sync / offline) with folder inheritance.
//!
//! This is a *distinct axis* from the per-item `content_state`
//! (`online | cached | materialized | not_applicable`, only the `online` string
//! overlaps): a [`OneDriveMode`] is the folder-level **intent** that drives which root
//! and content-state an item lands in. Resolution is pure — no I/O, no store. The caller
//! (webui #651) assembles the item's ancestor chain (mirroring the parent-id walk in
//! `fuse::PlaceholderIndex::from_items`) and this module resolves it against the account's
//! [`OneDriveModes`].

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

/// The three OneDrive per-folder modes. [`OneDriveMode::Online`] is the account default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OneDriveMode {
    /// Live listing, nothing stored (default).
    #[default]
    Online,
    /// Metadata cached, lazy bodies.
    Sync,
    /// Fully materialized, bidirectional.
    Offline,
}

impl OneDriveMode {
    /// True for the account-default mode — used to keep `default_mode = "online"` out of
    /// serialized TOML (mirrors the config file's skip-when-default convention).
    pub fn is_default(&self) -> bool {
        matches!(self, OneDriveMode::Online)
    }

    /// Stable machine tag, kept identical to the serde string form (for API / UI copy).
    pub fn as_str(self) -> &'static str {
        match self {
            OneDriveMode::Online => "online",
            OneDriveMode::Sync => "sync",
            OneDriveMode::Offline => "offline",
        }
    }
}

/// True for an empty override map — keeps an empty `folder_modes` out of serialized TOML
/// (mirrors the `path_is_empty` skip idiom in `config.rs`).
fn folder_modes_is_empty(m: &BTreeMap<String, OneDriveMode>) -> bool {
    m.is_empty()
}

/// One account's OneDrive mode policy: a fallback default plus explicit per-folder overrides.
///
/// Serialized under `[onedrive_modes.<account_id>]` in the config. `default_mode` is a scalar
/// and is declared **before** the `folder_modes` sub-table so the emitted TOML never places a
/// value after a table (which the `toml` serializer rejects).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct OneDriveModes {
    /// Fallback mode for folders with no explicit ancestor entry.
    #[serde(skip_serializing_if = "OneDriveMode::is_default")]
    pub default_mode: OneDriveMode,
    /// Explicit per-folder overrides, keyed by OneDrive folder id (the DriveItem id).
    /// Keep LAST: a TOML sub-table must follow all scalar keys of its parent table.
    #[serde(skip_serializing_if = "folder_modes_is_empty")]
    pub folder_modes: BTreeMap<String, OneDriveMode>,
}

impl OneDriveModes {
    /// Resolve the effective mode for `folder_id` given its ancestor chain.
    ///
    /// `ancestry` is ordered **deepest-first**: the folder's immediate parent, then its
    /// grandparent, … up toward the drive root (the folder's own id is passed separately as
    /// `folder_id`). The deepest explicit entry wins — the folder itself first, then its
    /// ancestors in order — and if none is set, the account [`OneDriveModes::default_mode`].
    pub fn effective_mode(&self, folder_id: &str, ancestry: &[&str]) -> OneDriveMode {
        if let Some(&m) = self.folder_modes.get(folder_id) {
            return m;
        }
        for pid in ancestry {
            if let Some(&m) = self.folder_modes.get(*pid) {
                return m;
            }
        }
        self.default_mode
    }

    /// Tombstoned-entry cleanup: drop overrides whose folder is no longer present, given the
    /// set of currently-live folder ids. Returns how many overrides were removed.
    pub fn prune(&mut self, live_folder_ids: &HashSet<&str>) -> usize {
        let before = self.folder_modes.len();
        self.folder_modes
            .retain(|id, _| live_folder_ids.contains(id.as_str()));
        before - self.folder_modes.len()
    }

    /// Collect validation problems (empty / whitespace-only folder-id keys). `who` scopes the
    /// message, matching `Config::validate`'s error-accumulating style.
    pub fn validation_errors(&self, who: &str) -> Vec<String> {
        let mut errs = Vec::new();
        for id in self.folder_modes.keys() {
            if id.trim().is_empty() {
                errs.push(format!("{who}: empty folder_modes key"));
            }
        }
        errs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modes(default_mode: OneDriveMode, pairs: &[(&str, OneDriveMode)]) -> OneDriveModes {
        OneDriveModes {
            default_mode,
            folder_modes: pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect(),
        }
    }

    // ---- AC2: inheritance — deepest explicit ancestor wins ----------------------
    #[test]
    fn effective_mode_deepest_ancestor_wins() {
        // Tree: root -> child -> grandchild. Explicit mode only on `child`.
        let m = modes(OneDriveMode::Online, &[("child", OneDriveMode::Offline)]);
        // `grandchild` has no own entry; its ancestry (deepest-first) is [child, root].
        assert_eq!(
            m.effective_mode("grandchild", &["child", "root"]),
            OneDriveMode::Offline,
            "grandchild inherits child's explicit mode"
        );
        // A deeper explicit entry on `grandchild` itself beats the ancestor `child`.
        let m2 = modes(
            OneDriveMode::Online,
            &[
                ("child", OneDriveMode::Offline),
                ("grandchild", OneDriveMode::Sync),
            ],
        );
        assert_eq!(
            m2.effective_mode("grandchild", &["child", "root"]),
            OneDriveMode::Sync,
            "the deepest (grandchild) explicit entry wins over the shallower child"
        );
    }

    #[test]
    fn effective_mode_falls_back_to_default() {
        let m = modes(OneDriveMode::Sync, &[("other", OneDriveMode::Offline)]);
        // No entry anywhere in the chain -> account default_mode.
        assert_eq!(
            m.effective_mode("folder", &["parent", "root"]),
            OneDriveMode::Sync
        );
        // Empty ancestry with no own entry also falls back to default.
        assert_eq!(m.effective_mode("folder", &[]), OneDriveMode::Sync);
    }

    #[test]
    fn effective_mode_folder_itself_beats_ancestors() {
        let m = modes(
            OneDriveMode::Online,
            &[
                ("folder", OneDriveMode::Sync),
                ("parent", OneDriveMode::Offline),
            ],
        );
        // The folder's own explicit mode wins even though an ancestor is also explicit.
        assert_eq!(
            m.effective_mode("folder", &["parent", "root"]),
            OneDriveMode::Sync
        );
    }

    // ---- tombstoned-cleanup helper ---------------------------------------------
    #[test]
    fn prune_removes_tombstoned_entries() {
        let mut m = modes(
            OneDriveMode::Online,
            &[
                ("live", OneDriveMode::Sync),
                ("gone1", OneDriveMode::Offline),
                ("gone2", OneDriveMode::Sync),
            ],
        );
        let live: HashSet<&str> = ["live"].into_iter().collect();
        let removed = m.prune(&live);
        assert_eq!(removed, 2, "two tombstoned entries removed");
        assert_eq!(m.folder_modes.len(), 1);
        assert!(m.folder_modes.contains_key("live"));
    }

    // ---- serde string form matches as_str --------------------------------------
    #[test]
    fn serde_lowercase_matches_as_str() {
        assert_eq!(
            serde_json::to_string(&OneDriveMode::Offline).unwrap(),
            "\"offline\""
        );
        assert_eq!(
            serde_json::to_string(&OneDriveMode::Sync).unwrap(),
            "\"sync\""
        );
        assert_eq!(
            serde_json::to_string(&OneDriveMode::Online).unwrap(),
            "\"online\""
        );
        let back: OneDriveMode = serde_json::from_str("\"sync\"").unwrap();
        assert_eq!(back, OneDriveMode::Sync);
        // as_str() is deckungsgleich with the serde form for every variant.
        for m in [
            OneDriveMode::Online,
            OneDriveMode::Sync,
            OneDriveMode::Offline,
        ] {
            assert_eq!(
                serde_json::to_string(&m).unwrap(),
                format!("\"{}\"", m.as_str())
            );
        }
        // Online is the default (used for skip-when-default).
        assert_eq!(OneDriveMode::default(), OneDriveMode::Online);
        assert!(OneDriveMode::Online.is_default());
        assert!(!OneDriveMode::Sync.is_default());
    }
}
