//! OneDrive connector — ingests a Graph delta walk into the store (plan §6).
//!
//! This is the **remote → local** half of the bidirectional sync: it walks the
//! `/me/drive/root/delta` query (via [`isyncyou_graph::run_delta`]), upserts each
//! item into the [`Store`] keyed by id, records tombstones for removed items,
//! maps cloud names to local names via [`MappingTable`], and persists the new
//! delta cursor. The local → remote upload half (driving uploads from local
//! changes) layers on top using the same crates.

use crate::common::shard_path;
use crate::scope::{owning_scope, FolderScope};
use isyncyou_graph::{
    classify, run_delta, DeltaCursor, DeltaError, GraphAction, Outcome, Pacer, Transport,
};
use isyncyou_pathmap::MappingTable;
use isyncyou_store::{Item, Store, StoreError};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

const ROOT_DELTA: &str = "https://graph.microsoft.com/v1.0/me/drive/root/delta";
const SERVICE: &str = "onedrive";

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("delta: {0:?}")]
    Delta(DeltaError),
    #[error("malformed delta item: {0}")]
    Malformed(String),
    #[error("remote: {0}")]
    Remote(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl From<DeltaError> for SyncError {
    fn from(e: DeltaError) -> Self {
        SyncError::Delta(e)
    }
}

/// What one incremental sync changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub upserted: usize,
    pub deleted: usize,
    pub skipped: usize,
    pub resynced: bool,
}

/// Run one incremental delta sync, ingesting changes into `store`. `now` is the
/// RFC3339 timestamp used for tombstones (supplied by the caller so this is
/// deterministic / testable).
pub fn incremental_sync<T: Transport>(
    transport: &mut T,
    store: &Store,
    map: &mut MappingTable,
    account: &str,
    now: &str,
    archive_root: &Path,
) -> Result<SyncReport, SyncError> {
    let cursor = store
        .get_delta_cursor(account, SERVICE, "")?
        .map(DeltaCursor::new);
    let out = run_delta(transport, ROOT_DELTA, cursor.as_ref(), 5)?;

    let mut report = SyncReport {
        resynced: out.resynced,
        ..Default::default()
    };
    for item in &out.items {
        match ingest_item(
            store,
            map,
            account,
            item,
            now,
            "remote_dirty",
            Some(archive_root),
        )? {
            Ingest::Upserted => report.upserted += 1,
            Ingest::Deleted => report.deleted += 1,
            Ingest::Skipped => report.skipped += 1,
        }
    }
    store.set_delta_cursor(account, SERVICE, "", out.cursor.as_str())?;
    Ok(report)
}

enum Ingest {
    Upserted,
    Deleted,
    Skipped,
}

/// Ingest a single OneDrive delta item into the store.
fn ingest_item(
    store: &Store,
    map: &mut MappingTable,
    account: &str,
    item: &Value,
    now: &str,
    state: &str,
    // `Some` from the delta pass (writes the full-metadata sidecar); `None` from
    // the local→remote push paths, whose freshly-uploaded item lacks the
    // server-enriched facets (EXIF/image dims) — the next delta writes those.
    archive_root: Option<&Path>,
) -> Result<Ingest, SyncError> {
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| SyncError::Malformed("item has no id".into()))?;

    // Tombstone: the `deleted` facet (or the legacy `@removed`) marks removal.
    if item.get("deleted").is_some() || item.get("@removed").is_some() {
        store.mark_deleted(account, SERVICE, id, now)?;
        if let Some(root) = archive_root {
            remove_item_json(root, id);
        }
        return Ok(Ingest::Deleted);
    }

    // The drive root has a `root` facet and no usable parent/name — skip it.
    if item.get("root").is_some() {
        return Ok(Ingest::Skipped);
    }

    let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
    let parent = item
        .get("parentReference")
        .and_then(|p| p.get("id"))
        .and_then(Value::as_str)
        .map(String::from);
    let is_folder = item.get("folder").is_some();

    // Map the cloud name to a local name within its parent.
    let local_name = match &parent {
        Some(p) => map.assign_local_name(p, name),
        None => name.to_string(),
    };

    let mut it = Item::new(
        account,
        SERVICE,
        id,
        name,
        if is_folder { "folder" } else { "file" },
    );
    it.parent_remote_id = parent;
    it.local_path = Some(local_name);
    it.etag = item.get("eTag").and_then(Value::as_str).map(String::from);
    it.ctag = item.get("cTag").and_then(Value::as_str).map(String::from);
    it.quickxorhash = item
        .pointer("/file/hashes/quickXorHash")
        .and_then(Value::as_str)
        .map(String::from);
    it.size = item.get("size").and_then(Value::as_i64);
    it.remote_mtime = item
        .pointer("/fileSystemInfo/lastModifiedDateTime")
        .and_then(Value::as_str)
        .map(String::from);
    it.sync_state = state.into();
    store.upsert_item(&it)?;
    // Archive the full DriveItem JSON so the rich metadata the 9 indexed columns
    // can't hold is captured straight from the delta payload (#564).
    if let Some(root) = archive_root {
        write_item_json(root, id, item)?;
    }
    Ok(Ingest::Upserted)
}

/// Archive the full Graph DriveItem JSON (`onedrive/<shard>/<id>.json`) so the
/// backup captures every rich facet the indexed columns can't hold — `mimeType`,
/// `sha256Hash` (alongside the indexed quickXor), created/last-modified-by,
/// `webUrl`, `image`/`photo`/`video`/`audio` + GPS, `shared`, `malware`,
/// `specialFolder`, `folder.childCount`, `package` (#564). Written at ingest
/// straight from the delta payload (no extra fetch, no new scope) and rewritten
/// on every change so the metadata stays current. Atomic tmp+rename.
fn write_item_json(archive_root: &Path, id: &str, item: &Value) -> Result<(), SyncError> {
    let abs = shard_path(archive_root, SERVICE, id, "json");
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(item).map_err(|e| SyncError::Malformed(e.to_string()))?;
    let tmp = abs.with_extension("json.part");
    std::fs::write(&tmp, isyncyou_core::envelope::seal_for_disk(&bytes))?;
    std::fs::rename(&tmp, &abs)?;
    Ok(())
}

/// Drop the archived metadata sidecar when its item is tombstoned.
fn remove_item_json(archive_root: &Path, id: &str) {
    let _ = std::fs::remove_file(shard_path(archive_root, SERVICE, id, "json"));
}

// ---- Scoped per-folder delta (Mode 2/3, S-OM.7) --------------------------------
//
// Additive to `incremental_sync` (the global-root desktop/FUSE path). Mode 2/3 sync
// only the configured folders: for each `FolderScope` we walk that folder's own
// `driveItem` delta — which Graph reports **recursively over the subtree** — with a
// per-folder cursor. Because a configured parent and child overlap (the parent delta
// reports the child's items too), `owning_scope` assigns each item to the deepest
// active ancestor, so an item belongs to exactly one active scope.

/// The per-folder recursive delta endpoint (analogue of `ROOT_DELTA`).
fn item_delta_url(folder_id: &str) -> String {
    format!("https://graph.microsoft.com/v1.0/me/drive/items/{folder_id}/delta")
}

/// Build an item's ancestry `[id, parent, grandparent, …]` (deepest first) by walking
/// `parent_remote_id` through the store. Follows **tombstoned** intermediate folders
/// too (`get_item` does not filter deleted rows), so a whole-subtree delete still
/// resolves ownership. Bounded against cycles / pathological depth.
fn store_ancestry(store: &Store, account: &str, id: &str) -> Result<Vec<String>, SyncError> {
    let mut chain = vec![id.to_string()];
    let mut seen: HashSet<String> = std::iter::once(id.to_string()).collect();
    let mut cur = id.to_string();
    while chain.len() < 256 {
        match store
            .get_item(account, SERVICE, &cur)?
            .and_then(|it| it.parent_remote_id)
        {
            Some(p) if seen.insert(p.clone()) => {
                chain.push(p.clone());
                cur = p;
            }
            _ => break,
        }
    }
    Ok(chain)
}

/// Number of active scope roots on a folder's ancestry — its "depth" for deepest-first
/// scope ordering. A scope root owns at least itself, so this is ≥1 once its hierarchy
/// is in the store; unknown ancestors just yield a shallower (safe) estimate.
fn scope_depth(store: &Store, account: &str, folder_id: &str, active: &BTreeSet<&str>) -> usize {
    store_ancestry(store, account, folder_id)
        .map(|chain| {
            chain
                .iter()
                .filter(|id| active.contains(id.as_str()))
                .count()
        })
        .unwrap_or(1)
}

/// Walk one folder-scoped delta **page by page**, persisting the cursor after every
/// page. On each 2xx page: ingest it (`on_page`) *then* persist — a `@odata.nextLink`
/// is stored as the cursor so a crash resumes from the last completed page; the
/// `@odata.deltaLink` is adopted only at the very end. The stored cursor is polymorph
/// (a `nextLink` mid-enumeration or a `deltaLink` once caught up) and is followed
/// uniformly as "the next url". Retry/backoff and `410 Gone` resync mirror
/// [`isyncyou_graph::run_delta`], reusing its public building blocks so this does not
/// touch the graph crate. Returns whether a `410` resync happened.
fn walk_scope_delta<T, F>(
    transport: &mut T,
    store: &Store,
    account: &str,
    scope_id: &str,
    base_url: &str,
    max_retries: u32,
    mut on_page: F,
) -> Result<bool, SyncError>
where
    T: Transport,
    F: FnMut(&[Value]) -> Result<(), SyncError>,
{
    let mut url = store
        .get_delta_cursor(account, SERVICE, scope_id)?
        .unwrap_or_else(|| base_url.to_string());
    let mut resynced = false;
    let mut retries = 0u32;
    let mut pacer = Pacer::new();

    loop {
        let resp = transport.get(&url);
        match classify(resp.status, resp.retry_after) {
            GraphAction::Ok => {
                retries = 0;
                pacer.update(Outcome::Ok);
                let body = resp.body.ok_or(SyncError::Delta(DeltaError::MissingBody))?;
                if let Some(arr) = body.get("value").and_then(|v| v.as_array()) {
                    // Ingest the page BEFORE advancing the cursor, so a crash resumes
                    // at this page and re-ingest is idempotent (upsert by id,
                    // deterministic name mapping, mark_deleted set-once).
                    on_page(arr)?;
                }
                if let Some(next) = body.get("@odata.nextLink").and_then(|v| v.as_str()) {
                    store.set_delta_cursor(account, SERVICE, scope_id, next)?;
                    url = next.to_string();
                    continue;
                }
                if let Some(delta) = body.get("@odata.deltaLink").and_then(|v| v.as_str()) {
                    store.set_delta_cursor(account, SERVICE, scope_id, delta)?;
                    return Ok(resynced);
                }
                return Err(SyncError::Delta(DeltaError::NoCursor));
            }
            GraphAction::Retry { after } => {
                retries += 1;
                if retries > max_retries {
                    return Err(SyncError::Delta(DeltaError::TooManyRetries));
                }
                transport.backoff(pacer.update(Outcome::Retry { after }));
                continue;
            }
            GraphAction::Resync => {
                // 410 Gone: the stored token is stale. Drop it so a crash mid-resync
                // restarts cleanly from base_url, then re-walk the whole subtree.
                store.clear_delta_cursor(account, SERVICE, scope_id)?;
                resynced = true;
                retries = 0;
                url = base_url.to_string();
                continue;
            }
            GraphAction::RefreshAuth => return Err(SyncError::Delta(DeltaError::AuthExpired)),
            _ => return Err(SyncError::Delta(DeltaError::Fatal(resp.status))),
        }
    }
}

/// The ownership context for a scoped ingest: the scope currently being walked plus
/// the full set of active scope roots (for deepest-wins resolution).
struct ScopeCtx<'a> {
    scope_id: &'a str,
    active: &'a BTreeSet<&'a str>,
}

/// Ingest one delta item under a specific scope, applying scope-ownership.
///
/// Tombstone rule (corrected for OneDrive's recursive/nested delta — the mail
/// `parent_remote_id == folder_id` check would leak nested deletes): decide purely via
/// `owning_scope` over the whole subtree. Tombstone **unless** the item provably lives
/// in a *different* active scope (a move-in already applied by that scope's delta →
/// don't clobber). Owned by this scope (incl. deeply nested) or unknown → tombstone
/// (`mark_deleted` is a no-op if there is no row, and a real tombstone for an orphan).
/// Non-removed items are upserted by id (idempotent, id-stable move); the return value
/// counts the item under its **owning** scope (an item a deeper scope owns is reported
/// `Skipped` here — one active scope per item).
fn ingest_item_scoped(
    store: &Store,
    map: &mut MappingTable,
    account: &str,
    ctx: &ScopeCtx,
    item: &Value,
    now: &str,
    archive_root: Option<&Path>,
) -> Result<Ingest, SyncError> {
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| SyncError::Malformed("item has no id".into()))?;

    if item.get("deleted").is_some() || item.get("@removed").is_some() {
        let ancestry = store_ancestry(store, account, id)?;
        let refs: Vec<&str> = ancestry.iter().map(String::as_str).collect();
        if let Some(owner) = owning_scope(&refs, ctx.active) {
            if owner != ctx.scope_id {
                // Moved into a different active scope — that scope's delta already
                // reparented the id; removing it here would clobber the move.
                return Ok(Ingest::Skipped);
            }
        }
        store.mark_deleted(account, SERVICE, id, now)?;
        if let Some(root) = archive_root {
            remove_item_json(root, id);
        }
        return Ok(Ingest::Deleted);
    }

    // The drive root has a `root` facet and no usable parent/name — skip it.
    if item.get("root").is_some() {
        return Ok(Ingest::Skipped);
    }

    let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
    let parent = item
        .get("parentReference")
        .and_then(|p| p.get("id"))
        .and_then(Value::as_str)
        .map(String::from);
    let is_folder = item.get("folder").is_some();
    let local_name = match &parent {
        Some(p) => map.assign_local_name(p, name),
        None => name.to_string(),
    };

    let mut it = Item::new(
        account,
        SERVICE,
        id,
        name,
        if is_folder { "folder" } else { "file" },
    );
    it.parent_remote_id = parent;
    it.local_path = Some(local_name);
    it.etag = item.get("eTag").and_then(Value::as_str).map(String::from);
    it.ctag = item.get("cTag").and_then(Value::as_str).map(String::from);
    it.quickxorhash = item
        .pointer("/file/hashes/quickXorHash")
        .and_then(Value::as_str)
        .map(String::from);
    it.size = item.get("size").and_then(Value::as_i64);
    it.remote_mtime = item
        .pointer("/fileSystemInfo/lastModifiedDateTime")
        .and_then(Value::as_str)
        .map(String::from);
    it.sync_state = "remote_dirty".into();
    store.upsert_item(&it)?;
    if let Some(root) = archive_root {
        write_item_json(root, id, item)?;
    }

    // Ownership for reporting: an item a deeper active scope owns is reported Skipped
    // (counted by that scope). The row is upserted regardless (idempotent, id-stable).
    let ancestry = store_ancestry(store, account, id)?;
    let refs: Vec<&str> = ancestry.iter().map(String::as_str).collect();
    match owning_scope(&refs, ctx.active) {
        Some(owner) if owner != ctx.scope_id => Ok(Ingest::Skipped),
        _ => Ok(Ingest::Upserted),
    }
}

/// Run one scoped incremental sync (Mode 2/3): walk each configured folder's own
/// `driveItem` delta with a per-folder cursor + scope-ownership. Additive to
/// [`incremental_sync`] (the global-root path). `scopes` is injected by the caller (a
/// later wiring story builds it from the config mode map); this function is fully
/// self-contained and unit-tested in isolation.
pub fn incremental_sync_scoped<T: Transport>(
    transport: &mut T,
    store: &Store,
    map: &mut MappingTable,
    account: &str,
    now: &str,
    archive_root: &Path,
    scopes: &[FolderScope],
) -> Result<SyncReport, SyncError> {
    let active: BTreeSet<&str> = scopes.iter().map(|s| s.folder_id.as_str()).collect();
    // Deepest-first so a cross-scope move-in (written by the deeper scope) precedes the
    // shallower scope's removal report — ownership then resolves to the deeper scope and
    // the move is skipped, never tombstoned. (A move implies the item pre-existed, so
    // its folder hierarchy is already in the store and the depth is known.)
    let mut ordered: Vec<&FolderScope> = scopes.iter().collect();
    ordered.sort_by_key(|s| std::cmp::Reverse(scope_depth(store, account, &s.folder_id, &active)));

    let mut report = SyncReport::default();
    for scope in ordered {
        let base = item_delta_url(&scope.folder_id);
        let ctx = ScopeCtx {
            scope_id: scope.folder_id.as_str(),
            active: &active,
        };
        let resynced =
            walk_scope_delta(transport, store, account, ctx.scope_id, &base, 5, |arr| {
                for item in arr {
                    match ingest_item_scoped(
                        store,
                        map,
                        account,
                        &ctx,
                        item,
                        now,
                        Some(archive_root),
                    )? {
                        Ingest::Upserted => report.upserted += 1,
                        Ingest::Deleted => report.deleted += 1,
                        Ingest::Skipped => report.skipped += 1,
                    }
                }
                Ok(())
            })?;
        report.resynced |= resynced;
    }
    Ok(report)
}

