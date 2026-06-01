//! Conflict engine: classify two-sided changes and pick a resolution.
//!
//! Bidirectional sync must never silently lose data. When both sides of an item
//! changed since the last sync, this module classifies the [`ConflictKind`] and —
//! given a [`ConflictPolicy`] — chooses a [`Resolution`]. The headless default is
//! always **keep-both** (write a conflict copy) for any data-loss-risk kind, so an
//! unattended daemon never destroys a divergent edit.
//!
//! Pure and deterministic; the actual file/Graph operations live in the engine.

/// What happened to one side (local or remote) relative to the last-synced base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Unchanged,
    Created,
    ContentEdited,
    /// Renamed to the given new name.
    Renamed(String),
    Deleted,
    /// File<->folder type change.
    TypeChanged,
}

/// The kind of conflict between two diverging sides (plan §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    ContentContent,
    RenameRename,
    RenameDelete,
    EditDelete,
    FileFolderType,
    /// Transfer-time: optimistic-concurrency upload precondition failed.
    UploadPreconditionFailed,
    /// Transfer-time: the remote changed while a download was in flight.
    DownloadChangedDuringTransfer,
}

/// The action the engine should take to resolve a conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Upload the local version, overwriting the remote.
    KeepLocal,
    /// Download the remote version, overwriting the local.
    TakeRemote,
    /// Preserve both: rename the local side to a conflict copy, then sync.
    KeepBoth { local_renamed_to: String },
    /// Identical content, only the timestamp differs — just align mtime.
    FixTimestampOnly,
    /// Re-fetch the remote and re-evaluate (no decision yet).
    Reevaluate,
    /// Cannot auto-resolve — leave for the user / GUI prompt.
    Skip,
}

/// How conflicts should be resolved.
#[derive(Debug, Clone)]
pub struct ConflictPolicy {
    /// Headless daemon: never prompt; default to keep-both for data-loss risks.
    pub headless: bool,
    /// Hostname used in conflict-copy names.
    pub host: String,
}

impl ConflictPolicy {
    pub fn headless(host: impl Into<String>) -> Self {
        ConflictPolicy {
            headless: true,
            host: host.into(),
        }
    }
}

/// Classify a conflict from each side's change. Returns `None` when there is no
/// real conflict (only one side changed, both agree, or the changes are
/// independently mergeable like rename-on-one + edit-on-other).
pub fn classify(local: &Change, remote: &Change) -> Option<ConflictKind> {
    use Change::*;
    match (local, remote) {
        // one side unchanged -> the other simply wins; not a conflict
        (Unchanged, _) | (_, Unchanged) => None,

        // both deleted -> agree
        (Deleted, Deleted) => None,

        // type change on either side while the other also changed
        (TypeChanged, _) | (_, TypeChanged) => Some(ConflictKind::FileFolderType),

        (ContentEdited, ContentEdited) | (Created, Created) => Some(ConflictKind::ContentContent),

        (ContentEdited, Deleted) | (Deleted, ContentEdited) => Some(ConflictKind::EditDelete),

        (Renamed(a), Renamed(b)) => {
            if a == b {
                None // both renamed to the same target -> agree
            } else {
                Some(ConflictKind::RenameRename)
            }
        }
        (Renamed(_), Deleted) | (Deleted, Renamed(_)) => Some(ConflictKind::RenameDelete),

        // rename on one side + content edit on the other are independent -> mergeable
        (Renamed(_), ContentEdited) | (ContentEdited, Renamed(_)) => None,

        // created on one side only, or other independent combos -> no conflict
        (Created, _) | (_, Created) => None,
    }
}

/// Choose a [`Resolution`] for a conflict under the given policy.
pub fn resolve(kind: ConflictKind, policy: &ConflictPolicy) -> Resolution {
    use ConflictKind::*;
    match kind {
        // Transfer-time conflicts are not resolved here; the engine retries.
        UploadPreconditionFailed | DownloadChangedDuringTransfer => Resolution::Reevaluate,
        // Data-loss-risk conflicts: keep both by default (always, in headless).
        ContentContent | RenameRename | RenameDelete | EditDelete | FileFolderType => {
            if policy.headless {
                Resolution::KeepBoth {
                    local_renamed_to: String::new(),
                }
            } else {
                Resolution::Skip
            }
        }
    }
}

/// Build a conflict-copy file name (abraunegg-style `safeBackup`), e.g.
/// `report-laptop-safeBackup-0001.txt`. `n` disambiguates repeated conflicts.
pub fn conflict_copy_name(original: &str, host: &str, n: u32) -> String {
    let (stem, ext) = split_ext(original);
    format!("{stem}-{host}-safeBackup-{n:04}{ext}")
}

/// Result of comparing a local and remote version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Versus {
    /// Same content hash — not a real conflict.
    Identical,
    LocalNewer,
    RemoteNewer,
    /// Same whole-second mtime but different content — a genuine conflict.
    Diverged,
}

