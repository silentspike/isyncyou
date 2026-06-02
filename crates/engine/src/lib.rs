//! `isyncyou-engine` — the sync **orchestration** that sits above the per-operation
//! connectors: one full bidirectional OneDrive pass for an account.
//!
//! [`sync_once`] is the single source of truth for "what a sync does", shared by
//! the CLI (`isyncyou sync`) and the daemon's background scheduler. It takes an
//! already-open [`Store`] (it never opens one itself) so the caller controls the
//! single-instance lock — the CLI opens per invocation, the daemon holds one
//! shared handle and locks it for the pass.
//!
//! It returns a structured [`SyncReport`] and does **no** printing: the caller
//! decides how to surface progress (stdout lines, an activity-log row, a metric).

use isyncyou_core::guard::{DeleteGuard, Direction, GuardVerdict};
use isyncyou_core::Config;
use isyncyou_graph::GraphClient;
use isyncyou_pathmap::MappingTable;
use isyncyou_store::Store;
use std::time::{SystemTime, UNIX_EPOCH};

/// Structured outcome of one [`sync_once`] pass. All counts are best-effort
/// totals for the pass; `*_blocked` carries the mass-delete-guard reason when a
/// deletion batch was held back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    // remote -> local
    pub upserted: usize,
    pub deleted: usize,
    pub skipped: usize,
    pub resynced: bool,
    pub downloaded: usize,
    pub dirs_created: usize,
    pub materialize_failed: usize,
    pub local_trashed: usize,
    pub local_delete_blocked: Option<String>,
    // local -> remote
    pub uploaded_creates: usize,
    pub modified_uploaded: usize,
    pub modified_conflicts: usize,
    pub modified_failed: usize,
    pub cloud_deleted: usize,
    pub cloud_delete_blocked: Option<String>,
}

impl SyncReport {
    /// One-line human summary of the pass.
    pub fn summary(&self) -> String {
        let mut s = format!(
            "sync: {} upserted, {} deleted, {} skipped{}; {} downloaded, {} trashed; \
             {} created, {} modified, {} cloud-deleted up",
            self.upserted,
            self.deleted,
            self.skipped,
            if self.resynced { " (full resync)" } else { "" },
            self.downloaded,
            self.local_trashed,
            self.uploaded_creates,
            self.modified_uploaded,
            self.cloud_deleted,
        );
        if self.modified_conflicts > 0 {
            s.push_str(&format!(" ({} conflict copies)", self.modified_conflicts));
        }
        if let Some(r) = &self.local_delete_blocked {
            s.push_str(&format!(" [local deletes held: {r}]"));
        }
        if let Some(r) = &self.cloud_delete_blocked {
            s.push_str(&format!(" [cloud deletes held: {r}]"));
        }
        s
    }
}

fn unix_now() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}

/// Read-app OAuth identity + cached-token resolution — the SSOT for the
/// unattended read token both the CLI and the daemon use to sync without a fresh
/// interactive login (a cached `login`/`setup` token is refreshed silently).
pub mod auth {
    use isyncyou_core::Config;
    use std::path::PathBuf;

    /// Public client app registration for read/backup scopes.
    pub const READ_CLIENT: &str = "cee80dd9-c13e-4dbb-9d4c-73eb4987d447";
    /// Delegated read scopes (User.Read lets `setup` confirm the identity).
    pub const READ_SCOPES: &[&str] = &[
        "Files.Read",
        "Mail.Read",
        "Calendars.Read",
        "Contacts.Read",
        "Tasks.Read",
        "Notes.Read",
        "User.Read",
        "offline_access",
    ];
    /// Per-account cached read-token file (under the account's archive root).
    pub const READ_CACHE_FILE: &str = ".isyncyou-token-read.json";

    /// Where an account's cached read token lives.
    pub fn read_token_cache_path(cfg: &Config, account: &str) -> Option<PathBuf> {
        cfg.accounts
            .iter()
            .find(|a| a.id == account)
            .map(|a| a.archive_root.join(READ_CACHE_FILE))
    }

    /// Resolve a usable read access token for `account` from its cached login,
    /// refreshing silently. Errors (rather than blocking) when no token is cached —
    /// the daemon then skips that account until the user runs `login`/`setup`.
    pub fn resolve_cached_read_token(cfg: &Config, account: &str) -> Result<String, String> {
        let cache = read_token_cache_path(cfg, account)
            .ok_or_else(|| format!("no account '{account}' in config"))?;
        if !cache.exists() {
            return Err(format!(
                "no cached token for '{account}' (run `isyncyou login`/`setup` once)"
            ));
        }
        let now = super::unix_now().parse::<u64>().unwrap_or(0);
        isyncyou_graph::auth::flow::ensure_access_token(&cache, READ_CLIENT, READ_SCOPES, now)
    }

    /// Public client app registration for write scopes (restore + bidirectional sync).
    pub const WRITE_CLIENT: &str = "a90d9140-3a62-46d0-907b-f2b7b61a573a";
    /// Scopes a bidirectional OneDrive sync needs — read+write of files only (not
    /// the mail/calendar/notes write scopes, which are for restore). A subset of
    /// what `login --write` consented, so a silent refresh succeeds.
    pub const SYNC_SCOPES: &[&str] = &["Files.ReadWrite", "offline_access"];
    /// Per-account cached write-token file (under the account's archive root).
    pub const WRITE_CACHE_FILE: &str = ".isyncyou-token-write.json";

    /// Where an account's cached write token lives.
    pub fn write_token_cache_path(cfg: &Config, account: &str) -> Option<PathBuf> {
        cfg.accounts
            .iter()
            .find(|a| a.id == account)
            .map(|a| a.archive_root.join(WRITE_CACHE_FILE))
    }