/// Abstraction over the remote write operations, so the local→remote push driver
/// is unit-testable with a mock and live-tested with the real client.
pub trait RemoteWriter {
    /// Upload `data` to `dest_path`; returns the created drive item JSON.
    fn upload(&self, dest_path: &str, data: &[u8]) -> Result<Value, String>;
    /// Delete a drive item by id.
    fn delete(&self, item_id: &str) -> Result<(), String>;
    /// Upload with a **persisted** resumable session (plan §6/§9) so a process kill
    /// mid-upload resumes instead of restarting. The default ignores `resume` and
    /// calls [`upload`](Self::upload), so test mocks need no change.
    fn upload_resumable(
        &self,
        dest_path: &str,
        data: &[u8],
        _resume: &dyn isyncyou_graph::UploadResumeStore,
    ) -> Result<Value, String> {
        self.upload(dest_path, data)
    }
    /// Set a drive item's `fileSystemInfo.lastModifiedDateTime` (RFC3339 UTC) and
    /// return the updated item JSON, so an upload preserves the file's original
    /// timestamp instead of the upload time. The default is a no-op returning
    /// `Value::Null` (mocks need no change); `push_upload` then keeps the upload
    /// item's own timestamp.
    fn set_mtime(&self, _item_id: &str, _rfc3339: &str) -> Result<Value, String> {
        Ok(Value::Null)
    }
}

#[cfg(feature = "http")]
impl RemoteWriter for isyncyou_graph::GraphClient {
    fn upload(&self, dest_path: &str, data: &[u8]) -> Result<Value, String> {
        // 10 MiB fragments (320 KiB-aligned) for the resumable path.
        self.upload_file(dest_path, data, 10 * 1024 * 1024)
            .map_err(|e| e.to_string())
    }
    fn delete(&self, item_id: &str) -> Result<(), String> {
        self.delete_item(item_id).map_err(|e| e.to_string())
    }
    fn upload_resumable(
        &self,
        dest_path: &str,
        data: &[u8],
        resume: &dyn isyncyou_graph::UploadResumeStore,
    ) -> Result<Value, String> {
        self.upload_file_resumable(dest_path, data, 10 * 1024 * 1024, resume)
            .map_err(|e| e.to_string())
    }
    fn set_mtime(&self, item_id: &str, rfc3339: &str) -> Result<Value, String> {
        self.patch_json(
            &format!("/me/drive/items/{item_id}"),
            &serde_json::json!({ "fileSystemInfo": { "lastModifiedDateTime": rfc3339 } }),
        )
        .map_err(|e| e.to_string())
    }
}

/// An [`isyncyou_graph::UploadResumeStore`] backed by the store's `upload_sessions`
/// table for one account, so large OneDrive uploads survive a process kill.
struct StoreResume<'a> {
    store: &'a Store,
    account: &'a str,
}

impl isyncyou_graph::UploadResumeStore for StoreResume<'_> {
    fn load(&self, dest: &str) -> Option<(String, u64)> {
        self.store
            .get_upload_session(self.account, SERVICE, dest)
            .ok()
            .flatten()
    }
    fn save(&self, dest: &str, upload_url: &str, total: u64, next_offset: u64) {
        let _ = self.store.save_upload_session(
            self.account,
            SERVICE,
            dest,
            upload_url,
            total as i64,
            next_offset as i64,
        );
    }
    fn clear(&self, dest: &str) {
        let _ = self.store.clear_upload_session(self.account, SERVICE, dest);
    }
}

/// Push a local file to the cloud: upload it, then ingest the returned item into
/// the store as `clean`. Returns the new remote id.
pub fn push_upload<W: RemoteWriter>(
    writer: &W,
    store: &Store,
    map: &mut MappingTable,
    account: &str,
    dest_path: &str,
    data: &[u8],
    local_mtime_secs: Option<i64>,
) -> Result<String, SyncError> {
    let resume = StoreResume { store, account };
    let item = writer
        .upload_resumable(dest_path, data, &resume)
        .map_err(SyncError::Remote)?;
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| SyncError::Malformed("upload response had no id".into()))?;
    // Preserve the file's original timestamp: stamp the item's fileSystemInfo with
    // the local mtime so the cloud keeps it instead of the upload time. Ingest the
    // updated item (with the corrected timestamp) when set_mtime returns one.
    let to_ingest = match local_mtime_secs {
        Some(secs) => {
            let updated = writer
                .set_mtime(&id, &unix_to_rfc3339(secs))
                .map_err(SyncError::Remote)?;
            if updated.is_null() {
                item
            } else {
                updated
            }
        }
        None => item,
    };
    ingest_item(store, map, account, &to_ingest, "", "clean", None)?;
    Ok(id)
}

/// Push a local deletion to the cloud: delete the remote item, then tombstone it
/// in the store.
pub fn push_delete<W: RemoteWriter>(
    writer: &W,
    store: &Store,
    account: &str,
    remote_id: &str,
    now: &str,
) -> Result<(), SyncError> {
    writer.delete(remote_id).map_err(SyncError::Remote)?;
    store.mark_deleted(account, SERVICE, remote_id, now)?;
    Ok(())
}

/// Fetches a drive item's content bytes. Abstracted so materialization is
/// unit-testable with a mock and live-tested with the real client.
pub trait Downloader {
    fn download(&self, remote_id: &str) -> Result<Vec<u8>, String>;
    /// Download reporting the cumulative bytes read through `on_progress` as they arrive, so the
    /// materialize can show a moving download bar (#656 F-C). The default buffers via
    /// [`download`](Self::download) and reports once at the end — mocks need no change.
    fn download_with_progress(
        &self,
        remote_id: &str,
        on_progress: &mut dyn FnMut(u64),
    ) -> Result<Vec<u8>, String> {
        let bytes = self.download(remote_id)?;
        on_progress(bytes.len() as u64);
        Ok(bytes)
    }
}

#[cfg(feature = "http")]
impl Downloader for isyncyou_graph::GraphClient {
    fn download(&self, remote_id: &str) -> Result<Vec<u8>, String> {
        self.download_content(remote_id).map_err(|e| e.to_string())
    }
    fn download_with_progress(
        &self,
        remote_id: &str,
        on_progress: &mut dyn FnMut(u64),
    ) -> Result<Vec<u8>, String> {
        self.download_content_with_progress(remote_id, on_progress)
            .map_err(|e| e.to_string())
    }
}

/// What one materialization pass wrote to disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaterializeReport {
    pub downloaded: usize,
    pub dirs_created: usize,
    pub skipped: usize,
    pub failed: usize,
    /// Locally-edited files moved aside as `safeBackup` conflict copies before
    /// the newer cloud version was written (download-path keep-both).
    pub conflicts: usize,
    /// Files skipped because the user cancelled the transfer before it started.
    /// Distinct from `failed` — a user-requested cancel is not an error (#656).
    pub cancelled: usize,
}

/// One in-flight transfer's progress — the connectors-layer producer type, mirror of
/// `webui::TransferState` (#655 / S-OM.9). `app-host`'s `DaemonTransfer` maps it onto the
/// webui type for `GET /api/v1/onedrive/transfers`. `bytes_total == 0` means the size is
/// not yet known; `retry_after_secs > 0` means it is backing off on a 429.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransferSlot {
    pub id: String,
    pub name: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub retry_after_secs: u64,
}

/// Where the offline materialize/writeback pass reports per-file progress, so the UI
/// (`/onedrive/transfers`) can show live download/upload progress (#655 / S-OM.9). The
/// no-op `()` impl lets the desktop path and host tests ignore progress entirely.
pub trait ProgressSink: Send + Sync {
    /// A transfer started: `id` (the item's remote id), a display `name`, and the total
    /// byte count (`0` when unknown).
    fn begin(&self, id: &str, name: &str, bytes_total: u64);
    /// Bytes transferred so far for `id`.
    fn advance(&self, id: &str, bytes_done: u64);
    /// `id` is backing off on a 429 for `secs` seconds.
    fn retry_after(&self, id: &str, secs: u64);
    /// `id` finished (success or failure) — drop it from the in-flight set.
    fn finish(&self, id: &str);
    /// True if a cancel was requested for `id`. The materialize pass checks this before
    /// starting each queued file and skips a cancelled one (best-effort, queue-deep — a
    /// download already in flight still runs to completion). Default: never cancelled (#656).
    fn is_cancelled(&self, _id: &str) -> bool {
        false
    }
    /// Consume the one-shot cancel for `id` (called on skip) so a later pass re-materializes
    /// the file instead of skipping it forever. Default: no-op (#656).
    fn consume_cancel(&self, _id: &str) {}
}

impl ProgressSink for () {
    fn begin(&self, _: &str, _: &str, _: u64) {}
    fn advance(&self, _: &str, _: u64) {}
    fn retry_after(&self, _: &str, _: u64) {}
    fn finish(&self, _: &str) {}
}

/// A shared, thread-safe in-flight transfer set: the offline pass writes it (as a
/// [`ProgressSink`]) and the `/onedrive/transfers` endpoint reads a [`snapshot`] of it.
/// Cheaply cloneable (one shared `Arc<Mutex<…>>`), so the engine and the router hold the
/// same handle.
///
/// [`snapshot`]: SharedProgress::snapshot
#[derive(Clone, Default)]
pub struct SharedProgress {
    slots: std::sync::Arc<std::sync::Mutex<Vec<TransferSlot>>>,
    /// Remote ids with a pending one-shot cancel request (#656). The materialize pass reads
    /// this via [`ProgressSink::is_cancelled`] before each file and consumes it on skip.
    cancels: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

impl SharedProgress {
    pub fn new() -> Self {
        Self::default()
    }

    /// A snapshot of the current in-flight transfers (for the read-only endpoint).
    pub fn snapshot(&self) -> Vec<TransferSlot> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Request cancellation of transfer `id` (the item's remote id). The materialize pass
    /// skips it at its next file boundary; a download already in flight still completes
    /// (best-effort, queue-deep) (#656).
    pub fn request_cancel(&self, id: &str) {
        self.cancels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string());
    }

    fn with<R>(&self, f: impl FnOnce(&mut Vec<TransferSlot>) -> R) -> R {
        let mut g = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut g)
    }
}

impl ProgressSink for SharedProgress {
    fn begin(&self, id: &str, name: &str, bytes_total: u64) {
        self.with(|v| match v.iter_mut().find(|s| s.id == id) {
            Some(s) => {
                s.name = name.to_string();
                s.bytes_total = bytes_total;
                s.bytes_done = 0;
                s.retry_after_secs = 0;
            }
            None => v.push(TransferSlot {
                id: id.to_string(),
                name: name.to_string(),
                bytes_done: 0,
                bytes_total,
                retry_after_secs: 0,
            }),
        });
    }
    fn advance(&self, id: &str, bytes_done: u64) {
        self.with(|v| {
            if let Some(s) = v.iter_mut().find(|s| s.id == id) {
                s.bytes_done = bytes_done;
            }
        });
    }
    fn retry_after(&self, id: &str, secs: u64) {
        self.with(|v| {
            if let Some(s) = v.iter_mut().find(|s| s.id == id) {
                s.retry_after_secs = secs;
            }
        });
    }
    fn finish(&self, id: &str) {
        self.with(|v| v.retain(|s| s.id != id));
        // Clear any pending cancel for a finished transfer so a stale request can't affect a
        // future transfer that reuses the same remote id.
        self.cancels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }
    fn is_cancelled(&self, id: &str) -> bool {
        self.cancels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(id)
    }
    fn consume_cancel(&self, id: &str) {
        self.cancels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }
}

/// Guard against a malformed parent cycle when walking to the drive root.
const MAX_PATH_DEPTH: usize = 256;

/// Build an item's path **relative to the sync root** by walking `parent_remote_id`
/// up through `by_id`, collecting each ancestor's mapped local name. Stops at the
/// drive root (whose id is absent from the store). `None` if an item on the chain
/// has no local name or a cycle is hit.
///
/// Public so integrity checks (`isyncyou verify`) can resolve where a synced
/// OneDrive item lives on disk — its `local_path` is only the name *segment*
/// (resolved through its parents), unlike the archive-relative body paths of the
/// backup services.
pub fn local_rel_path(by_id: &HashMap<&str, &Item>, it: &Item) -> Option<PathBuf> {
    let mut parts: Vec<&str> = Vec::new();
    let mut cur = it;
    for _ in 0..MAX_PATH_DEPTH {
        parts.push(cur.local_path.as_deref()?);
        match cur.parent_remote_id.as_deref() {
            Some(pid) => match by_id.get(pid) {
                Some(parent) => cur = parent,
                None => break, // parent is the drive root → done
            },
            None => break,
        }
    }
    parts.reverse();
    Some(parts.iter().collect())
}

/// Atomic write: temp file in the same dir + rename, so a reader never sees a
/// partial file and an interrupted download is safe to restart.
fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!(
        "{}.isync-tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    std::fs::write(&tmp, isyncyou_core::envelope::seal_for_disk(data))?;
    std::fs::rename(&tmp, path)
}

/// Days since the Unix epoch for a civil (proleptic-Gregorian) date — Howard
/// Hinnant's algorithm. `month` is 1..=12.
fn days_from_civil(y: i64, month: i64, d: i64) -> i64 {
    let y = if month <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12; // Mar=0 … Feb=11
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Parse a Graph RFC3339 timestamp (e.g. `2024-01-02T03:04:05Z` or with
/// fractional seconds) into seconds since the Unix epoch. Assumes UTC (Graph
/// returns `Z`); fractional seconds and any trailing zone are ignored. Returns
/// `None` on a malformed string — best-effort, never panics.
fn rfc3339_to_unix(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let y = num(0, 4)?;
    let mo = num(5, 7)?;
    let d = num(8, 10)?;
    let h = num(11, 13)?;
    let mi = num(14, 16)?;
    let se = num(17, 19)?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se)
}

/// Inverse of [`days_from_civil`] (Howard Hinnant's civil-from-days): days since the
/// Unix epoch → `(year, month, day)`. Valid across the proleptic Gregorian calendar.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Format a Unix timestamp (seconds) as an RFC3339 UTC string
/// (`YYYY-MM-DDTHH:MM:SSZ`) — the inverse of [`rfc3339_to_unix`]. Used to stamp a
/// just-uploaded item's `fileSystemInfo.lastModifiedDateTime` with the file's local
/// mtime so the cloud preserves the original timestamp instead of the upload time.
fn unix_to_rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, mo, d) = civil_from_days(days);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// A file's on-disk mtime as Unix seconds, or `None` if unreadable — best-effort
/// input for preserving the original timestamp on upload.
fn local_mtime_secs(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

/// True if the local file already matches the store record by **size + mtime**,
/// so a re-download would be redundant (e.g. after a `410` resync re-marked
/// everything `remote_dirty`). Cheap heuristic; a content hash would be
/// definitive (a follow-up). A file without a stored size/mtime never matches.
/// Persist the last-synced on-disk reference for an item from the file that was
/// just written/uploaded: its actual disk size + mtime, plus the content hash.
/// Best-effort — a metadata failure only means the next pass treats the item as
/// reference-less (legacy behavior), never an error.
fn record_synced_state(
    store: &Store,
    account: &str,
    remote_id: &str,
    full: &Path,
    hash: Option<String>,
) {
    if let Ok(meta) = std::fs::metadata(full) {
        let mtime = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let _ = store.set_synced_state(
            account,
            SERVICE,
            remote_id,
            meta.len() as i64,
            mtime,
            hash.as_deref(),
        );
    }
}

/// Whether the on-disk file was edited since the last sync, judged against the
/// **last-synced reference** (not the item's current — already remote-updated —
/// metadata). Same cheap-first ladder as [`is_local_modified`]: size, then
/// mtime, then a QuickXorHash content check so a pure `touch` is not a conflict.
fn locally_edited_since_sync(full: &Path, ssize: i64, smtime: i64, shash: Option<&str>) -> bool {
    let meta = match std::fs::metadata(full) {
        Ok(m) => m,
        Err(_) => return false, // nothing on disk → nothing to protect
    };
    if meta.len() as i64 != ssize {
        return true;
    }
    let disk_mtime = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);
    if disk_mtime == Some(smtime) {
        return false;
    }
    match (shash, isyncyou_core::envelope::read_body(full)) {
        (Some(h), Ok(data)) => crate::quickxor::quickxor_base64(&data) != h,
        // no stored hash / unreadable: same size + different mtime alone is not
        // enough evidence to declare a conflict
        _ => false,
    }
}

fn local_file_matches(path: &Path, it: &Item) -> bool {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    // Compare the item's stored (cloud/plaintext) size to the file's *plaintext* size — on
    // mobile the on-disk file is a sealed envelope larger than the plaintext, so `meta.len()`
    // would never match. `on_disk_plaintext_len` reads the envelope header (or the raw length
    // for a desktop plaintext file), so this holds for both.
    if it.size != Some(isyncyou_core::envelope::on_disk_plaintext_len(path) as i64) {
        return false;
    }
    match (&it.remote_mtime, meta.modified().ok()) {
        (Some(rm), Some(m)) => {
            let local = m
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs());
            rfc3339_to_unix(rm).filter(|s| *s >= 0).map(|s| s as u64) == local
        }
        _ => false,
    }
}