/// Compare two versions: hash decides identity; otherwise whole-second mtime,
/// with equal-mtime-but-different-content reported as [`Versus::Diverged`]
/// (abraunegg's whole-second compare + hash tiebreak, §23).
pub fn compare_versions(
    local_hash: Option<&str>,
    remote_hash: Option<&str>,
    local_mtime_secs: i64,
    remote_mtime_secs: i64,
) -> Versus {
    if let (Some(a), Some(b)) = (local_hash, remote_hash) {
        if a == b {
            return Versus::Identical;
        }
    }
    match local_mtime_secs.cmp(&remote_mtime_secs) {
        std::cmp::Ordering::Greater => Versus::LocalNewer,
        std::cmp::Ordering::Less => Versus::RemoteNewer,
        std::cmp::Ordering::Equal => Versus::Diverged,
    }
}

fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::Change::*;
    use super::*;

    #[test]
    fn one_sided_change_is_not_a_conflict() {
        assert_eq!(classify(&ContentEdited, &Unchanged), None);
        assert_eq!(classify(&Unchanged, &Deleted), None);
        assert_eq!(classify(&Deleted, &Deleted), None);
    }

    #[test]
    fn both_edited_is_content_conflict() {
        assert_eq!(
            classify(&ContentEdited, &ContentEdited),
            Some(ConflictKind::ContentContent)
        );
        assert_eq!(
            classify(&Created, &Created),
            Some(ConflictKind::ContentContent)
        );
    }

    #[test]
    fn edit_delete_and_rename_delete() {
        assert_eq!(
            classify(&ContentEdited, &Deleted),
            Some(ConflictKind::EditDelete)
        );
        assert_eq!(
            classify(&Deleted, &ContentEdited),
            Some(ConflictKind::EditDelete)
        );
        assert_eq!(
            classify(&Renamed("a".into()), &Deleted),
            Some(ConflictKind::RenameDelete)
        );
    }

    #[test]
    fn rename_rename_only_conflicts_on_divergent_targets() {
        assert_eq!(
            classify(&Renamed("x".into()), &Renamed("y".into())),
            Some(ConflictKind::RenameRename)
        );
        assert_eq!(
            classify(&Renamed("same".into()), &Renamed("same".into())),
            None
        );
    }

    #[test]
    fn rename_plus_edit_is_mergeable() {
        assert_eq!(classify(&Renamed("x".into()), &ContentEdited), None);
        assert_eq!(classify(&ContentEdited, &Renamed("x".into())), None);
    }

    #[test]
    fn type_change_is_file_folder_conflict() {
        assert_eq!(
            classify(&TypeChanged, &ContentEdited),
            Some(ConflictKind::FileFolderType)
        );
    }

    #[test]
    fn headless_keeps_both_for_data_risk() {
        let p = ConflictPolicy::headless("host");
        for k in [
            ConflictKind::ContentContent,
            ConflictKind::RenameRename,
            ConflictKind::RenameDelete,
            ConflictKind::EditDelete,
            ConflictKind::FileFolderType,
        ] {
            assert!(
                matches!(resolve(k, &p), Resolution::KeepBoth { .. }),
                "{k:?}"
            );
        }
    }

    #[test]
    fn gui_policy_skips_for_prompt() {
        let p = ConflictPolicy {
            headless: false,
            host: "h".into(),
        };
        assert_eq!(resolve(ConflictKind::ContentContent, &p), Resolution::Skip);
    }

    #[test]
    fn transfer_conflicts_reevaluate() {
        let p = ConflictPolicy::headless("h");
        assert_eq!(
            resolve(ConflictKind::UploadPreconditionFailed, &p),
            Resolution::Reevaluate
        );
        assert_eq!(
            resolve(ConflictKind::DownloadChangedDuringTransfer, &p),
            Resolution::Reevaluate
        );
    }

    #[test]
    fn conflict_copy_naming() {
        assert_eq!(
            conflict_copy_name("report.txt", "laptop", 1),
            "report-laptop-safeBackup-0001.txt"
        );
        assert_eq!(
            conflict_copy_name("noext", "h", 12),
            "noext-h-safeBackup-0012"
        );
        assert_eq!(
            conflict_copy_name("a.tar.gz", "h", 3),
            "a.tar-h-safeBackup-0003.gz"
        );
        assert_eq!(
            conflict_copy_name(".dotfile", "h", 1),
            ".dotfile-h-safeBackup-0001"
        );
    }

    #[test]
    fn version_comparison() {
        assert_eq!(
            compare_versions(Some("h1"), Some("h1"), 10, 20),
            Versus::Identical
        );
        assert_eq!(
            compare_versions(Some("h1"), Some("h2"), 30, 20),
            Versus::LocalNewer
        );
        assert_eq!(
            compare_versions(Some("h1"), Some("h2"), 10, 20),
            Versus::RemoteNewer
        );
        // same whole-second mtime, different content -> genuine conflict
        assert_eq!(
            compare_versions(Some("h1"), Some("h2"), 20, 20),
            Versus::Diverged
        );
        // no hashes -> fall back to mtime
        assert_eq!(compare_versions(None, None, 5, 4), Versus::LocalNewer);
    }
}