    /// Resolve a read+write file token for unattended **bidirectional** sync from
    /// the cached `login --write`, refreshing silently. Errors (skip) when absent.
    pub fn resolve_cached_sync_token(cfg: &Config, account: &str) -> Result<String, String> {
        let cache = write_token_cache_path(cfg, account)
            .ok_or_else(|| format!("no account '{account}' in config"))?;
        if !cache.exists() {
            return Err(format!(
                "no cached write token for '{account}' (run `isyncyou login --write` once)"
            ));
        }
        let now = super::unix_now().parse::<u64>().unwrap_or(0);
        isyncyou_graph::auth::flow::ensure_access_token(&cache, WRITE_CLIENT, SYNC_SCOPES, now)
    }
}

/// Run one full bidirectional sync pass for `account` against an already-open
/// `store`: pull the remote delta into the store, materialize it to disk, then
/// mirror local creates / modifies (If-Match, keep-both) / deletes up to the
/// cloud — each deletion batch guarded by the mass-delete guard. `host` labels
/// conflict copies (`*-<host>-safeBackup-NNNN`).
pub fn sync_once(
    cfg: &Config,
    account: &str,
    store: &Store,
    client: &mut GraphClient,
    map: &mut MappingTable,
    host: &str,
) -> Result<SyncReport, String> {
    use isyncyou_connectors as connectors;
    let mut out = SyncReport::default();
    let now = unix_now();

    let ingest = connectors::incremental_sync(client, store, map, account, &now)
        .map_err(|e| e.to_string())?;
    out.upserted = ingest.upserted;
    out.deleted = ingest.deleted;
    out.skipped = ingest.skipped;
    out.resynced = ingest.resynced;

    let acc = cfg
        .accounts
        .iter()
        .find(|a| a.id == account)
        .ok_or_else(|| format!("no account '{account}' in config"))?;
    let sync_root = acc.sync_root.clone();
    let trash_root = acc.archive_root.join(".isyncyou-trash");
    let dg = cfg.sync.delete_guard.clone();
    let guard = DeleteGuard {
        max_absolute: dg.max_absolute,
        max_fraction: dg.max_fraction,
        fraction_min_total: dg.fraction_min_total,
    };

    let mat = connectors::materialize_downloads(store, client, account, &sync_root)
        .map_err(|e| e.to_string())?;
    out.downloaded = mat.downloaded;
    out.dirs_created = mat.dirs_created;
    out.materialize_failed = mat.failed;

    // remote -> local deletions (to trash, guarded)
    let pending =
        connectors::pending_local_deletes(store, account, &sync_root).map_err(|e| e.to_string())?;
    if !pending.is_empty() {
        let remaining = store
            .count_by_service(account, "onedrive")
            .map_err(|e| e.to_string())? as usize;
        match guard.evaluate(
            pending.len(),
            remaining + pending.len(),
            Direction::CloudToLocal,
        ) {
            GuardVerdict::Block { reason } => out.local_delete_blocked = Some(reason),
            GuardVerdict::Proceed => {
                out.local_trashed =
                    connectors::apply_local_deletes(&sync_root, &trash_root, &pending)
                        .map_err(|e| e.to_string())?;
            }
        }
    }

    // local -> remote creates
    let creates =
        connectors::scan_local_creates(store, account, &sync_root).map_err(|e| e.to_string())?;
    if !creates.is_empty() {
        out.uploaded_creates =
            connectors::push_local_creates(client, store, map, account, &sync_root, &creates)
                .map_err(|e| e.to_string())?;
    }

    // local -> remote modifies (If-Match, keep-both on conflict)
    let modifies =
        connectors::scan_local_modifies(store, account, &sync_root).map_err(|e| e.to_string())?;
    if !modifies.is_empty() {
        let mr = connectors::apply_local_modifies(
            client, store, map, account, &sync_root, host, &modifies,
        )
        .map_err(|e| e.to_string())?;
        out.modified_uploaded = mr.uploaded;
        out.modified_conflicts = mr.conflicts;
        out.modified_failed = mr.failed;
    }

    // local -> remote deletions (guarded)
    let local_deletes =
        connectors::scan_local_deletes(store, account, &sync_root).map_err(|e| e.to_string())?;
    if !local_deletes.is_empty() {
        let remaining = store
            .count_by_service(account, "onedrive")
            .map_err(|e| e.to_string())? as usize;
        match guard.evaluate(local_deletes.len(), remaining, Direction::LocalToCloud) {
            GuardVerdict::Block { reason } => out.cloud_delete_blocked = Some(reason),
            GuardVerdict::Proceed => {
                for id in &local_deletes {
                    connectors::push_delete(client, store, account, id, &now)
                        .map_err(|e| e.to_string())?;
                    out.cloud_deleted += 1;
                }
            }
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_mentions_each_direction_and_conflicts() {
        let r = SyncReport {
            upserted: 3,
            downloaded: 2,
            uploaded_creates: 1,
            modified_uploaded: 1,
            modified_conflicts: 2,
            cloud_deleted: 1,
            local_delete_blocked: Some("60% > 50%".into()),
            ..Default::default()
        };
        let s = r.summary();
        assert!(s.contains("3 upserted"));
        assert!(s.contains("2 downloaded"));
        assert!(s.contains("1 created"));
        assert!(s.contains("2 conflict copies"));
        assert!(s.contains("local deletes held: 60% > 50%"));
    }

    #[test]
    fn default_report_summary_is_all_zero() {
        let s = SyncReport::default().summary();
        assert!(s.contains("0 upserted"));
        assert!(!s.contains("conflict copies"));
        assert!(!s.contains("held"));
    }
}