/// Best-effort: set a just-materialized file's mtime to its cloud
/// `lastModifiedDateTime`, so local timestamps mirror the cloud (plan §6) instead
/// of showing the download time. Silently does nothing on a bad timestamp or a
/// platform that rejects the set.
fn set_file_mtime(path: &Path, remote_mtime: &str) {
    if let Some(secs) = rfc3339_to_unix(remote_mtime) {
        if secs >= 0 {
            if let Ok(f) = std::fs::File::open(path) {
                let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64);
                let _ = f.set_modified(when);
            }
        }
    }
}

/// Materialize **remote-dirty** OneDrive items to disk under `sync_root`: create
/// folders, download file content, write it atomically, and mark each item
/// `clean` so a re-run skips it. This is the missing half of the remote→local
/// sync (ingest records metadata; this writes the actual files).
///
/// A file already on disk with a matching size + mtime is skipped (no
/// re-download), so a `410` resync that re-marks everything `remote_dirty`
/// doesn't re-fetch the whole drive. A content-hash match would be definitive;
/// that's a follow-up.
pub fn materialize_downloads<D: Downloader>(
    store: &Store,
    downloader: &D,
    account: &str,
    sync_root: &Path,
    host: &str,
) -> Result<MaterializeReport, SyncError> {
    let items = store.items_by_service(account, SERVICE)?;
    let by_id: HashMap<&str, &Item> = items.iter().map(|i| (i.remote_id.as_str(), i)).collect();
    let mut report = MaterializeReport::default();

    // Folders first (create the tree), then files, so a file's parent exists.
    for pass_folders in [true, false] {
        for it in &items {
            let is_folder = it.item_type == "folder";
            if is_folder != pass_folders || it.sync_state != "remote_dirty" {
                continue;
            }
            let rel = match local_rel_path(&by_id, it) {
                Some(p) => p,
                None => {
                    report.failed += 1;
                    continue;
                }
            };
            let full = sync_root.join(&rel);
            if is_folder {
                match std::fs::create_dir_all(&full) {
                    Ok(()) => {
                        store.set_sync_state(account, SERVICE, &it.remote_id, "clean")?;
                        report.dirs_created += 1;
                    }
                    Err(_) => report.failed += 1,
                }
            } else if local_file_matches(&full, it) {
                // already on disk with the same size + mtime — skip the download
                // (e.g. a 410 resync re-marked everything remote_dirty).
                store.set_sync_state(account, SERVICE, &it.remote_id, "clean")?;
                record_synced_state(
                    store,
                    account,
                    &it.remote_id,
                    &full,
                    it.quickxorhash.clone(),
                );
                report.skipped += 1;
            } else {
                // Keep-both on the download path too (plan §10): a locally-edited
                // file must never be clobbered by a newer cloud version. "Locally
                // edited" = the on-disk file differs from the last-synced reference
                // (size → mtime → QuickXorHash ladder). The item's own metadata
                // cannot be used here — the delta ingest already overwrote it with
                // the NEW remote values. Items without a reference (pre-v8 stores,
                // never-synced) keep the plain overwrite rather than spraying a
                // conflict copy for every ordinary remote update.
                if let Some((ssize, smtime, shash)) =
                    store.get_synced_state(account, SERVICE, &it.remote_id)?
                {
                    if locally_edited_since_sync(&full, ssize, smtime, shash.as_deref()) {
                        let dir = full
                            .parent()
                            .map(Path::to_path_buf)
                            .unwrap_or_else(|| sync_root.to_path_buf());
                        let fname = full.file_name().and_then(|s| s.to_str()).unwrap_or("file");
                        let copy = unique_conflict_copy(&dir, fname, host);
                        if std::fs::rename(&full, dir.join(&copy)).is_ok() {
                            report.conflicts += 1;
                        }
                    }
                }
                if let Some(parent) = full.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match downloader.download(&it.remote_id) {
                    Ok(bytes) => match atomic_write(&full, &bytes) {
                        Ok(()) => {
                            // mirror the cloud's last-modified time onto the local file
                            if let Some(mt) = &it.remote_mtime {
                                set_file_mtime(&full, mt);
                            }
                            store.set_sync_state(account, SERVICE, &it.remote_id, "clean")?;
                            let hash = Some(crate::quickxor::quickxor_base64(&bytes));
                            record_synced_state(store, account, &it.remote_id, &full, hash);
                            report.downloaded += 1;
                        }
                        Err(_) => report.failed += 1,
                    },
                    Err(_) => report.failed += 1,
                }
            }
        }
    }
    Ok(report)
}

/// True if the offline scope set owns `remote_id` — its deepest active ancestor is one
/// of `offline_ids` ([`owning_scope`] over the store ancestry). The scope-ownership
/// filter for Mode-3 offline materialize + writeback (#655 / S-OM.9).
fn item_in_offline(
    store: &Store,
    account: &str,
    remote_id: &str,
    offline_ids: &BTreeSet<&str>,
) -> Result<bool, SyncError> {
    let ancestry = store_ancestry(store, account, remote_id)?;
    let refs: Vec<&str> = ancestry.iter().map(String::as_str).collect();
    Ok(owning_scope(&refs, offline_ids).is_some())
}

/// The sync-root-relative local paths of the offline scope-root folders, so an untracked
/// local **create** can be attributed to an offline scope by path prefix (a create has no
/// store row, so [`item_in_offline`] can't be used on it).
fn offline_scope_prefixes(
    store: &Store,
    account: &str,
    offline_ids: &BTreeSet<&str>,
) -> Result<Vec<PathBuf>, SyncError> {
    let items = store.all_items_by_service(account, SERVICE)?;
    let by_id: HashMap<&str, &Item> = items.iter().map(|i| (i.remote_id.as_str(), i)).collect();
    let mut prefixes = Vec::new();
    for id in offline_ids {
        if let Some(it) = by_id.get(*id) {
            if let Some(rel) = local_rel_path(&by_id, it) {
                prefixes.push(rel);
            }
        }
    }
    Ok(prefixes)
}

/// Scoped, policy-gated, progress-reported materialize for Mode-3 **offline** folders
/// (#655 / S-OM.9). Like [`materialize_downloads`], but: (1) only items an `offline`
/// scope owns are written (deepest-active-ancestor rule, [`owning_scope`]); (2)
/// [`isyncyou_core::policy::evaluate`] gates each NEW download on the storage floor /
/// network / power policy — a `Blocked` verdict stops new downloads and leaves existing
/// files untouched; (3) per-file progress is reported to `progress`; (4) each item's v14
/// content-state columns are marked `materialized`/`sync`/`available` (+ `materialized_at
/// = now`) so the mobile body endpoint (`has_body == body_state=='available'`) serves it,
/// with a `downloading` checkpoint before the body write for crash recovery (AC3). The
/// desktop [`materialize_downloads`] is intentionally left unchanged.
#[allow(clippy::too_many_arguments)]
pub fn materialize_downloads_scoped<D: Downloader>(
    store: &Store,
    downloader: &D,
    account: &str,
    sync_root: &Path,
    host: &str,
    now: &str,
    offline_ids: &BTreeSet<&str>,
    cfg_sync: &isyncyou_core::SyncConfig,
    dev: &isyncyou_core::policy::DeviceState,
    progress: &dyn ProgressSink,
) -> Result<MaterializeReport, SyncError> {
    let items = store.items_by_service(account, SERVICE)?;
    let by_id: HashMap<&str, &Item> = items.iter().map(|i| (i.remote_id.as_str(), i)).collect();
    let mut report = MaterializeReport::default();

    // Folders first (create the tree), then files, so a file's parent exists.
    for pass_folders in [true, false] {
        for it in &items {
            let is_folder = it.item_type == "folder";
            if is_folder != pass_folders || it.sync_state != "remote_dirty" {
                continue;
            }
            // Scope filter: only items an offline scope owns (deepest active ancestor).
            if !item_in_offline(store, account, &it.remote_id, offline_ids)? {
                continue;
            }
            let rel = match local_rel_path(&by_id, it) {
                Some(p) => p,
                None => {
                    report.failed += 1;
                    continue;
                }
            };
            let full = sync_root.join(&rel);
            if is_folder {
                match std::fs::create_dir_all(&full) {
                    Ok(()) => {
                        store.set_sync_state(account, SERVICE, &it.remote_id, "clean")?;
                        report.dirs_created += 1;
                    }
                    Err(_) => report.failed += 1,
                }
            } else if local_file_matches(&full, it) {
                // Already on disk with the same size + mtime — skip the download but still
                // mark the content-state so `has_body` is true for the mobile body endpoint.
                store.set_sync_state(account, SERVICE, &it.remote_id, "clean")?;
                record_synced_state(
                    store,
                    account,
                    &it.remote_id,
                    &full,
                    it.quickxorhash.clone(),
                );
                store.set_content_state(
                    account,
                    SERVICE,
                    &it.remote_id,
                    Some("materialized"),
                    Some("sync"),
                    Some("available"),
                    Some(now),
                )?;
                report.skipped += 1;
            } else {
                // User cancelled this transfer before it started (best-effort, queue-deep):
                // consume the one-shot request and skip it. The body stays `missing` (not
                // touched), and a later pass re-materializes it since the cancel is consumed.
                if progress.is_cancelled(&it.remote_id) {
                    progress.consume_cancel(&it.remote_id);
                    report.cancelled += 1;
                    continue;
                }
                // Policy gate: a Blocked verdict stops NEW downloads (existing files stay).
                // The body is left `missing` (not `failed`) — it is simply not fetched yet.
                if !isyncyou_core::policy::evaluate(cfg_sync, dev).is_allowed() {
                    store.set_content_state(
                        account,
                        SERVICE,
                        &it.remote_id,
                        None,
                        Some("sync"),
                        Some("missing"),
                        None,
                    )?;
                    report.failed += 1;
                    continue;
                }
                // Download-path keep-both (plan §10): a locally-edited file must never be
                // clobbered by a newer cloud version. Same reference ladder as the desktop
                // path; items without a reference keep the plain overwrite.
                if let Some((ssize, smtime, shash)) =
                    store.get_synced_state(account, SERVICE, &it.remote_id)?
                {
                    if locally_edited_since_sync(&full, ssize, smtime, shash.as_deref()) {
                        let dir = full
                            .parent()
                            .map(Path::to_path_buf)
                            .unwrap_or_else(|| sync_root.to_path_buf());
                        let fname = full.file_name().and_then(|s| s.to_str()).unwrap_or("file");
                        let copy = unique_conflict_copy(&dir, fname, host);
                        if std::fs::rename(&full, dir.join(&copy)).is_ok() {
                            report.conflicts += 1;
                        }
                    }
                }
                if let Some(parent) = full.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let name = rel.file_name().and_then(|s| s.to_str()).unwrap_or("file");
                let total = it.size.unwrap_or(0).max(0) as u64;
                progress.begin(&it.remote_id, name, total);
                // Checkpoint the body as `downloading` before the write, so a crash
                // mid-materialize is recoverable and a re-run is idempotent (AC3).
                store.set_content_state(
                    account,
                    SERVICE,
                    &it.remote_id,
                    None,
                    Some("sync"),
                    Some("downloading"),
                    None,
                )?;
                // Stream the body and report cumulative bytes as they arrive (#656 F-C), so the
                // transfer panel shows a moving bar instead of sitting at 0% until the file lands.
                let dl = downloader.download_with_progress(&it.remote_id, &mut |done| {
                    progress.advance(&it.remote_id, done);
                });
                match dl {
                    Ok(bytes) => match atomic_write(&full, &bytes) {
                        Ok(()) => {
                            if let Some(mt) = &it.remote_mtime {
                                set_file_mtime(&full, mt);
                            }
                            store.set_sync_state(account, SERVICE, &it.remote_id, "clean")?;
                            let hash = Some(crate::quickxor::quickxor_base64(&bytes));
                            record_synced_state(store, account, &it.remote_id, &full, hash);
                            store.set_content_state(
                                account,
                                SERVICE,
                                &it.remote_id,
                                Some("materialized"),
                                Some("sync"),
                                Some("available"),
                                Some(now),
                            )?;
                            report.downloaded += 1;
                        }
                        Err(_) => {
                            store.set_content_state(
                                account,
                                SERVICE,
                                &it.remote_id,
                                None,
                                Some("sync"),
                                Some("failed"),
                                None,
                            )?;
                            report.failed += 1;
                        }
                    },
                    Err(_) => {
                        store.set_content_state(
                            account,
                            SERVICE,
                            &it.remote_id,
                            None,
                            Some("sync"),
                            Some("failed"),
                            None,
                        )?;
                        report.failed += 1;
                    }
                }
                progress.finish(&it.remote_id);
            }
        }
    }
    Ok(report)
}

/// A tombstoned item whose local file/dir still exists and should be removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingLocalDelete {
    pub remote_id: String,
    /// Path relative to the sync root.
    pub rel: PathBuf,
}

/// Find OneDrive items tombstoned in the store whose local file/dir still exists
/// under `sync_root` — the pending remote→local deletions. The caller applies the
/// mass-delete guard (it owns the config) before calling [`apply_local_deletes`].
pub fn pending_local_deletes(
    store: &Store,
    account: &str,
    sync_root: &Path,
) -> Result<Vec<PendingLocalDelete>, SyncError> {
    let items = store.all_items_by_service(account, SERVICE)?;
    let by_id: HashMap<&str, &Item> = items.iter().map(|i| (i.remote_id.as_str(), i)).collect();
    let mut out = Vec::new();
    for it in &items {
        if it.deleted_at.is_none() {
            continue;
        }
        let rel = match local_rel_path(&by_id, it) {
            Some(p) => p,
            None => continue,
        };
        if sync_root.join(&rel).exists() {
            out.push(PendingLocalDelete {
                remote_id: it.remote_id.clone(),
                rel,
            });
        }
    }
    Ok(out)
}

/// Mirror remote deletions locally **without destroying data**: move each path
/// into `trash_root`, preserving its layout (plan §9.3 / A9 — the trash lives
/// outside the sync root). A path already gone (e.g. removed with its parent
/// folder) is skipped. Returns how many were moved.
pub fn apply_local_deletes(
    sync_root: &Path,
    trash_root: &Path,
    deletes: &[PendingLocalDelete],
) -> Result<usize, SyncError> {
    let mut moved = 0;
    for d in deletes {
        let src = sync_root.join(&d.rel);
        if !src.exists() {
            continue; // already moved along with an ancestor folder
        }
        let dst = trash_root.join(&d.rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::rename(&src, &dst) {
            Ok(()) => moved += 1,
            // cross-device fallback for files (rare); leave dirs for the next pass.
            Err(_) if src.is_file() => {
                std::fs::copy(&src, &dst)?;
                std::fs::remove_file(&src)?;
                moved += 1;
            }
            Err(e) => return Err(SyncError::Io(e)),
        }
    }
    Ok(moved)
}

/// Recursively collect every regular file under `root` (skips our own
/// `*.isync-tmp` scratch files). Returns absolute paths.
fn walk_local_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.is_file() && !p.to_string_lossy().ends_with(".isync-tmp") {
                out.push(p);
            }
        }
    }
    out
}

/// Build a cloud destination path from a sync-root-relative local path, encoding
/// each segment to a OneDrive-safe name (reverse of the cloud→local mapping).
///
/// Public so the ledger-backed offline writeback (S-OM.9 / #655) can record a stable
/// upload target (the dest path) in the operation ledger and recover it after a crash.
pub fn cloud_dest_path(rel: &Path) -> String {
    let mut s = String::new();
    for comp in rel.components() {
        if let std::path::Component::Normal(os) = comp {
            s.push('/');
            s.push_str(&isyncyou_pathmap::to_cloud(&os.to_string_lossy()));
        }
    }
    s
}

/// Find files under `sync_root` that the store doesn't track yet — local
/// **creates** to push to the cloud. A file matching a tracked item (even an
/// un-materialized `remote_dirty` one) is *not* a create; that keeps a brand-new
/// local file from being confused with a not-yet-downloaded remote file.
/// (Local *modifies* and *deletes* are handled separately — they need If-Match
/// conflict handling and the mass-delete guard, respectively.)
pub fn scan_local_creates(
    store: &Store,
    account: &str,
    sync_root: &Path,
) -> Result<Vec<PathBuf>, SyncError> {
    let items = store.all_items_by_service(account, SERVICE)?;
    let by_id: HashMap<&str, &Item> = items.iter().map(|i| (i.remote_id.as_str(), i)).collect();
    let mut tracked: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for it in &items {
        if it.deleted_at.is_some() {
            continue;
        }
        if let Some(rel) = local_rel_path(&by_id, it) {
            tracked.insert(rel);
        }
    }
    let mut creates = Vec::new();
    for full in walk_local_files(sync_root) {
        if let Ok(rel) = full.strip_prefix(sync_root) {
            if !tracked.contains(rel) {
                creates.push(rel.to_path_buf());
            }
        }
    }
    Ok(creates)
}

/// Local **creates** restricted to Mode-3 **offline** scopes (#655 / S-OM.9): an
/// untracked file counts only if it lives under an offline scope-root folder's local
/// path. A create has no store row, so it is attributed by path prefix rather than
/// [`item_in_offline`].
pub fn scan_local_creates_scoped(
    store: &Store,
    account: &str,
    sync_root: &Path,
    offline_ids: &BTreeSet<&str>,
) -> Result<Vec<PathBuf>, SyncError> {
    let prefixes = offline_scope_prefixes(store, account, offline_ids)?;
    let all = scan_local_creates(store, account, sync_root)?;
    Ok(all
        .into_iter()
        .filter(|rel| prefixes.iter().any(|p| rel.starts_with(p)))
        .collect())
}

