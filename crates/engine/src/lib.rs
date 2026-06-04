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

use isyncyou_connectors::MailPreview;
use isyncyou_core::guard::{DeleteGuard, Direction, GuardVerdict};
use isyncyou_core::Config;
use isyncyou_graph::GraphClient;
use isyncyou_pathmap::MappingTable;
use isyncyou_store::{Item, Store};
use std::time::{SystemTime, UNIX_EPOCH};

mod mail_restore;
mod restore_key;
mod restore_recovery;
pub use mail_restore::{
    pending_mail_restore_count, recover_pending_mail_restores, recover_pending_mail_restores_with,
    restore_mail_via_ledger, MailApi, MailSink,
};
pub use restore_key::{idempotency_key, load_or_create_secret, mail_marker};
pub use restore_recovery::{recover_restore_op, run_restore_op, RestoreOutcome, RestoreSink};

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

    /// Full write scopes needed to **restore** items across services (re-create
    /// mail/events/contacts/tasks). A superset of [`SYNC_SCOPES`].
    pub const RESTORE_SCOPES: &[&str] = &[
        "Files.ReadWrite",
        "Mail.ReadWrite",
        "Mail.Send",
        "Calendars.ReadWrite",
        "Contacts.ReadWrite",
        "Tasks.ReadWrite",
        "offline_access",
    ];

    /// Resolve a full write token (restore scopes) from the cached `login --write`,
    /// refreshing silently. Used by the daemon's web-UI restore action.
    pub fn resolve_cached_restore_token(cfg: &Config, account: &str) -> Result<String, String> {
        let cache = write_token_cache_path(cfg, account)
            .ok_or_else(|| format!("no account '{account}' in config"))?;
        if !cache.exists() {
            return Err(format!(
                "no cached write token for '{account}' (run `isyncyou login --write` once)"
            ));
        }
        let now = super::unix_now().parse::<u64>().unwrap_or(0);
        isyncyou_graph::auth::flow::ensure_access_token(&cache, WRITE_CLIENT, RESTORE_SCOPES, now)
    }
}

/// Cloud services that can be re-created from their canonical archive.
pub const RESTORE_SERVICES: &[&str] = &["mail", "calendar", "contacts", "todo", "onenote"];

/// Open the account's store and read one archived item's body bytes. Shared by the
/// cloud restore and the (read-only) restore preview. Error messages are stable —
/// the CLI tests assert on them.
fn read_archived_body(
    cfg: &Config,
    account: &str,
    service: &str,
    id: &str,
) -> Result<(Item, Vec<u8>), String> {
    let acc = cfg
        .accounts
        .iter()
        .find(|a| a.id == account)
        .ok_or_else(|| format!("no account '{account}' in config"))?;
    let store =
        Store::open(acc.archive_root.join(".isyncyou-store.db")).map_err(|e| e.to_string())?;
    let item = store
        .get_item(account, service, id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no archived {service} item '{id}' for account '{account}'"))?;
    let rel = item
        .local_path
        .clone()
        .ok_or_else(|| format!("item '{id}' has no archived body yet (run backup first)"))?;
    let bytes = std::fs::read(acc.archive_root.join(&rel)).map_err(|e| e.to_string())?;
    Ok((item, bytes))
}

/// Re-create one archived item back in the cloud via Graph (restore-cloud-item).
/// Opens the account's store, reads the archived body, and re-creates it through
/// the connectors. Shared by the CLI's `restore` and the daemon's web-UI restore
/// action. `token` must carry the write/restore scopes. Returns the new cloud id.
pub fn restore_cloud(
    cfg: &Config,
    account: &str,
    service: &str,
    id: &str,
    token: String,
) -> Result<String, String> {
    use isyncyou_connectors as connectors;
    // Safety gate: cloud-mutating restore is off by default. It re-creates items in
    // the cloud, and the crash-safe operation ledger that makes a retry idempotent
    // (so an interrupted restore cannot duplicate a mailbox item) is still being
    // hardened — so the feature does not ship "mostly safe". Recovering an archived
    // body to a local file goes through a different path and is never gated here.
    if !cfg.restore.cloud_restore_enabled {
        return Err(
            "cloud restore is disabled (set restore.cloud_restore_enabled = true to \
             opt in). It re-creates items in the cloud and the crash-safe operation \
             ledger is still being hardened, so it is off by default. Use \
             `restore --to-local` to recover an archived body to a file instead."
                .to_string(),
        );
    }
    if !RESTORE_SERVICES.contains(&service) {
        return Err(format!(
            "service '{service}' has no cloud restore path (expected one of {}); \
             use restore --to-local to recover its archived body to a file",
            RESTORE_SERVICES.join("|")
        ));
    }
    // Mail goes through the crash-safe operation ledger (ADR-001): record intent,
    // stamp a findable marker, post, and reconcile-not-duplicate on a re-entry after
    // a crash. The other services still use the direct path (their ledger migration
    // is tracked in docs/requirements/restore.yml).
    if service == "mail" {
        return mail_restore::restore_mail_via_ledger(cfg, account, id, token);
    }
    let (item, bytes) = read_archived_body(cfg, account, service, id)?;
    let client = GraphClient::new(token);
    let new_id = match service {
        "mail" => connectors::restore_message(&client, &bytes)?,
        "calendar" => {
            let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            connectors::restore_event(&client, &v)?
        }
        "contacts" => {
            let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            connectors::restore_contact(&client, &v)?
        }
        "todo" => {
            let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            let list = item
                .parent_remote_id
                .as_deref()
                .ok_or("archived task has no parent list id")?;
            connectors::restore_task(&client, list, &v)?
        }
        // OneNote pages re-create from their archived HTML (not a JSON object).
        "onenote" => connectors::restore_page(&client, &bytes)?,
        _ => unreachable!("validated against RESTORE_SERVICES"),
    };
    Ok(new_id)
}

/// A non-destructive preview of what a cloud restore of one archived item *would*
/// create. Built by reading local archive bytes only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePreview {
    pub service: String,
    pub source_item_id: String,
    /// Size of the archived body in bytes.
    pub archived_bytes: usize,
    /// Parsed mail details, present only for `service == "mail"`.
    pub mail: Option<MailPreview>,
    /// One-line human summary.
    pub summary: String,
}

/// Inspect what restoring `id` to the cloud would create, **without mutating
/// anything**. Reads only the local archive — no token, no network, and crucially
/// **not** gated by `cloud_restore_enabled` (a preview is always safe). For mail it
/// returns a parsed [`MailPreview`]; for other services it reports the archived size.
pub fn restore_preview(
    cfg: &Config,
    account: &str,
    service: &str,
    id: &str,
) -> Result<RestorePreview, String> {
    if !RESTORE_SERVICES.contains(&service) {
        return Err(format!(
            "service '{service}' has no cloud restore path (expected one of {})",
            RESTORE_SERVICES.join("|")
        ));
    }
    let (_item, bytes) = read_archived_body(cfg, account, service, id)?;
    let mail = if service == "mail" {
        Some(isyncyou_connectors::mail_preview(&bytes))
    } else {
        None
    };
    let summary = match &mail {
        Some(m) => format!(
            "mail: \"{}\" from {} ({} byte archive){}",
            m.subject.as_deref().unwrap_or("(no subject)"),
            m.from.as_deref().unwrap_or("(unknown sender)"),
            bytes.len(),
            if m.has_html { ", html" } else { "" }
        ),
        None => format!(
            "{service}: a {} byte archive would be re-created as a new cloud item",
            bytes.len()
        ),
    };
    Ok(RestorePreview {
        service: service.to_string(),
        source_item_id: id.to_string(),
        archived_bytes: bytes.len(),
        mail,
        summary,
    })
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

    #[test]
    fn restore_cloud_refuses_when_disabled_by_default() {
        // Default config has restore.cloud_restore_enabled = false. The gate must
        // fire before any store access or network call, so even a missing account
        // surfaces the "disabled" message rather than an account-not-found error.
        let cfg = Config::default();
        let err = restore_cloud(&cfg, "anyone", "mail", "some-id", "tok".into()).unwrap_err();
        assert!(
            err.contains("cloud restore is disabled"),
            "expected disabled-gate message, got: {err}"
        );
    }

    #[test]
    fn restore_preview_reads_mail_without_token_or_gate() {
        let dir = std::env::temp_dir().join(format!("isyncyou-eng-preview-{}", std::process::id()));
        let arch = dir.join("arch");
        std::fs::create_dir_all(arch.join("mail/aa")).unwrap();
        let eml = b"Subject: Hi\r\nFrom: a@example.com\r\nTo: b@example.com\r\n\
                    Content-Type: text/plain\r\n\r\nbody text here";
        std::fs::write(arch.join("mail/aa/m.eml"), eml).unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut it = Item::new("a", "mail", "m1", "Hi", "message");
            it.local_path = Some("mail/aa/m.eml".into());
            store.upsert_item(&it).unwrap();
        }
        let cfg = Config {
            accounts: vec![isyncyou_core::AccountConfig {
                id: "a".into(),
                username: "a@example.com".into(),
                sync_root: dir.join("od"),
                archive_root: arch.clone(),
            }],
            ..Default::default()
        };
        // Gate is OFF (default) and no token is supplied — preview must still work.
        assert!(!cfg.restore.cloud_restore_enabled);
        let p = restore_preview(&cfg, "a", "mail", "m1").unwrap();
        let m = p.mail.expect("mail preview present");
        assert_eq!(m.subject.as_deref(), Some("Hi"));
        assert_eq!(m.from.as_deref(), Some("a@example.com"));
        assert_eq!(m.to, vec!["b@example.com"]);
        assert_eq!(p.archived_bytes, eml.len());
        assert!(p.summary.contains("Hi"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