/// Local **modifies** restricted to offline scopes (#655): like [`scan_local_modifies`]
/// but only for items an offline scope owns.
pub fn scan_local_modifies_scoped(
    store: &Store,
    account: &str,
    sync_root: &Path,
    offline_ids: &BTreeSet<&str>,
) -> Result<Vec<(String, PathBuf, String)>, SyncError> {
    let mut out = Vec::new();
    for m in scan_local_modifies(store, account, sync_root)? {
        if item_in_offline(store, account, &m.0, offline_ids)? {
            out.push(m);
        }
    }
    Ok(out)
}

/// Local **deletes** restricted to offline scopes (#655): like [`scan_local_deletes`]
/// but only for items an offline scope owns.
pub fn scan_local_deletes_scoped(
    store: &Store,
    account: &str,
    sync_root: &Path,
    offline_ids: &BTreeSet<&str>,
) -> Result<Vec<String>, SyncError> {
    let mut out = Vec::new();
    for id in scan_local_deletes(store, account, sync_root)? {
        if item_in_offline(store, account, &id, offline_ids)? {
            out.push(id);
        }
    }
    Ok(out)
}

/// Push local-create files up to the cloud: read each, upload to its encoded
/// cloud path, and ingest the created item into the store (as `clean`). Returns
/// how many were uploaded.
pub fn push_local_creates<W: RemoteWriter>(
    writer: &W,
    store: &Store,
    map: &mut MappingTable,
    account: &str,
    sync_root: &Path,
    creates: &[PathBuf],
) -> Result<usize, SyncError> {
    let mut uploaded = 0;
    for rel in creates {
        let full = sync_root.join(rel);
        // Decrypt the local body before upload — the on-disk copy is a sealed envelope on
        // mobile, and the cloud must receive the plaintext, never ciphertext (#0B).
        let data = isyncyou_core::envelope::read_body(&full)?;
        let dest = cloud_dest_path(rel);
        // preserve the file's local mtime on the cloud item (best-effort)
        let local_mtime = local_mtime_secs(&full);
        let id = push_upload(writer, store, map, account, &dest, &data, local_mtime)?;
        // the uploaded bytes ARE the on-disk state — record the synced reference
        let hash = Some(crate::quickxor::quickxor_base64(&data));
        record_synced_state(store, account, &id, &full, hash);
        uploaded += 1;
    }
    Ok(uploaded)
}

/// Find tracked files that were materialized (`clean`) but whose local copy is
/// now **missing** — local **deletes** to mirror to the cloud. Only `clean`
/// items qualify, so a not-yet-downloaded `remote_dirty` file (never on disk) is
/// never mistaken for a local deletion. Returns the affected `remote_id`s; the
/// caller applies the mass-delete guard before pushing the deletions.
pub fn scan_local_deletes(
    store: &Store,
    account: &str,
    sync_root: &Path,
) -> Result<Vec<String>, SyncError> {
    let items = store.all_items_by_service(account, SERVICE)?;
    let by_id: HashMap<&str, &Item> = items.iter().map(|i| (i.remote_id.as_str(), i)).collect();
    let mut out = Vec::new();
    for it in &items {
        if it.deleted_at.is_some() || it.item_type != "file" || it.sync_state != "clean" {
            continue;
        }
        let rel = match local_rel_path(&by_id, it) {
            Some(p) => p,
            None => continue,
        };
        if !sync_root.join(&rel).exists() {
            out.push(it.remote_id.clone());
        }
    }
    Ok(out)
}

/// Replaces an item's content only if its etag still matches (so a concurrent
/// cloud change is never silently overwritten — A3). Abstracted so the modify
/// driver is unit-testable with a mock. `Ok(Some(item))` = replaced;
/// `Ok(None)` = conflict (cloud changed, not overwritten).
pub trait ContentReplacer {
    fn replace_if_match(
        &self,
        item_id: &str,
        data: &[u8],
        etag: &str,
    ) -> Result<Option<Value>, String>;
    /// Set the item's `fileSystemInfo.lastModifiedDateTime` (preserve the local
    /// mtime after a modify upload), returning the updated item. Default no-op
    /// (`Value::Null`) so mocks need no change.
    fn set_mtime(&self, _item_id: &str, _rfc3339: &str) -> Result<Value, String> {
        Ok(Value::Null)
    }
}

#[cfg(feature = "http")]
impl ContentReplacer for isyncyou_graph::GraphClient {
    fn replace_if_match(
        &self,
        item_id: &str,
        data: &[u8],
        etag: &str,
    ) -> Result<Option<Value>, String> {
        self.replace_content_if_match(item_id, data, etag)
            .map_err(|e| e.to_string())
    }
    fn set_mtime(&self, item_id: &str, rfc3339: &str) -> Result<Value, String> {
        self.patch_json(
            &format!("/me/drive/items/{item_id}"),
            &serde_json::json!({ "fileSystemInfo": { "lastModifiedDateTime": rfc3339 } }),
        )
        .map_err(|e| e.to_string())
    }
}

/// What one local→remote modify pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModifyReport {
    pub uploaded: usize,
    /// Cloud also changed since the last sync — reported, never overwritten.
    pub conflicts: usize,
    pub failed: usize,
}

/// Find tracked files whose on-disk size differs from the stored size — local
/// **modifies**. Size-only: a same-size in-place edit is missed (a v1 limit),
/// but there are no false positives (a freshly downloaded file matches its stored
/// size). Returns `(remote_id, rel, etag)`; an item without a stored etag is
/// skipped (the conditional replace needs one).
pub fn scan_local_modifies(
    store: &Store,
    account: &str,
    sync_root: &Path,
) -> Result<Vec<(String, PathBuf, String)>, SyncError> {
    let items = store.all_items_by_service(account, SERVICE)?;
    let by_id: HashMap<&str, &Item> = items.iter().map(|i| (i.remote_id.as_str(), i)).collect();
    let mut out = Vec::new();
    for it in &items {
        if it.deleted_at.is_some() || it.item_type != "file" {
            continue;
        }
        let rel = match local_rel_path(&by_id, it) {
            Some(p) => p,
            None => continue,
        };
        if is_local_modified(&sync_root.join(&rel), it) {
            if let Some(etag) = it.etag.clone() {
                out.push((it.remote_id.clone(), rel, etag));
            }
        }
    }
    Ok(out)
}

/// Decide whether a tracked file's on-disk copy differs from the cloud version,
/// cheapest check first (the rclone-style ladder):
/// 1. size+mtime match → unchanged (no read);
/// 2. size differs → modified;
/// 3. size matches but mtime differs → confirm by content hash (QuickXorHash), so
///    a same-size in-place edit is caught while a pure mtime touch is not a false
///    modify. Without a stored hash this falls back to the size verdict
///    (unchanged), so it never regresses on synthetic items.
fn is_local_modified(full: &Path, it: &Item) -> bool {
    if std::fs::metadata(full).is_err() {
        return false; // not on disk → not a modify (delete is handled elsewhere)
    }
    if local_file_matches(full, it) {
        return false;
    }
    // Plaintext size (mobile bodies are sealed envelopes larger than the plaintext).
    if it.size != Some(isyncyou_core::envelope::on_disk_plaintext_len(full) as i64) {
        return true;
    }
    // size matches but mtime differs: only a content hash can decide.
    match (&it.quickxorhash, isyncyou_core::envelope::read_body(full)) {
        (Some(stored), Ok(data)) => crate::quickxor::quickxor_base64(&data) != *stored,
        _ => false,
    }
}

/// Upload local modifies with an If-Match guard: replace the cloud content only
/// if its etag still matches, so a concurrent cloud change is reported as a
/// conflict and **never silently overwritten** (A3). On success the store item is
/// refreshed (new size/etag, `clean`).
/// Build an abraunegg-style `safeBackup` conflict-copy file name, e.g.
/// `report-laptop-safeBackup-0001.txt`. Replicated here (not pulled from
/// `core::conflict`) to keep `connectors` dependency-light.
fn conflict_copy_name(fname: &str, host: &str, n: u32) -> String {
    let (stem, ext) = match fname.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (fname, String::new()),
    };
    format!("{stem}-{host}-safeBackup-{n:04}{ext}")
}

/// First conflict-copy name in `dir` that doesn't already exist (so repeated
/// conflicts on the same file never clobber an earlier copy).
fn unique_conflict_copy(dir: &Path, fname: &str, host: &str) -> String {
    (1..=9999)
        .map(|n| conflict_copy_name(fname, host, n))
        .find(|name| !dir.join(name).exists())
        .unwrap_or_else(|| conflict_copy_name(fname, host, 9999))
}

pub fn apply_local_modifies<R: ContentReplacer>(
    replacer: &R,
    store: &Store,
    map: &mut MappingTable,
    account: &str,
    sync_root: &Path,
    host: &str,
    modifies: &[(String, PathBuf, String)],
) -> Result<ModifyReport, SyncError> {
    let mut report = ModifyReport::default();
    for (id, rel, etag) in modifies {
        // Decrypt the local body before re-upload — never send ciphertext to the cloud (#0B).
        let data = match isyncyou_core::envelope::read_body(&sync_root.join(rel)) {
            Ok(d) => d,
            Err(_) => {
                report.failed += 1;
                continue;
            }
        };
        match replacer.replace_if_match(id, &data, etag) {
            Ok(Some(item)) => {
                // Preserve the file's local mtime on the cloud item (best-effort);
                // ingest the timestamp-corrected item when set_mtime returns one.
                let to_ingest = match local_mtime_secs(&sync_root.join(rel)) {
                    Some(secs) => match replacer.set_mtime(id, &unix_to_rfc3339(secs)) {
                        Ok(updated) if !updated.is_null() => updated,
                        _ => item,
                    },
                    None => item,
                };
                ingest_item(store, map, account, &to_ingest, "", "clean", None)?;
                // the uploaded bytes ARE the on-disk state — record the reference
                let hash = Some(crate::quickxor::quickxor_base64(&data));
                record_synced_state(store, account, id, &sync_root.join(rel), hash);
                report.uploaded += 1;
            }
            Ok(None) => {
                // Keep both (plan §10, headless default): the cloud changed, so we
                // never overwrite it. Move the local edit aside as a conflict copy
                // (picked up as a new file -> uploaded next pass) and re-mark the
                // item remote_dirty so the cloud version re-downloads to the original
                // path. This also breaks the re-conflict loop the old "report only"
                // path caused (the modified file would re-conflict every sync).
                let src = sync_root.join(rel);
                let dir = src
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| sync_root.to_path_buf());
                let fname = rel.file_name().and_then(|s| s.to_str()).unwrap_or("file");
                let copy = unique_conflict_copy(&dir, fname, host);
                match std::fs::rename(&src, dir.join(&copy)) {
                    Ok(()) => {
                        store.set_sync_state(account, SERVICE, id, "remote_dirty")?;
                        report.conflicts += 1;
                    }
                    Err(_) => report.failed += 1,
                }
            }
            Err(_) => report.failed += 1,
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use isyncyou_graph::client::Response;
    use serde_json::json;

    struct MockTransport(Vec<Response>, usize);
    impl Transport for MockTransport {
        fn get(&mut self, _url: &str) -> Response {
            let r = self.0[self.1].clone();
            self.1 += 1;
            r
        }
    }

    fn file_item(id: &str, name: &str, parent: &str) -> Value {
        json!({
            "id": id,
            "name": name,
            "parentReference": { "id": parent },
            "size": 42,
            "eTag": "etag1",
            "cTag": "ctag1",
            "file": { "hashes": { "quickXorHash": "QXH==" } },
            "fileSystemInfo": { "lastModifiedDateTime": "2024-01-02T03:04:05Z" }
        })
    }

    #[test]
    fn ingests_files_folders_and_tombstones() {
        let store = Store::open_in_memory().unwrap();
        let arch = tempfile::tempdir().unwrap();
        let mut map = MappingTable::new();
        // an item that already existed and will be tombstoned by the delta
        store
            .upsert_item(&Item::new("acc", SERVICE, "gone1", "old.txt", "file"))
            .unwrap();
        // seed its metadata sidecar so we can prove the tombstone drops it
        write_item_json(arch.path(), "gone1", &json!({ "id": "gone1" })).unwrap();
        assert!(shard_path(arch.path(), SERVICE, "gone1", "json").exists());
        let page = json!({
            "value": [
                { "id": "root1", "root": {}, "name": "root" },
                { "id": "F1", "name": "Photos", "parentReference": {"id": "root1"}, "folder": {"childCount": 1} },
                file_item("a1", "IMG.jpg", "F1"),
                { "id": "gone1", "deleted": { "state": "deleted" } }
            ],
            "@odata.deltaLink": "CURSOR1"
        });
        let mut t = MockTransport(vec![Response::ok(page)], 0);

        let report = incremental_sync(
            &mut t,
            &store,
            &mut map,
            "acc",
            "2026-06-02T00:00:00Z",
            arch.path(),
        )
        .unwrap();
        assert_eq!(report.upserted, 2); // folder + file
        assert_eq!(report.deleted, 1);
        assert_eq!(report.skipped, 1); // root

        let file = store.get_item("acc", SERVICE, "a1").unwrap().unwrap();
        assert_eq!(file.name, "IMG.jpg");
        assert_eq!(file.parent_remote_id.as_deref(), Some("F1"));
        assert_eq!(file.quickxorhash.as_deref(), Some("QXH=="));
        assert_eq!(file.size, Some(42));
        assert_eq!(file.remote_mtime.as_deref(), Some("2024-01-02T03:04:05Z"));
        assert_eq!(file.sync_state, "remote_dirty");

        // the metadata sidecar archives the full DriveItem JSON straight from the
        // delta — the file sidecar round-trips, and the folder sidecar keeps a
        // facet (folder.childCount) the indexed columns drop (#564 AC-1).
        let raw = std::fs::read(shard_path(arch.path(), SERVICE, "a1", "json")).unwrap();
        let v: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v, file_item("a1", "IMG.jpg", "F1"));
        let fraw = std::fs::read(shard_path(arch.path(), SERVICE, "F1", "json")).unwrap();
        let fv: Value = serde_json::from_slice(&fraw).unwrap();
        assert_eq!(
            fv.pointer("/folder/childCount").and_then(Value::as_i64),
            Some(1)
        );

        // tombstone recorded
        assert!(store
            .get_item("acc", SERVICE, "gone1")
            .unwrap()
            .unwrap()
            .deleted_at
            .is_some());
        // tombstone dropped the metadata sidecar (#564 AC-2)
        assert!(!shard_path(arch.path(), SERVICE, "gone1", "json").exists());
        // cursor persisted
        assert_eq!(
            store
                .get_delta_cursor("acc", SERVICE, "")
                .unwrap()
                .as_deref(),
            Some("CURSOR1")
        );
    }

    #[test]
    fn second_run_uses_persisted_cursor_and_paginates() {
        let store = Store::open_in_memory().unwrap();
        let arch = tempfile::tempdir().unwrap();
        let mut map = MappingTable::new();
        // first run sets a cursor
        let p1 = json!({ "value": [file_item("a1","a.txt","root1")], "@odata.deltaLink": "C1" });
        let mut t1 = MockTransport(vec![Response::ok(p1)], 0);
        incremental_sync(&mut t1, &store, &mut map, "acc", "t", arch.path()).unwrap();
        // second run: two pages, then deltaLink
        let p2a = json!({ "value": [file_item("b1","b.txt","root1")], "@odata.nextLink": "u2" });
        let p2b = json!({ "value": [file_item("c1","c.txt","root1")], "@odata.deltaLink": "C2" });
        let mut t2 = MockTransport(vec![Response::ok(p2a), Response::ok(p2b)], 0);
        let r = incremental_sync(&mut t2, &store, &mut map, "acc", "t", arch.path()).unwrap();
        assert_eq!(r.upserted, 2);
        assert_eq!(
            store
                .get_delta_cursor("acc", SERVICE, "")
                .unwrap()
                .as_deref(),
            Some("C2")
        );
    }

    // ---- Scoped per-folder delta tests (S-OM.7) ----

    use crate::scope::Mode;
    use std::collections::VecDeque;

    /// URL-sensitive mock: routes a request to the first key that is a substring of the
    /// requested url, popping that route's queued responses in order. (The plain
    /// `MockTransport` ignores the url and cannot drive multiple scopes or a resume.)
    struct MockScopedTransport {
        routes: Vec<(String, VecDeque<Response>)>,
    }
    impl MockScopedTransport {
        fn new(routes: Vec<(&str, Vec<Response>)>) -> Self {
            MockScopedTransport {
                routes: routes
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.into_iter().collect()))
                    .collect(),
            }
        }
    }
    impl Transport for MockScopedTransport {
        fn get(&mut self, url: &str) -> Response {
            for (key, q) in self.routes.iter_mut() {
                if url.contains(key.as_str()) {
                    if let Some(r) = q.pop_front() {
                        return r;
                    }
                }
            }
            panic!("MockScopedTransport: no queued response for url {url}");
        }
    }

    fn seed_folder(store: &Store, id: &str, parent: Option<&str>) {
        let mut it = Item::new("acc", SERVICE, id, id, "folder");
        it.parent_remote_id = parent.map(String::from);
        store.upsert_item(&it).unwrap();
    }
    fn removed(id: &str) -> Value {
        json!({ "id": id, "@removed": { "reason": "deleted" } })
    }
    fn sync_scope(id: &str) -> FolderScope {
        FolderScope {
            folder_id: id.to_string(),
            mode: Mode::Sync,
        }
    }
    fn offline_scope(id: &str) -> FolderScope {
        FolderScope {
            folder_id: id.to_string(),
            mode: Mode::Offline,
        }
    }

    /// AC1 (a): a real delete deep inside a scope's subtree is tombstoned. The old mail
    /// rule (`parent_remote_id == folder_id`) would have skipped it — the nested item's
    /// parent is the subfolder, not the scope root — leaking the delete. `owning_scope`
    /// over the whole subtree fixes it. This is the regression guard for that leak.
    #[test]
    fn ac1_nested_delete_inside_scope_is_tombstoned() {
        let store = Store::open_in_memory().unwrap();
        let arch = tempfile::tempdir().unwrap();
        let mut map = MappingTable::new();
        // S (scope root) / U (nested subfolder) / X (file, parent = U, NOT S)
        seed_folder(&store, "S", Some("root1"));
        seed_folder(&store, "U", Some("S"));
        let mut x = Item::new("acc", SERVICE, "X", "x.txt", "file");
        x.parent_remote_id = Some("U".into());
        store.upsert_item(&x).unwrap();

        let mut t = MockScopedTransport::new(vec![(
            "items/S/delta",
            vec![Response::ok(json!({
                "value": [ removed("X") ],
                "@odata.deltaLink": "CS"
            }))],
        )]);
        let report = incremental_sync_scoped(
            &mut t,
            &store,
            &mut map,
            "acc",
            "2026-06-02T00:00:00Z",
            arch.path(),
            &[sync_scope("S")],
        )
        .unwrap();

        assert_eq!(report.deleted, 1, "nested delete must tombstone, not leak");
        assert!(store
            .get_item("acc", SERVICE, "X")
            .unwrap()
            .unwrap()
            .deleted_at
            .is_some());
        assert_eq!(
            store
                .get_delta_cursor("acc", SERVICE, "S")
                .unwrap()
                .as_deref(),
            Some("CS")
        );
    }

    /// AC1 (b): a cross-scope move (out of the shallower scope P, into the deeper scope
    /// C) is not clobbered. Deepest-first processing writes the move-in first; P's later
    /// `@removed` resolves ownership to C and is skipped. The item stays alive, id-stable.
    #[test]
    fn ac1_cross_scope_move_is_not_clobbered() {
        let store = Store::open_in_memory().unwrap();
        let arch = tempfile::tempdir().unwrap();
        let mut map = MappingTable::new();
        seed_folder(&store, "P", Some("root1"));
        seed_folder(&store, "C", Some("P"));
        let mut x = Item::new("acc", SERVICE, "X", "x.txt", "file");
        x.parent_remote_id = Some("P".into());
        store.upsert_item(&x).unwrap();

        let mut t = MockScopedTransport::new(vec![
            (
                "items/C/delta",
                vec![Response::ok(json!({
                    "value": [ file_item("X", "x.txt", "C") ],
                    "@odata.deltaLink": "CC"
                }))],
            ),
            (
                "items/P/delta",
                vec![Response::ok(json!({
                    "value": [ removed("X") ],
                    "@odata.deltaLink": "CP"
                }))],
            ),
        ]);
        let report = incremental_sync_scoped(
            &mut t,
            &store,
            &mut map,
            "acc",
            "t",
            arch.path(),
            &[sync_scope("P"), offline_scope("C")],
        )
        .unwrap();

        let x = store.get_item("acc", SERVICE, "X").unwrap().unwrap();
        assert!(x.deleted_at.is_none(), "moved item must stay alive");
        assert_eq!(
            x.parent_remote_id.as_deref(),
            Some("C"),
            "id-stable reparent"
        );
        assert_eq!(report.deleted, 0);
        assert_eq!(
            report.skipped, 1,
            "P's @removed skipped (owned by deeper C)"
        );
        assert_eq!(report.upserted, 1, "C claimed the move-in");
    }

    /// AC2 (part 1): the cursor is persisted after *every* page (a `nextLink` becomes
    /// the resume point) and the walk resumes from it. Walk 1 ingests page 1 then hits a
    /// fatal on page 2 → the stored cursor is the page-1 `nextLink`. Walk 2 resumes there
    /// and finishes at the `deltaLink`.
    #[test]
    fn ac2_persists_nextlink_per_page_and_resumes() {
        let store = Store::open_in_memory().unwrap();
        let arch = tempfile::tempdir().unwrap();
        let mut map = MappingTable::new();
        seed_folder(&store, "S", Some("root1"));

        let mut t1 = MockScopedTransport::new(vec![
            (
                "items/S/delta",
                vec![Response::ok(json!({
                    "value": [ file_item("A", "a.txt", "S") ],
                    "@odata.nextLink": "https://graph/next/u2"
                }))],
            ),
            ("next/u2", vec![Response::status(400)]),
        ]);
        let err = incremental_sync_scoped(
            &mut t1,
            &store,
            &mut map,
            "acc",
            "t",
            arch.path(),
            &[sync_scope("S")],
        );
        assert!(err.is_err(), "fatal on page 2 aborts the walk");
        // page 1 was ingested and its nextLink persisted as the resume point
        assert!(store.get_item("acc", SERVICE, "A").unwrap().is_some());
        assert_eq!(
            store
                .get_delta_cursor("acc", SERVICE, "S")
                .unwrap()
                .as_deref(),
            Some("https://graph/next/u2")
        );

        let mut t2 = MockScopedTransport::new(vec![(
            "next/u2",
            vec![Response::ok(json!({
                "value": [ file_item("B", "b.txt", "S") ],
                "@odata.deltaLink": "CS"
            }))],
        )]);
        let report = incremental_sync_scoped(
            &mut t2,
            &store,
            &mut map,
            "acc",
            "t",
            arch.path(),
            &[sync_scope("S")],
        )
        .unwrap();
        assert!(store.get_item("acc", SERVICE, "B").unwrap().is_some());
        assert_eq!(
            store
                .get_delta_cursor("acc", SERVICE, "S")
                .unwrap()
                .as_deref(),
            Some("CS")
        );
        assert_eq!(report.upserted, 1);
    }

    /// AC2 (part 2): a repeated id across pages ends with the last value (last-write-wins
    /// via `upsert_item`), and the `deltaLink` is adopted only at the end.
    #[test]
    fn ac2_last_write_wins_on_repeated_id_across_pages() {
        let store = Store::open_in_memory().unwrap();
        let arch = tempfile::tempdir().unwrap();
        let mut map = MappingTable::new();
        seed_folder(&store, "S", Some("root1"));

        let mut t = MockScopedTransport::new(vec![
            (
                "items/S/delta",
                vec![Response::ok(json!({
                    "value": [ file_item("X", "old.txt", "S") ],
                    "@odata.nextLink": "https://graph/next/p2"
                }))],
            ),
            (
                "next/p2",
                vec![Response::ok(json!({
                    "value": [ file_item("X", "new.txt", "S") ],
                    "@odata.deltaLink": "CS"
                }))],
            ),
        ]);
        incremental_sync_scoped(
            &mut t,
            &store,
            &mut map,
            "acc",
            "t",
            arch.path(),
            &[sync_scope("S")],
        )
        .unwrap();
        let x = store.get_item("acc", SERVICE, "X").unwrap().unwrap();
        assert_eq!(x.name, "new.txt", "last write wins");
        assert_eq!(
            store
                .get_delta_cursor("acc", SERVICE, "S")
                .unwrap()
                .as_deref(),
            Some("CS")
        );
    }

    /// AC3: overlapping parent (P, sync) and nested child (C, offline) scopes — a file X
    /// under C is owned by the deepest scope C. P's recursive delta also reports C and X,
    /// but both resolve to the deeper C and are skipped by P.
    #[test]
    fn ac3_overlapping_scopes_item_owned_by_deepest() {
        let store = Store::open_in_memory().unwrap();
        let arch = tempfile::tempdir().unwrap();
        let mut map = MappingTable::new();
        seed_folder(&store, "P", Some("root1"));
        seed_folder(&store, "C", Some("P"));

        let mut t = MockScopedTransport::new(vec![
            (
                "items/C/delta",
                vec![Response::ok(json!({
                    "value": [ file_item("X", "x.txt", "C") ],
                    "@odata.deltaLink": "CC"
                }))],
            ),
            (
                "items/P/delta",
                vec![Response::ok(json!({
                    "value": [
                        { "id": "C", "name": "C", "parentReference": {"id": "P"}, "folder": {} },
                        file_item("X", "x.txt", "C")
                    ],
                    "@odata.deltaLink": "CP"
                }))],
            ),
        ]);
        let report = incremental_sync_scoped(
            &mut t,
            &store,
            &mut map,
            "acc",
            "t",
            arch.path(),
            &[sync_scope("P"), offline_scope("C")],
        )
        .unwrap();

        let x = store.get_item("acc", SERVICE, "X").unwrap().unwrap();
        assert_eq!(x.parent_remote_id.as_deref(), Some("C"));
        assert_eq!(report.upserted, 1, "only the deepest scope C claims X");
        // P's page: folder C (owned by C) + X (owned by C) → both skipped by P.
        assert_eq!(report.skipped, 2, "P skips items owned by deeper C");
        assert_eq!(
            store
                .get_delta_cursor("acc", SERVICE, "C")
                .unwrap()
                .as_deref(),
            Some("CC")
        );
        assert_eq!(
            store
                .get_delta_cursor("acc", SERVICE, "P")
                .unwrap()
                .as_deref(),
            Some("CP")
        );
    }

    /// Records ops and returns a canned drive item for uploads.
    struct MockWriter {
        uploaded: std::cell::RefCell<Vec<(String, usize)>>,
        deleted: std::cell::RefCell<Vec<String>>,
        mtime_set: std::cell::RefCell<Vec<(String, String)>>,
    }
    impl MockWriter {
        fn new() -> Self {
            MockWriter {
                uploaded: Default::default(),
                deleted: Default::default(),
                mtime_set: Default::default(),
            }
        }
    }
    impl RemoteWriter for MockWriter {
        fn upload(&self, dest_path: &str, data: &[u8]) -> Result<Value, String> {
            self.uploaded
                .borrow_mut()
                .push((dest_path.to_string(), data.len()));
            let name = dest_path.rsplit('/').next().unwrap_or(dest_path);
            Ok(json!({
                "id": "NEWID",
                "name": name,
                "parentReference": { "id": "root1" },
                "size": data.len(),
                "file": { "hashes": { "quickXorHash": "UP==" } }
            }))
        }
        fn delete(&self, item_id: &str) -> Result<(), String> {
            self.deleted.borrow_mut().push(item_id.to_string());
            Ok(())
        }
        fn set_mtime(&self, item_id: &str, rfc3339: &str) -> Result<Value, String> {
            self.mtime_set
                .borrow_mut()
                .push((item_id.to_string(), rfc3339.to_string()));
            // mirror Graph: return the item with the stamped fileSystemInfo
            Ok(json!({
                "id": item_id,
                "name": "note.txt",
                "parentReference": { "id": "root1" },
                "size": 5,
                "file": { "hashes": { "quickXorHash": "UP==" } },
                "fileSystemInfo": { "lastModifiedDateTime": rfc3339 }
            }))
        }
    }

    #[test]
    fn unix_to_rfc3339_roundtrips_with_rfc3339_to_unix() {
        for s in [
            "2021-03-15T08:30:00Z",
            "1970-01-01T00:00:00Z",
            "2024-02-29T23:59:59Z", // leap day
            "2000-12-31T12:00:00Z",
        ] {
            let secs = rfc3339_to_unix(s).unwrap();
            assert_eq!(unix_to_rfc3339(secs), s, "roundtrip {s}");
        }
    }

    #[test]
    fn push_upload_preserves_local_mtime() {
        // With a local mtime, push_upload must stamp the cloud item's
        // fileSystemInfo (preserve the original timestamp) and ingest the corrected
        // item — not the upload-time one.
        let store = Store::open_in_memory().unwrap();
        let mut map = MappingTable::new();
        let w = MockWriter::new();
        let secs = rfc3339_to_unix("2021-03-15T08:30:00Z").unwrap();
        let id = push_upload(
            &w,
            &store,
            &mut map,
            "acc",
            "/Docs/note.txt",
            b"hello",
            Some(secs),
        )
        .unwrap();
        // set_mtime was called with the RFC3339 of the local mtime
        assert_eq!(
            *w.mtime_set.borrow(),
            vec![(id.clone(), "2021-03-15T08:30:00Z".to_string())]
        );
        // the stored item carries the preserved timestamp, not the upload time
        let it = store.get_item("acc", SERVICE, &id).unwrap().unwrap();
        assert_eq!(it.remote_mtime.as_deref(), Some("2021-03-15T08:30:00Z"));
    }

    #[test]
    fn push_upload_without_mtime_skips_set_mtime() {
        let store = Store::open_in_memory().unwrap();
        let mut map = MappingTable::new();
        let w = MockWriter::new();
        push_upload(
            &w,
            &store,
            &mut map,
            "acc",
            "/Docs/note.txt",
            b"hello",
            None,
        )
        .unwrap();
        assert!(w.mtime_set.borrow().is_empty());
    }

    #[test]
    fn push_upload_stores_clean_item() {
        let store = Store::open_in_memory().unwrap();
        let mut map = MappingTable::new();
        let w = MockWriter::new();
        let id = push_upload(
            &w,
            &store,
            &mut map,
            "acc",
            "/Docs/note.txt",
            b"hello",
            None,
        )
        .unwrap();
        assert_eq!(id, "NEWID");
        assert_eq!(
            w.uploaded.borrow().as_slice(),
            &[("/Docs/note.txt".to_string(), 5)]
        );
        let it = store.get_item("acc", SERVICE, "NEWID").unwrap().unwrap();
        assert_eq!(it.name, "note.txt");
        assert_eq!(it.sync_state, "clean");
        assert_eq!(it.size, Some(5));
    }

    #[test]
    fn push_delete_tombstones_after_remote_delete() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_item(&Item::new("acc", SERVICE, "X1", "x.txt", "file"))
            .unwrap();
        let w = MockWriter::new();
        push_delete(&w, &store, "acc", "X1", "2026-06-02T00:00:00Z").unwrap();
        assert_eq!(w.deleted.borrow().as_slice(), &["X1".to_string()]);
        assert!(store
            .get_item("acc", SERVICE, "X1")
            .unwrap()
            .unwrap()
            .deleted_at
            .is_some());
    }

    /// Returns canned content per remote id; errors for unknown ids.
    struct MockDownloader(HashMap<String, Vec<u8>>);
    impl Downloader for MockDownloader {
        fn download(&self, remote_id: &str) -> Result<Vec<u8>, String> {
            self.0
                .get(remote_id)
                .cloned()
                .ok_or_else(|| format!("no content for {remote_id}"))
        }
    }

    /// Seed one tracked file at the sync root: store item (remote_dirty with NEW
    /// remote metadata, as after a delta ingest) + on-disk content + optionally
    /// the last-synced reference for that on-disk content.
    fn seed_edit_edit_scenario(
        store: &Store,
        dir: &Path,
        disk_content: &[u8],
        record_reference: bool,
    ) -> std::path::PathBuf {
        let mut file = Item::new("acc", SERVICE, "c1", "doc.txt", "file");
        file.local_path = Some("doc.txt".into());
        file.sync_state = "remote_dirty".into();
        // NEW remote values (the other side's edit) — already ingested
        file.size = Some(9);
        file.remote_mtime = Some("2026-06-10T12:00:00Z".into());
        store.upsert_item(&file).unwrap();
        let full = dir.join("doc.txt");
        std::fs::write(&full, disk_content).unwrap();
        if record_reference {
            // reference == current disk state (file was clean at last sync)
            record_synced_state(
                store,
                "acc",
                "c1",
                &full,
                Some(crate::quickxor::quickxor_base64(disk_content)),
            );
        }
        full
    }

    #[test]
    fn materialize_keeps_both_when_local_file_was_edited() {
        // Regression for the data-loss bug the staging E2E found: local edit +
        // remote edit in the same pass silently overwrote the local edit. The
        // local edit must survive as a safeBackup conflict copy.
        let store = Store::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let full = seed_edit_edit_scenario(&store, dir.path(), b"v1 original", true);
        // user edits the file AFTER the reference was recorded
        std::fs::write(&full, b"v2 LOCAL edit").unwrap();

        let dl = MockDownloader(
            [("c1".to_string(), b"v2 REMOTE".to_vec())]
                .into_iter()
                .collect(),
        );
        let report = materialize_downloads(&store, &dl, "acc", dir.path(), "host").unwrap();
        assert_eq!(report.conflicts, 1, "local edit must be kept as a copy");
        assert_eq!(report.downloaded, 1);
        // original path now holds the cloud version
        assert_eq!(std::fs::read(&full).unwrap(), b"v2 REMOTE");
        // the local edit survives in the safeBackup copy
        let copy = dir.path().join("doc-host-safeBackup-0001.txt");
        assert_eq!(
            std::fs::read(&copy).unwrap(),
            b"v2 LOCAL edit",
            "local edit was clobbered"
        );
    }

    #[test]
    fn materialize_overwrites_stale_clean_file_without_conflict_copy() {
        // Counter-case: the local file is untouched since the last sync (disk ==
        // reference), only the cloud changed — a normal update must NOT spray a
        // conflict copy.
        let store = Store::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let full = seed_edit_edit_scenario(&store, dir.path(), b"v1 original", true);

        let dl = MockDownloader(
            [("c1".to_string(), b"v2 REMOTE".to_vec())]
                .into_iter()
                .collect(),
        );
        let report = materialize_downloads(&store, &dl, "acc", dir.path(), "host").unwrap();
        assert_eq!(report.conflicts, 0, "clean update must not create a copy");
        assert_eq!(report.downloaded, 1);
        assert_eq!(std::fs::read(&full).unwrap(), b"v2 REMOTE");
        assert!(!dir.path().join("doc-host-safeBackup-0001.txt").exists());
    }

    #[test]
    fn materialize_without_synced_reference_keeps_legacy_overwrite() {
        // Pre-v8 stores have no reference: keep the old overwrite behavior (no
        // conflict-copy spray on ordinary updates) rather than guessing.
        let store = Store::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let full = seed_edit_edit_scenario(&store, dir.path(), b"whatever is here", false);

        let dl = MockDownloader(
            [("c1".to_string(), b"v2 REMOTE".to_vec())]
                .into_iter()
                .collect(),
        );
        let report = materialize_downloads(&store, &dl, "acc", dir.path(), "host").unwrap();
        assert_eq!(report.conflicts, 0);
        assert_eq!(std::fs::read(&full).unwrap(), b"v2 REMOTE");
        assert!(!dir.path().join("doc-host-safeBackup-0001.txt").exists());
    }

    // ---- #655 / S-OM.9: scoped offline materialize + progress + scoped writeback ------

    const NOW: &str = "2026-07-04T00:00:00Z";

    /// Seed an offline scope root folder + a `remote_dirty` file under it, plus a
    /// non-scope folder + file. Returns nothing; ids are `F1`/`a1` (offline) and
    /// `G1`/`b1` (out of scope).
    fn seed_offline_and_nonscope(store: &Store) {
        let mut f1 = Item::new("acc", SERVICE, "F1", "Photos", "folder");
        f1.local_path = Some("Photos".into());
        f1.sync_state = "remote_dirty".into();
        store.upsert_item(&f1).unwrap();
        let mut a1 = Item::new("acc", SERVICE, "a1", "a.txt", "file");
        a1.local_path = Some("a.txt".into());
        a1.parent_remote_id = Some("F1".into());
        a1.sync_state = "remote_dirty".into();
        a1.size = Some(4);
        store.upsert_item(&a1).unwrap();
        let mut g1 = Item::new("acc", SERVICE, "G1", "Other", "folder");
        g1.local_path = Some("Other".into());
        g1.sync_state = "remote_dirty".into();
        store.upsert_item(&g1).unwrap();
        let mut b1 = Item::new("acc", SERVICE, "b1", "b.txt", "file");
        b1.local_path = Some("b.txt".into());
        b1.parent_remote_id = Some("G1".into());
        b1.sync_state = "remote_dirty".into();
        b1.size = Some(4);
        store.upsert_item(&b1).unwrap();
    }

    #[test]
    fn materialize_scoped_writes_only_offline_items_and_marks_content_state() {
        let store = Store::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        seed_offline_and_nonscope(&store);
        let dl = MockDownloader(
            [
                ("a1".to_string(), b"AAAA".to_vec()),
                ("b1".to_string(), b"BBBB".to_vec()),
            ]
            .into_iter()
            .collect(),
        );
        let offline: BTreeSet<&str> = ["F1"].into_iter().collect();
        let cfg = isyncyou_core::SyncConfig::default();
        let dev = isyncyou_core::policy::DeviceState::always_on(u64::MAX);
        let report = materialize_downloads_scoped(
            &store,
            &dl,
            "acc",
            dir.path(),
            "host",
            NOW,
            &offline,
            &cfg,
            &dev,
            &(),
        )
        .unwrap();
        assert_eq!(
            report.downloaded, 1,
            "only the offline-scope file is fetched"
        );
        // The offline file is materialized to sync_root with the content-state marked.
        assert_eq!(
            std::fs::read(dir.path().join("Photos/a.txt")).unwrap(),
            b"AAAA"
        );
        let a = store.get_item("acc", SERVICE, "a1").unwrap().unwrap();
        assert_eq!(a.content_state.as_deref(), Some("materialized"));
        assert_eq!(a.body_location.as_deref(), Some("sync"));
        assert_eq!(a.body_state.as_deref(), Some("available"));
        assert_eq!(a.materialized_at.as_deref(), Some(NOW));
        assert_eq!(a.sync_state, "clean");
        // The non-scope file is skipped: not on disk, content-state untouched.
        assert!(!dir.path().join("Other/b.txt").exists());
        let b = store.get_item("acc", SERVICE, "b1").unwrap().unwrap();
        assert_eq!(b.body_state, None, "non-offline item is left untouched");
        assert_eq!(b.sync_state, "remote_dirty");
    }

    #[test]
    fn materialize_scoped_storage_floor_stops_new_downloads() {
        let store = Store::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        seed_offline_and_nonscope(&store);
        let dl = MockDownloader([("a1".to_string(), b"AAAA".to_vec())].into_iter().collect());
        let offline: BTreeSet<&str> = ["F1"].into_iter().collect();
        // Zero free bytes < the storage floor (min_free_bytes = 256 MiB) → StorageFloor blocks it.
        let cfg = isyncyou_core::SyncConfig::default();
        let low = isyncyou_core::policy::DeviceState::always_on(0);
        let report = materialize_downloads_scoped(
            &store,
            &dl,
            "acc",
            dir.path(),
            "host",
            NOW,
            &offline,
            &cfg,
            &low,
            &(),
        )
        .unwrap();
        assert_eq!(report.downloaded, 0, "no new download under the floor");
        assert!(!dir.path().join("Photos/a.txt").exists());
        let a = store.get_item("acc", SERVICE, "a1").unwrap().unwrap();
        assert_eq!(a.body_state.as_deref(), Some("missing"));
        assert_eq!(a.content_state, None);
    }

    #[test]
    fn materialize_scoped_reports_and_clears_progress() {
        let store = Store::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        seed_offline_and_nonscope(&store);
        let dl = MockDownloader([("a1".to_string(), b"AAAA".to_vec())].into_iter().collect());
        let offline: BTreeSet<&str> = ["F1"].into_iter().collect();
        let cfg = isyncyou_core::SyncConfig::default();
        let dev = isyncyou_core::policy::DeviceState::always_on(u64::MAX);
        let progress = SharedProgress::new();
        let report = materialize_downloads_scoped(
            &store,
            &dl,
            "acc",
            dir.path(),
            "host",
            NOW,
            &offline,
            &cfg,
            &dev,
            &progress,
        )
        .unwrap();
        assert_eq!(report.downloaded, 1);
        // A finished transfer is dropped from the in-flight snapshot.
        assert!(progress.snapshot().is_empty());
    }

    #[test]
    fn shared_progress_tracks_and_clears_slots() {
        let p = SharedProgress::new();
        p.begin("id1", "f.txt", 100);
        p.advance("id1", 40);
        p.retry_after("id1", 7);
        let snap = p.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].bytes_done, 40);
        assert_eq!(snap[0].bytes_total, 100);
        assert_eq!(snap[0].retry_after_secs, 7);
        p.finish("id1");
        assert!(p.snapshot().is_empty());
    }

    #[test]
    fn shared_progress_cancel_is_one_shot() {
        let p = SharedProgress::new();
        assert!(!p.is_cancelled("rid"));
        p.request_cancel("rid");
        assert!(p.is_cancelled("rid"));
        // consume_cancel is one-shot: after the pass consumes it, a later pass is no longer
        // cancelled (so a cancelled folder re-materializes next time, not skipped forever).
        p.consume_cancel("rid");
        assert!(!p.is_cancelled("rid"));
        // finish() also clears a stale cancel so it can't leak onto a reused remote id.
        p.request_cancel("rid2");
        p.finish("rid2");
        assert!(!p.is_cancelled("rid2"));
    }

    #[test]
    fn materialize_scoped_skips_cancelled_transfer_then_retries_next_pass() {
        let store = Store::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        seed_offline_and_nonscope(&store);
        let dl = MockDownloader([("a1".to_string(), b"AAAA".to_vec())].into_iter().collect());
        let offline: BTreeSet<&str> = ["F1"].into_iter().collect();
        let cfg = isyncyou_core::SyncConfig::default();
        let dev = isyncyou_core::policy::DeviceState::always_on(u64::MAX);
        let progress = SharedProgress::new();
        // The user cancels the offline file before the pass reaches it.
        progress.request_cancel("a1");
        let report = materialize_downloads_scoped(
            &store,
            &dl,
            "acc",
            dir.path(),
            "host",
            NOW,
            &offline,
            &cfg,
            &dev,
            &progress,
        )
        .unwrap();
        assert_eq!(
            report.cancelled, 1,
            "the cancelled file counts as cancelled"
        );
        assert_eq!(report.downloaded, 0, "nothing is downloaded");
        assert_eq!(report.failed, 0, "a user-requested cancel is not a failure");
        assert!(
            !dir.path().join("Photos/a.txt").exists(),
            "no body is written for the cancelled file"
        );
        // One-shot: the cancel was consumed, so a second pass materializes the file normally.
        assert!(!progress.is_cancelled("a1"));
        let report2 = materialize_downloads_scoped(
            &store,
            &dl,
            "acc",
            dir.path(),
            "host",
            NOW,
            &offline,
            &cfg,
            &dev,
            &progress,
        )
        .unwrap();
        assert_eq!(
            report2.downloaded, 1,
            "the previously-cancelled file materializes on the next pass"
        );
        assert_eq!(
            std::fs::read(dir.path().join("Photos/a.txt")).unwrap(),
            b"AAAA"
        );
    }

    #[test]
    fn materialize_scoped_reports_incremental_download_progress() {
        // #656 F-C: the materialize streams the body and reports cumulative bytes as they arrive,
        // so the transfer panel shows a moving bar instead of jumping 0% -> gone.
        struct ChunkedDownloader;
        impl Downloader for ChunkedDownloader {
            fn download(&self, _id: &str) -> Result<Vec<u8>, String> {
                Ok(b"AAAA".to_vec())
            }
            fn download_with_progress(
                &self,
                _id: &str,
                on: &mut dyn FnMut(u64),
            ) -> Result<Vec<u8>, String> {
                on(2); // 2 of 4 bytes
                on(4); // all 4 bytes
                Ok(b"AAAA".to_vec())
            }
        }
        #[derive(Default)]
        struct RecordingProgress {
            advances: std::sync::Mutex<Vec<(String, u64)>>,
        }
        impl ProgressSink for RecordingProgress {
            fn begin(&self, _: &str, _: &str, _: u64) {}
            fn advance(&self, id: &str, done: u64) {
                self.advances.lock().unwrap().push((id.to_string(), done));
            }
            fn retry_after(&self, _: &str, _: u64) {}
            fn finish(&self, _: &str) {}
        }
        let store = Store::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        seed_offline_and_nonscope(&store);
        let offline: BTreeSet<&str> = ["F1"].into_iter().collect();
        let cfg = isyncyou_core::SyncConfig::default();
        let dev = isyncyou_core::policy::DeviceState::always_on(u64::MAX);
        let progress = RecordingProgress::default();
        let report = materialize_downloads_scoped(
            &store,
            &ChunkedDownloader,
            "acc",
            dir.path(),
            "host",
            NOW,
            &offline,
            &cfg,
            &dev,
            &progress,
        )
        .unwrap();
        assert_eq!(report.downloaded, 1);
        let a1: Vec<u64> = progress
            .advances
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| id == "a1")
            .map(|(_, n)| *n)
            .collect();
        assert_eq!(
            a1,
            vec![2, 4],
            "cumulative byte progress is reported incrementally, not just once at the end"
        );
    }

    #[test]
    fn scan_creates_scoped_keeps_only_offline_prefix() {
        let store = Store::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // Tracked scope-root folders (so their local paths become the offline prefix).
        let mut f1 = Item::new("acc", SERVICE, "F1", "Photos", "folder");
        f1.local_path = Some("Photos".into());
        store.upsert_item(&f1).unwrap();
        let mut g1 = Item::new("acc", SERVICE, "G1", "Other", "folder");
        g1.local_path = Some("Other".into());
        store.upsert_item(&g1).unwrap();
        std::fs::create_dir_all(dir.path().join("Photos")).unwrap();
        std::fs::create_dir_all(dir.path().join("Other")).unwrap();
        std::fs::write(dir.path().join("Photos/new.jpg"), b"x").unwrap();
        std::fs::write(dir.path().join("Other/x.jpg"), b"y").unwrap();
        let offline: BTreeSet<&str> = ["F1"].into_iter().collect();
        let creates = scan_local_creates_scoped(&store, "acc", dir.path(), &offline).unwrap();
        assert_eq!(creates, vec![PathBuf::from("Photos/new.jpg")]);
    }

    #[test]
    fn scan_modifies_scoped_keeps_only_offline_items() {
        let store = Store::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // Two clean tracked files, one under an offline scope, one not.
        let mut f1 = Item::new("acc", SERVICE, "F1", "Photos", "folder");
        f1.local_path = Some("Photos".into());
        store.upsert_item(&f1).unwrap();
        let mut g1 = Item::new("acc", SERVICE, "G1", "Other", "folder");
        g1.local_path = Some("Other".into());
        store.upsert_item(&g1).unwrap();
        for (id, parent, name) in [("a1", "F1", "Photos"), ("b1", "G1", "Other")] {
            let mut it = Item::new("acc", SERVICE, id, "f.txt", "file");
            it.local_path = Some("f.txt".into());
            it.parent_remote_id = Some(parent.into());
            it.sync_state = "clean".into();
            it.size = Some(4);
            it.etag = Some("e1".into());
            store.upsert_item(&it).unwrap();
            std::fs::create_dir_all(dir.path().join(name)).unwrap();
            // Write a differently-sized body so is_local_modified fires.
            std::fs::write(dir.path().join(name).join("f.txt"), b"MODIFIED").unwrap();
        }
        let offline: BTreeSet<&str> = ["F1"].into_iter().collect();
        let mods = scan_local_modifies_scoped(&store, "acc", dir.path(), &offline).unwrap();
        assert_eq!(mods.len(), 1);
        assert_eq!(mods[0].0, "a1");
    }

    #[test]
    fn push_local_creates_records_the_synced_reference() {
        // The upload path must record the reference, so a later remote edit +
        // local edit can be told apart by the download keep-both.
        let store = Store::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("n.txt"), b"local v1").unwrap();
        let w = MockWriter::new();
        let mut map = MappingTable::new();
        push_local_creates(
            &w,
            &store,
            &mut map,
            "acc",
            dir.path(),
            &[std::path::PathBuf::from("n.txt")],
        )
        .unwrap();
        let id = store
            .all_items_by_service("acc", SERVICE)
            .unwrap()
            .first()
            .unwrap()
            .remote_id
            .clone();
        let (size, _mtime, hash) = store
            .get_synced_state("acc", SERVICE, &id)
            .unwrap()
            .expect("upload must record the synced reference");
        assert_eq!(size, 8); // b"local v1"
        assert_eq!(hash, Some(crate::quickxor::quickxor_base64(b"local v1")));
    }

    #[test]
    fn materialize_writes_remote_dirty_files_then_marks_clean() {
        let store = Store::open_in_memory().unwrap();
        // a folder under the (absent) drive root, and a file under that folder
        let mut folder = Item::new("acc", SERVICE, "F1", "Photos", "folder");
        folder.parent_remote_id = Some("root1".into()); // root id is not in the store
        folder.local_path = Some("Photos".into());
        folder.sync_state = "remote_dirty".into();
        store.upsert_item(&folder).unwrap();
        let mut file = Item::new("acc", SERVICE, "a1", "IMG.jpg", "file");
        file.parent_remote_id = Some("F1".into());
        file.local_path = Some("IMG.jpg".into());
        file.sync_state = "remote_dirty".into();
        file.remote_mtime = Some("2024-01-02T03:04:05Z".into());
        store.upsert_item(&file).unwrap();

        let dl = MockDownloader(
            [("a1".to_string(), b"JPEGDATA".to_vec())]
                .into_iter()
                .collect(),
        );
        let dir = tempfile::tempdir().unwrap();
        let report = materialize_downloads(&store, &dl, "acc", dir.path(), "host").unwrap();
        assert_eq!(report.downloaded, 1);
        assert_eq!(report.dirs_created, 1);
        assert_eq!(report.failed, 0);

        // the file is on disk under Photos/ with the right content
        let path = dir.path().join("Photos").join("IMG.jpg");
        assert_eq!(std::fs::read(&path).unwrap(), b"JPEGDATA");
        // its mtime mirrors the cloud lastModifiedDateTime (2024-01-02T03:04:05Z)
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let secs = modified
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(
            secs, 1_704_164_645,
            "mtime should match the cloud timestamp"
        );
        // no stray temp file left in the folder
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("Photos"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() != "IMG.jpg")
            .collect();
        assert!(
            leftovers.is_empty(),
            "atomic write left a temp file: {leftovers:?}"
        );
        // both items are now clean
        for id in ["a1", "F1"] {
            assert_eq!(
                store
                    .get_item("acc", SERVICE, id)
                    .unwrap()
                    .unwrap()
                    .sync_state,
                "clean"
            );
        }

        // a second pass downloads nothing (everything is clean)
        let r2 = materialize_downloads(&store, &dl, "acc", dir.path(), "host").unwrap();
        assert_eq!(r2.downloaded, 0);
        assert_eq!(r2.dirs_created, 0);
    }

    #[test]
    fn rfc3339_to_unix_parses_utc_timestamps() {
        assert_eq!(rfc3339_to_unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_to_unix("2024-01-02T03:04:05Z"), Some(1_704_164_645));
        // fractional seconds + zone are ignored (seconds-resolution)
        assert_eq!(
            rfc3339_to_unix("2024-01-02T03:04:05.678Z"),
            Some(1_704_164_645)
        );
        // a leap-day date computes correctly
        assert_eq!(rfc3339_to_unix("2024-02-29T00:00:00Z"), Some(1_709_164_800));
        // malformed → None, never panics
        assert_eq!(rfc3339_to_unix(""), None);
        assert_eq!(rfc3339_to_unix("not-a-date"), None);
        assert_eq!(rfc3339_to_unix("2024-13-01T00:00:00Z"), None);
    }

    #[test]
    fn materialize_counts_unresolvable_item_as_failed_not_panic() {
        let store = Store::open_in_memory().unwrap();
        let mut f = Item::new("acc", SERVICE, "x", "x", "file");
        f.sync_state = "remote_dirty".into();
        f.local_path = None; // no local name → path can't be built
        store.upsert_item(&f).unwrap();
        let dl = MockDownloader(HashMap::new());
        let dir = tempfile::tempdir().unwrap();
        let r = materialize_downloads(&store, &dl, "acc", dir.path(), "host").unwrap();
        assert_eq!(r.failed, 1);
        assert_eq!(r.downloaded, 0);
    }

    #[test]
    fn materialize_skips_file_already_present_and_unchanged() {
        let store = Store::open_in_memory().unwrap();
        let mut file = Item::new("acc", SERVICE, "a1", "doc.txt", "file");
        file.local_path = Some("doc.txt".into());
        file.sync_state = "remote_dirty".into();
        file.size = Some(5);
        file.remote_mtime = Some("2024-01-02T03:04:05Z".into());
        store.upsert_item(&file).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.txt");
        std::fs::write(&path, b"hello").unwrap(); // size 5 matches
        set_file_mtime(&path, "2024-01-02T03:04:05Z"); // mtime matches

        // a downloader that errors on any call — so a skip is provable (no download)
        let dl = MockDownloader(HashMap::new());
        let report = materialize_downloads(&store, &dl, "acc", dir.path(), "host").unwrap();
        assert_eq!(report.skipped, 1, "unchanged file should be skipped");
        assert_eq!(report.downloaded, 0);
        assert_eq!(
            report.failed, 0,
            "must not attempt a download for an unchanged file"
        );
        assert_eq!(
            store
                .get_item("acc", SERVICE, "a1")
                .unwrap()
                .unwrap()
                .sync_state,
            "clean"
        );

        // a size change defeats the skip → it attempts a (here failing) download
        std::fs::write(&path, b"changed!!").unwrap(); // size 9 != 5
        store
            .set_sync_state("acc", SERVICE, "a1", "remote_dirty")
            .unwrap();
        let r2 = materialize_downloads(&store, &dl, "acc", dir.path(), "host").unwrap();
        assert_eq!(r2.skipped, 0);
        assert_eq!(
            r2.failed, 1,
            "size mismatch must trigger a re-download attempt"
        );
    }

    #[test]
    fn pending_and_apply_local_deletes_move_tombstoned_files_to_trash() {
        let store = Store::open_in_memory().unwrap();
        let mut folder = Item::new("acc", SERVICE, "F1", "Docs", "folder");
        folder.local_path = Some("Docs".into());
        store.upsert_item(&folder).unwrap();
        let mut file = Item::new("acc", SERVICE, "a1", "note.txt", "file");
        file.parent_remote_id = Some("F1".into());
        file.local_path = Some("note.txt".into());
        store.upsert_item(&file).unwrap();

        // it's on disk under sync_root/Docs/note.txt
        let base = tempfile::tempdir().unwrap();
        let sync_root = base.path().join("od");
        let trash_root = base.path().join("trash"); // A9: outside the sync root
        std::fs::create_dir_all(sync_root.join("Docs")).unwrap();
        std::fs::write(sync_root.join("Docs").join("note.txt"), b"hi").unwrap();

        // nothing tombstoned yet -> no pending deletes
        assert!(pending_local_deletes(&store, "acc", &sync_root)
            .unwrap()
            .is_empty());

        // the remote item is deleted -> it becomes a pending local delete
        store
            .mark_deleted("acc", SERVICE, "a1", "2026-06-02T00:00:00Z")
            .unwrap();
        let pending = pending_local_deletes(&store, "acc", &sync_root).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].remote_id, "a1");
        assert_eq!(pending[0].rel, PathBuf::from("Docs/note.txt"));

        let moved = apply_local_deletes(&sync_root, &trash_root, &pending).unwrap();
        assert_eq!(moved, 1);
        // gone from the sync root, present in the trash (outside sync_root)
        assert!(!sync_root.join("Docs").join("note.txt").exists());
        assert_eq!(
            std::fs::read(trash_root.join("Docs").join("note.txt")).unwrap(),
            b"hi"
        );
        assert!(
            !trash_root.starts_with(&sync_root),
            "trash must be outside the sync root"
        );

        // a second pass finds nothing (the local file is gone)
        assert!(pending_local_deletes(&store, "acc", &sync_root)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn apply_local_deletes_skips_already_gone_paths() {
        let base = tempfile::tempdir().unwrap();
        let sync_root = base.path().join("od");
        std::fs::create_dir_all(&sync_root).unwrap();
        let targets = vec![PendingLocalDelete {
            remote_id: "x".into(),
            rel: PathBuf::from("missing.txt"),
        }];
        // path doesn't exist -> moved=0, no error
        let moved = apply_local_deletes(&sync_root, &base.path().join("trash"), &targets).unwrap();
        assert_eq!(moved, 0);
    }

    #[test]
    fn cloud_dest_path_encodes_each_segment() {
        assert_eq!(
            cloud_dest_path(&PathBuf::from("Docs/note.txt")),
            "/Docs/note.txt"
        );
        // a forbidden cloud char in a local name is encoded (never raw)
        let p = cloud_dest_path(&PathBuf::from("a:b/c.txt"));
        assert!(p.starts_with("/") && p.ends_with("/c.txt"));
        assert!(!p.contains(':'), "raw forbidden char leaked: {p}");
    }

    #[test]
    fn scan_local_creates_finds_only_untracked_files() {
        let store = Store::open_in_memory().unwrap();
        // a tracked, materialized file at Docs/note.txt
        let mut folder = Item::new("acc", SERVICE, "F1", "Docs", "folder");
        folder.local_path = Some("Docs".into());
        store.upsert_item(&folder).unwrap();
        let mut tracked = Item::new("acc", SERVICE, "a1", "note.txt", "file");
        tracked.parent_remote_id = Some("F1".into());
        tracked.local_path = Some("note.txt".into());
        store.upsert_item(&tracked).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let sync_root = dir.path();
        std::fs::create_dir_all(sync_root.join("Docs")).unwrap();
        std::fs::write(sync_root.join("Docs").join("note.txt"), b"tracked").unwrap(); // tracked
        std::fs::write(sync_root.join("Docs").join("new.txt"), b"new").unwrap(); // create
        std::fs::write(sync_root.join("top.txt"), b"top").unwrap(); // create
        std::fs::write(sync_root.join("scratch.isync-tmp"), b"x").unwrap(); // ignored

        let mut creates = scan_local_creates(&store, "acc", sync_root).unwrap();
        creates.sort();
        assert_eq!(
            creates,
            vec![PathBuf::from("Docs/new.txt"), PathBuf::from("top.txt")]
        );
    }

    #[test]
    fn push_local_creates_uploads_each_to_its_cloud_path() {
        let store = Store::open_in_memory().unwrap();
        let mut map = MappingTable::new();
        let w = MockWriter::new();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Docs")).unwrap();
        std::fs::write(dir.path().join("Docs").join("new.txt"), b"hello").unwrap();

        let n = push_local_creates(
            &w,
            &store,
            &mut map,
            "acc",
            dir.path(),
            &[PathBuf::from("Docs/new.txt")],
        )
        .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            w.uploaded.borrow().as_slice(),
            &[("/Docs/new.txt".to_string(), 5)]
        );
    }

    /// A replacer that either echoes a refreshed item (replaced) or signals a
    /// conflict, controllable per test.
    struct MockReplacer {
        conflict: bool,
        calls: std::cell::RefCell<Vec<(String, usize, String)>>,
    }
    impl ContentReplacer for MockReplacer {
        fn replace_if_match(
            &self,
            item_id: &str,
            data: &[u8],
            etag: &str,
        ) -> Result<Option<Value>, String> {
            self.calls
                .borrow_mut()
                .push((item_id.to_string(), data.len(), etag.to_string()));
            if self.conflict {
                Ok(None)
            } else {
                Ok(Some(json!({
                    "id": item_id,
                    "name": "note.txt",
                    "parentReference": { "id": "F1" },
                    "size": data.len(),
                    "eTag": "etag-new"
                })))
            }
        }
    }

    fn store_with_tracked_file(size: i64) -> (Store, tempfile::TempDir) {
        let store = Store::open_in_memory().unwrap();
        let mut folder = Item::new("acc", SERVICE, "F1", "Docs", "folder");
        folder.local_path = Some("Docs".into());
        store.upsert_item(&folder).unwrap();
        let mut f = Item::new("acc", SERVICE, "a1", "note.txt", "file");
        f.parent_remote_id = Some("F1".into());
        f.local_path = Some("note.txt".into());
        f.size = Some(size);
        f.etag = Some("etag-old".into());
        store.upsert_item(&f).unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Docs")).unwrap();
        (store, dir)
    }

    #[test]
    fn scan_local_modifies_detects_size_change_only() {
        let (store, dir) = store_with_tracked_file(2); // store says size=2
                                                       // write a different-sized local file -> modify
        std::fs::write(dir.path().join("Docs").join("note.txt"), b"changed").unwrap();
        let m = scan_local_modifies(&store, "acc", dir.path()).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].0, "a1");
        assert_eq!(m[0].1, PathBuf::from("Docs/note.txt"));
        assert_eq!(m[0].2, "etag-old");

        // same size as stored, no stored hash -> not a modify (size-only fallback)
        let (store2, dir2) = store_with_tracked_file(2);
        std::fs::write(dir2.path().join("Docs").join("note.txt"), b"hi").unwrap(); // 2 bytes
        assert!(scan_local_modifies(&store2, "acc", dir2.path())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn scan_local_modifies_catches_same_size_edit_via_hash() {
        let store = Store::open_in_memory().unwrap();
        let mut folder = Item::new("acc", SERVICE, "F1", "Docs", "folder");
        folder.local_path = Some("Docs".into());
        store.upsert_item(&folder).unwrap();
        let mut f = Item::new("acc", SERVICE, "a1", "n.txt", "file");
        f.parent_remote_id = Some("F1".into());
        f.local_path = Some("n.txt".into());
        f.size = Some(5);
        f.etag = Some("etag-old".into());
        f.remote_mtime = Some("2024-01-02T03:04:05Z".into());
        f.quickxorhash = Some(crate::quickxor::quickxor_base64(b"hello")); // cloud content
        store.upsert_item(&f).unwrap();

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Docs")).unwrap();
        let path = dir.path().join("Docs").join("n.txt");

        // same content as the cloud, but mtime touched (now != stored): the hash
        // matches, so it is NOT a modify (no false positive on a pure touch).
        std::fs::write(&path, b"hello").unwrap();
        assert!(
            scan_local_modifies(&store, "acc", dir.path())
                .unwrap()
                .is_empty(),
            "same content with a touched mtime must not be a modify"
        );

        // same SIZE but different content: only the hash reveals the edit.
        std::fs::write(&path, b"world").unwrap(); // 5 bytes, != "hello"
        let m = scan_local_modifies(&store, "acc", dir.path()).unwrap();
        assert_eq!(m.len(), 1, "a same-size edit must be detected via the hash");
        assert_eq!(m[0].0, "a1");
    }

    #[test]
    fn apply_local_modifies_uploads_then_refreshes_store() {
        let (store, dir) = store_with_tracked_file(2);
        std::fs::write(dir.path().join("Docs").join("note.txt"), b"changed").unwrap();
        let mut map = MappingTable::new();
        let r = MockReplacer {
            conflict: false,
            calls: Default::default(),
        };
        let modifies = vec![(
            "a1".into(),
            PathBuf::from("Docs/note.txt"),
            "etag-old".into(),
        )];
        let report =
            apply_local_modifies(&r, &store, &mut map, "acc", dir.path(), "host", &modifies)
                .unwrap();
        assert_eq!(report.uploaded, 1);
        assert_eq!(report.conflicts, 0);
        // the conditional replace was called with the stored etag
        assert_eq!(r.calls.borrow()[0].2, "etag-old");
        // store refreshed: new size + etag, clean
        let it = store.get_item("acc", SERVICE, "a1").unwrap().unwrap();
        assert_eq!(it.size, Some(7)); // "changed"
        assert_eq!(it.etag.as_deref(), Some("etag-new"));
        assert_eq!(it.sync_state, "clean");
    }

    #[test]
    fn apply_local_modifies_keeps_both_on_conflict() {
        let (store, dir) = store_with_tracked_file(2);
        std::fs::write(dir.path().join("Docs").join("note.txt"), b"changed").unwrap();
        let mut map = MappingTable::new();
        let r = MockReplacer {
            conflict: true, // cloud changed -> 412
            calls: Default::default(),
        };
        let modifies = vec![(
            "a1".into(),
            PathBuf::from("Docs/note.txt"),
            "etag-old".into(),
        )];
        let report =
            apply_local_modifies(&r, &store, &mut map, "acc", dir.path(), "host", &modifies)
                .unwrap();
        assert_eq!(report.uploaded, 0);
        assert_eq!(report.conflicts, 1);
        // cloud version NOT overwritten: size/etag unchanged...
        let it = store.get_item("acc", SERVICE, "a1").unwrap().unwrap();
        assert_eq!(it.size, Some(2));
        assert_eq!(it.etag.as_deref(), Some("etag-old"));
        // ...but the item is re-marked remote_dirty so the cloud copy re-downloads
        assert_eq!(it.sync_state, "remote_dirty");
        // keep-both: the local edit moved to a conflict copy, original path freed
        let docs = dir.path().join("Docs");
        assert!(
            !docs.join("note.txt").exists(),
            "original must be moved aside"
        );
        let copy = docs.join("note-host-safeBackup-0001.txt");
        assert!(copy.exists(), "conflict copy must exist");
        assert_eq!(std::fs::read(&copy).unwrap(), b"changed");
    }

    #[test]
    fn conflict_copy_name_disambiguates() {
        assert_eq!(
            conflict_copy_name("report.txt", "laptop", 1),
            "report-laptop-safeBackup-0001.txt"
        );
        // no extension
        assert_eq!(
            conflict_copy_name("README", "host", 2),
            "README-host-safeBackup-0002"
        );
    }

    #[test]
    fn scan_local_deletes_only_clean_files_now_missing() {
        let store = Store::open_in_memory().unwrap();
        // a clean (materialized) file that the user deleted locally
        let mut gone = Item::new("acc", SERVICE, "gone", "gone.txt", "file");
        gone.local_path = Some("gone.txt".into());
        gone.sync_state = "clean".into();
        store.upsert_item(&gone).unwrap();
        // a clean file still present locally
        let mut kept = Item::new("acc", SERVICE, "kept", "kept.txt", "file");
        kept.local_path = Some("kept.txt".into());
        kept.sync_state = "clean".into();
        store.upsert_item(&kept).unwrap();
        // a not-yet-downloaded (remote_dirty) file, also absent on disk
        let mut pending = Item::new("acc", SERVICE, "pending", "pending.txt", "file");
        pending.local_path = Some("pending.txt".into());
        pending.sync_state = "remote_dirty".into();
        store.upsert_item(&pending).unwrap();

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("kept.txt"), b"here").unwrap(); // only kept exists

        let deletes = scan_local_deletes(&store, "acc", dir.path()).unwrap();
        // gone.txt: clean + missing -> delete. kept.txt: present -> no.
        // pending.txt: missing but remote_dirty (never downloaded) -> NOT a delete.
        assert_eq!(deletes, vec!["gone".to_string()]);
    }

    /// Live local→remote: upload via the connector + GraphClient, confirm the
    /// store has a clean row, then push-delete (removes from OneDrive + tombstones).
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_push_upload_then_delete() {
        let _gate = crate::live_test_gate();
        let token = match std::env::var("ISYNCYOU_TEST_WRITE_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!(
                    "skipping live_push_upload_then_delete: ISYNCYOU_TEST_WRITE_TOKEN not set"
                );
                return;
            }
        };
        let store = Store::open_in_memory().unwrap();
        let mut map = MappingTable::new();
        let client = isyncyou_graph::GraphClient::new(token);
        let data = b"isyncyou connector push test".to_vec();
        let id = push_upload(
            &client,
            &store,
            &mut map,
            "testuser",
            "/iSyncYou-livetest/push.txt",
            &data,
            None,
        )
        .expect("push_upload should succeed");
        let it = store.get_item("testuser", SERVICE, &id).unwrap().unwrap();
        assert_eq!(it.sync_state, "clean");
        eprintln!("pushed item {id} (state={})", it.sync_state);
        push_delete(&client, &store, "testuser", &id, "2026-06-02T00:00:00Z")
            .expect("push_delete should succeed");
        assert!(store
            .get_item("testuser", SERVICE, &id)
            .unwrap()
            .unwrap()
            .deleted_at
            .is_some());
        eprintln!("deleted item {id}");
    }

    /// Live cross-process upload resume (plan §6/§9): "process 1" opens a resumable
    /// session and persists it (then dies before uploading); "process 2" — a fresh
    /// client — loads the persisted session and completes the upload **on the same
    /// uploadUrl** (no new session), proving resume survives a kill. Needs feature
    /// `http` + `ISYNCYOU_TEST_WRITE_TOKEN` (Files.ReadWrite).
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_upload_resume_survives_process_kill() {
        use isyncyou_graph::UploadResumeStore;
        let _gate = crate::live_test_gate();
        let token = match std::env::var("ISYNCYOU_TEST_WRITE_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!("skipping live_upload_resume_survives_process_kill: no write token");
                return;
            }
        };
        let dest = "/iSyncYou-livetest/resume-big.bin";
        let data = vec![0x5au8; 1_200_000]; // ~1.15 MiB → multi-chunk at 320 KiB
        let store = Store::open_in_memory().unwrap();
        let resume = StoreResume {
            store: &store,
            account: "testuser",
        };

        // Process 1: open the session + persist it, then "die" (upload nothing).
        let client1 = isyncyou_graph::GraphClient::new(token.clone());
        let s = client1
            .create_upload_session(dest, data.len() as u64)
            .expect("create session");
        resume.save(dest, &s.upload_url, data.len() as u64, 0);
        let persisted = store
            .get_upload_session("testuser", SERVICE, dest)
            .unwrap()
            .expect("session persisted before kill");
        assert_eq!(persisted.0, s.upload_url, "persisted the session uploadUrl");

        // Process 2: a fresh client resumes the persisted session and completes.
        let client2 = isyncyou_graph::GraphClient::new(token);
        let item = client2
            .upload_file_resumable(dest, &data, 320 * 1024, &resume)
            .expect("resumed upload should complete");
        // full file uploaded (server-reported size matches)
        assert_eq!(
            item.get("size").and_then(Value::as_u64),
            Some(data.len() as u64),
            "resumed upload uploaded the whole file"
        );
        // session cleared on completion
        assert!(store
            .get_upload_session("testuser", SERVICE, dest)
            .unwrap()
            .is_none());
        let id = item.get("id").and_then(Value::as_str).unwrap().to_string();
        eprintln!(
            "resumed + completed upload as item {id} ({} bytes)",
            data.len()
        );
        client2.delete_item(&id).ok();
    }

    /// Live end-to-end: real OneDrive delta -> store, against the throwaway
    /// account. Needs feature `http` + `ISYNCYOU_TEST_TOKEN` (Files.Read).
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_incremental_sync() {
        let _gate = crate::live_test_gate();
        let token = match std::env::var("ISYNCYOU_TEST_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!("skipping live_incremental_sync: ISYNCYOU_TEST_TOKEN not set");
                return;
            }
        };
        let store = Store::open_in_memory().unwrap();
        let arch = tempfile::tempdir().unwrap();
        let mut map = MappingTable::new();
        let mut client = isyncyou_graph::GraphClient::new(token);
        let report = incremental_sync(
            &mut client,
            &store,
            &mut map,
            "testuser",
            "2026-06-02T00:00:00Z",
            arch.path(),
        )
        .expect("live incremental sync should succeed");
        assert!(report.upserted > 0, "expected to ingest some items");
        assert!(store
            .get_delta_cursor("testuser", SERVICE, "")
            .unwrap()
            .is_some());
        eprintln!(
            "live sync: upserted={} deleted={} skipped={}",
            report.upserted, report.deleted, report.skipped
        );
    }

    /// Live remote→local: delta-sync the real drive, then materialize it to a temp
    /// sync root and confirm at least one file landed on disk with content.
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_materialize_downloads() {
        let _gate = crate::live_test_gate();
        let token = match std::env::var("ISYNCYOU_TEST_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!("skipping live_materialize_downloads: ISYNCYOU_TEST_TOKEN not set");
                return;
            }
        };
        let store = Store::open_in_memory().unwrap();
        let arch = tempfile::tempdir().unwrap();
        let mut map = MappingTable::new();
        let mut client = isyncyou_graph::GraphClient::new(token);
        incremental_sync(&mut client, &store, &mut map, "testuser", "t", arch.path())
            .expect("live sync should succeed");
        let dir = tempfile::tempdir().unwrap();
        let report = materialize_downloads(&store, &client, "testuser", dir.path(), "host")
            .expect("materialize should succeed");
        eprintln!(
            "live materialize: downloaded={} dirs={} failed={}",
            report.downloaded, report.dirs_created, report.failed
        );
        // find a materialized file and confirm it has content
        let mut found = false;
        for entry in walkdir(dir.path()) {
            if entry.is_file()
                && std::fs::metadata(&entry)
                    .map(|m| m.len() > 0)
                    .unwrap_or(false)
            {
                found = true;
                break;
            }
        }
        assert!(
            found || report.downloaded == 0,
            "files were downloaded but none landed on disk with content"
        );
    }

    /// Tiny recursive file walk for the live test (avoids a walkdir dependency).
    #[cfg(feature = "http")]
    fn walkdir(root: &std::path::Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            if let Ok(rd) = std::fs::read_dir(&d) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        stack.push(p);
                    } else {
                        out.push(p);
                    }
                }
            }
        }
        out
    }

    /// Live end-to-end remote→local **delete**: upload a throwaway file, sync +
    /// materialize it to disk, delete it on OneDrive, sync again, then confirm the
    /// local copy is moved to the trash. Needs `ISYNCYOU_TEST_WRITE_TOKEN`.
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_remote_delete_moves_local_to_trash() {
        let _gate = crate::live_test_gate();
        let token = match std::env::var("ISYNCYOU_TEST_WRITE_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!(
                    "skipping live_remote_delete_moves_local_to_trash: ISYNCYOU_TEST_WRITE_TOKEN not set"
                );
                return;
            }
        };
        let mut client = isyncyou_graph::GraphClient::new(token);
        let store = Store::open_in_memory().unwrap();
        let arch = tempfile::tempdir().unwrap();
        let mut map = MappingTable::new();
        let dest = "/iSyncYou-deltest/del-me.txt";

        // create the file remotely, then sync + materialize it to a temp sync root
        let item = client
            .upload(dest, b"delete me")
            .expect("upload should succeed");
        let id = item.get("id").and_then(Value::as_str).unwrap().to_string();
        incremental_sync(&mut client, &store, &mut map, "acc", "t1", arch.path())
            .expect("sync should succeed");
        let base = tempfile::tempdir().unwrap();
        let sync_root = base.path().join("od");
        let trash_root = base.path().join("trash");
        materialize_downloads(&store, &client, "acc", &sync_root, "host").expect("materialize");
        let local = sync_root.join("iSyncYou-deltest").join("del-me.txt");
        assert!(local.exists(), "file should have materialized to disk");

        // delete it on OneDrive, sync again -> tombstone -> local moves to trash
        client.delete(&id).expect("remote delete should succeed");
        incremental_sync(&mut client, &store, &mut map, "acc", "t2", arch.path())
            .expect("second sync should succeed");
        let pending = pending_local_deletes(&store, "acc", &sync_root).unwrap();
        assert!(
            pending.iter().any(|p| p.remote_id == id),
            "the deleted file should be a pending local delete"
        );
        let moved = apply_local_deletes(&sync_root, &trash_root, &pending).unwrap();
        assert!(moved >= 1);
        assert!(
            !local.exists(),
            "local copy should be gone from the sync root"
        );
        assert!(
            trash_root
                .join("iSyncYou-deltest")
                .join("del-me.txt")
                .exists(),
            "local copy should be in the trash"
        );
        eprintln!("remote delete of {id} moved {moved} local item(s) to trash");
    }

    /// Live local→remote create: drop a new file in a temp sync root, scan +
    /// push it, confirm it exists on OneDrive, then delete it. Needs the write
    /// token (`Files.ReadWrite`).
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_local_create_uploads_to_cloud() {
        use isyncyou_graph::Transport;
        let _gate = crate::live_test_gate();
        let token = match std::env::var("ISYNCYOU_TEST_WRITE_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!(
                    "skipping live_local_create_uploads_to_cloud: ISYNCYOU_TEST_WRITE_TOKEN not set"
                );
                return;
            }
        };
        let mut client = isyncyou_graph::GraphClient::new(token);
        let store = Store::open_in_memory().unwrap();
        let mut map = MappingTable::new();
        let dir = tempfile::tempdir().unwrap();
        let sync_root = dir.path();
        std::fs::create_dir_all(sync_root.join("iSyncYou-createtest")).unwrap();
        std::fs::write(
            sync_root.join("iSyncYou-createtest").join("up.txt"),
            b"created locally",
        )
        .unwrap();

        let creates = scan_local_creates(&store, "acc", sync_root).unwrap();
        assert!(creates.contains(&PathBuf::from("iSyncYou-createtest/up.txt")));
        let uploaded =
            push_local_creates(&client, &store, &mut map, "acc", sync_root, &creates).unwrap();
        assert!(uploaded >= 1);

        // the store now tracks the uploaded item (push ingests it); verify on cloud
        let up = store
            .items_by_service("acc", SERVICE)
            .unwrap()
            .into_iter()
            .find(|i| i.name == "up.txt")
            .expect("uploaded item should be tracked");
        let got = client
            .get(&format!(
                "https://graph.microsoft.com/v1.0/me/drive/items/{}",
                up.remote_id
            ))
            .body
            .expect("GET uploaded item");
        assert_eq!(got.get("name").and_then(Value::as_str), Some("up.txt"));
        eprintln!("uploaded new local file -> cloud item {}", up.remote_id);
        client.delete(&up.remote_id).expect("cleanup");
    }

    /// Live local→remote modify: upload a file, sync + materialize it, edit it
    /// locally (changing its size), then push the modify with an If-Match guard
    /// and confirm the cloud content updated. Needs the write token.
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_local_modify_replaces_cloud_content() {
        let _gate = crate::live_test_gate();
        let token = match std::env::var("ISYNCYOU_TEST_WRITE_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!(
                    "skipping live_local_modify_replaces_cloud_content: ISYNCYOU_TEST_WRITE_TOKEN not set"
                );
                return;
            }
        };
        let mut client = isyncyou_graph::GraphClient::new(token);
        let store = Store::open_in_memory().unwrap();
        let mut map = MappingTable::new();
        let item = client
            .upload("/iSyncYou-modtest/m.txt", b"original")
            .expect("upload");
        let id = item.get("id").and_then(Value::as_str).unwrap().to_string();

        let arch = tempfile::tempdir().unwrap();
        incremental_sync(&mut client, &store, &mut map, "acc", "t1", arch.path()).expect("sync");
        let base = tempfile::tempdir().unwrap();
        let sync_root = base.path().join("od");
        materialize_downloads(&store, &client, "acc", &sync_root, "host").expect("materialize");
        let local = sync_root.join("iSyncYou-modtest").join("m.txt");
        assert!(local.exists(), "file should have materialized");

        // edit locally (size changes) and push the modify
        let new_content = b"original + MODIFIED locally";
        std::fs::write(&local, new_content).unwrap();
        let modifies = scan_local_modifies(&store, "acc", &sync_root).unwrap();
        assert!(
            modifies.iter().any(|(mid, _, _)| mid == &id),
            "the edited file should be a detected modify"
        );
        let report = apply_local_modifies(
            &client, &store, &mut map, "acc", &sync_root, "host", &modifies,
        )
        .unwrap();
        assert!(report.uploaded >= 1, "modify should upload: {report:?}");

        // confirm the cloud content now matches the local edit
        let cloud = client.download_content(&id).expect("download");
        assert_eq!(
            cloud, new_content,
            "cloud content should match the local edit"
        );
        eprintln!("modify uploaded; cloud content updated for {id}");
        client.delete(&id).expect("cleanup");
    }

    /// Live conflict keep-both: upload + materialize a file, advance the cloud
    /// copy out-of-band (so the stored etag goes stale), then edit locally and push
    /// the modify. The If-Match must 412; the engine must keep both — move the local
    /// edit to a `*-host-safeBackup-NNNN` copy and re-mark the item remote_dirty,
    /// never overwriting the cloud (plan §10 / A3). Needs the write token.
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_modify_conflict_keeps_both() {
        let _gate = crate::live_test_gate();
        let token = match std::env::var("ISYNCYOU_TEST_WRITE_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!(
                    "skipping live_modify_conflict_keeps_both: ISYNCYOU_TEST_WRITE_TOKEN not set"
                );
                return;
            }
        };
        let mut client = isyncyou_graph::GraphClient::new(token);
        let store = Store::open_in_memory().unwrap();
        let mut map = MappingTable::new();
        let item = client
            .upload("/iSyncYou-conflicttest/c.txt", b"original")
            .expect("upload");
        let id = item.get("id").and_then(Value::as_str).unwrap().to_string();

        let arch = tempfile::tempdir().unwrap();
        incremental_sync(&mut client, &store, &mut map, "acc", "t1", arch.path()).expect("sync");
        let base = tempfile::tempdir().unwrap();
        let sync_root = base.path().join("od");
        materialize_downloads(&store, &client, "acc", &sync_root, "host").expect("materialize");
        let local = sync_root.join("iSyncYou-conflicttest").join("c.txt");
        assert!(local.exists());

        // the etag the store recorded — the engine will send this with If-Match
        let stale_etag = store
            .get_item("acc", SERVICE, &id)
            .unwrap()
            .unwrap()
            .etag
            .expect("stored etag");
        // advance the cloud copy out-of-band so that etag is now stale
        client
            .replace_content_if_match(&id, b"cloud-side change wins", &stale_etag)
            .expect("out-of-band replace")
            .expect("etag should still match for the out-of-band write");

        // now edit locally and push — must hit a 412 and keep both
        std::fs::write(&local, b"my local edit").unwrap();
        let modifies = scan_local_modifies(&store, "acc", &sync_root).unwrap();
        let report = apply_local_modifies(
            &client, &store, &mut map, "acc", &sync_root, "host", &modifies,
        )
        .unwrap();
        assert_eq!(
            report.uploaded, 0,
            "must NOT overwrite the cloud: {report:?}"
        );
        assert_eq!(
            report.conflicts, 1,
            "must register one conflict: {report:?}"
        );

        // cloud keeps its out-of-band content (no silent overwrite)
        let cloud = client.download_content(&id).expect("download");
        assert_eq!(cloud, b"cloud-side change wins");
        // local edit preserved as a conflict copy; original path freed + remote_dirty
        let dir = sync_root.join("iSyncYou-conflicttest");
        assert!(!local.exists(), "original must be moved aside");
        let copy = dir.join("c-host-safeBackup-0001.txt");
        assert_eq!(std::fs::read(&copy).unwrap(), b"my local edit");
        assert_eq!(
            store
                .get_item("acc", SERVICE, &id)
                .unwrap()
                .unwrap()
                .sync_state,
            "remote_dirty"
        );
        eprintln!("conflict kept both: cloud preserved, local edit -> {copy:?}");
        client.delete(&id).expect("cleanup");
    }

    /// Live local→remote delete: upload a file, sync + materialize it, remove it
    /// locally, then scan + push the deletion and confirm it's gone on OneDrive.
    /// Needs the write token.
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_local_delete_removes_from_cloud() {
        use isyncyou_graph::Transport;
        let _gate = crate::live_test_gate();
        let token = match std::env::var("ISYNCYOU_TEST_WRITE_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!(
                    "skipping live_local_delete_removes_from_cloud: ISYNCYOU_TEST_WRITE_TOKEN not set"
                );
                return;
            }
        };
        let mut client = isyncyou_graph::GraphClient::new(token);
        let store = Store::open_in_memory().unwrap();
        let mut map = MappingTable::new();
        let item = client
            .upload("/iSyncYou-deltest2/d.txt", b"to be deleted")
            .expect("upload");
        let id = item.get("id").and_then(Value::as_str).unwrap().to_string();
        let arch = tempfile::tempdir().unwrap();

        incremental_sync(&mut client, &store, &mut map, "acc", "t1", arch.path()).expect("sync");
        let base = tempfile::tempdir().unwrap();
        let sync_root = base.path().join("od");
        materialize_downloads(&store, &client, "acc", &sync_root, "host").expect("materialize");
        let local = sync_root.join("iSyncYou-deltest2").join("d.txt");
        assert!(local.exists(), "file should have materialized");

        // user deletes it locally → scan + push the cloud deletion
        std::fs::remove_file(&local).unwrap();
        let deletes = scan_local_deletes(&store, "acc", &sync_root).unwrap();
        assert!(
            deletes.contains(&id),
            "the removed file should be a detected local delete"
        );
        for did in &deletes {
            push_delete(&client, &store, "acc", did, "t2").expect("cloud delete");
        }

        // confirm it's gone on the cloud (404)
        let resp = client.get(&format!(
            "https://graph.microsoft.com/v1.0/me/drive/items/{id}"
        ));
        assert_eq!(resp.status, 404, "item should be gone on the cloud");
        eprintln!("local deletion mirrored: cloud item {id} is gone");
    }
}
