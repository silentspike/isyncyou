//! OneDrive connector — ingests a Graph delta walk into the store (plan §6).
//!
//! This is the **remote → local** half of the bidirectional sync: it walks the
//! `/me/drive/root/delta` query (via [`isyncyou_graph::run_delta`]), upserts each
//! item into the [`Store`] keyed by id, records tombstones for removed items,
//! maps cloud names to local names via [`MappingTable`], and persists the new
//! delta cursor. The local → remote upload half (driving uploads from local
//! changes) layers on top using the same crates.

use isyncyou_graph::{run_delta, DeltaCursor, DeltaError, Transport};
use isyncyou_pathmap::MappingTable;
use isyncyou_store::{Item, Store, StoreError};
use serde_json::Value;
use std::collections::HashMap;
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
        match ingest_item(store, map, account, item, now, "remote_dirty")? {
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
) -> Result<Ingest, SyncError> {
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| SyncError::Malformed("item has no id".into()))?;

    // Tombstone: the `deleted` facet (or the legacy `@removed`) marks removal.
    if item.get("deleted").is_some() || item.get("@removed").is_some() {
        store.mark_deleted(account, SERVICE, id, now)?;
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
    Ok(Ingest::Upserted)
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
    ingest_item(store, map, account, &to_ingest, "", "clean")?;
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
}

#[cfg(feature = "http")]
impl Downloader for isyncyou_graph::GraphClient {
    fn download(&self, remote_id: &str) -> Result<Vec<u8>, String> {
        self.download_content(remote_id).map_err(|e| e.to_string())
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
    std::fs::write(&tmp, data)?;
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
    match (shash, std::fs::read(full)) {
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
    if it.size != Some(meta.len() as i64) {
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
fn cloud_dest_path(rel: &Path) -> String {
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
        let data = std::fs::read(&full)?;
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
    let meta = match std::fs::metadata(full) {
        Ok(m) => m,
        Err(_) => return false, // not on disk → not a modify (delete is handled elsewhere)
    };
    if local_file_matches(full, it) {
        return false;
    }
    if it.size != Some(meta.len() as i64) {
        return true;
    }
    // size matches but mtime differs: only a content hash can decide.
    match (&it.quickxorhash, std::fs::read(full)) {
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
        let data = match std::fs::read(sync_root.join(rel)) {
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
                ingest_item(store, map, account, &to_ingest, "", "clean")?;
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
        let mut map = MappingTable::new();
        // an item that already existed and will be tombstoned by the delta
        store
            .upsert_item(&Item::new("acc", SERVICE, "gone1", "old.txt", "file"))
            .unwrap();
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

        let report =
            incremental_sync(&mut t, &store, &mut map, "acc", "2026-06-02T00:00:00Z").unwrap();
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

        // tombstone recorded
        assert!(store
            .get_item("acc", SERVICE, "gone1")
            .unwrap()
            .unwrap()
            .deleted_at
            .is_some());
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
        let mut map = MappingTable::new();
        // first run sets a cursor
        let p1 = json!({ "value": [file_item("a1","a.txt","root1")], "@odata.deltaLink": "C1" });
        let mut t1 = MockTransport(vec![Response::ok(p1)], 0);
        incremental_sync(&mut t1, &store, &mut map, "acc", "t").unwrap();
        // second run: two pages, then deltaLink
        let p2a = json!({ "value": [file_item("b1","b.txt","root1")], "@odata.nextLink": "u2" });
        let p2b = json!({ "value": [file_item("c1","c.txt","root1")], "@odata.deltaLink": "C2" });
        let mut t2 = MockTransport(vec![Response::ok(p2a), Response::ok(p2b)], 0);
        let r = incremental_sync(&mut t2, &store, &mut map, "acc", "t").unwrap();
        assert_eq!(r.upserted, 2);
        assert_eq!(
            store
                .get_delta_cursor("acc", SERVICE, "")
                .unwrap()
                .as_deref(),
            Some("C2")
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
        let mut map = MappingTable::new();
        let mut client = isyncyou_graph::GraphClient::new(token);
        let report = incremental_sync(
            &mut client,
            &store,
            &mut map,
            "testuser",
            "2026-06-02T00:00:00Z",
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
        let mut map = MappingTable::new();
        let mut client = isyncyou_graph::GraphClient::new(token);
        incremental_sync(&mut client, &store, &mut map, "testuser", "t")
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
        let mut map = MappingTable::new();
        let dest = "/iSyncYou-deltest/del-me.txt";

        // create the file remotely, then sync + materialize it to a temp sync root
        let item = client
            .upload(dest, b"delete me")
            .expect("upload should succeed");
        let id = item.get("id").and_then(Value::as_str).unwrap().to_string();
        incremental_sync(&mut client, &store, &mut map, "acc", "t1").expect("sync should succeed");
        let base = tempfile::tempdir().unwrap();
        let sync_root = base.path().join("od");
        let trash_root = base.path().join("trash");
        materialize_downloads(&store, &client, "acc", &sync_root, "host").expect("materialize");
        let local = sync_root.join("iSyncYou-deltest").join("del-me.txt");
        assert!(local.exists(), "file should have materialized to disk");

        // delete it on OneDrive, sync again -> tombstone -> local moves to trash
        client.delete(&id).expect("remote delete should succeed");
        incremental_sync(&mut client, &store, &mut map, "acc", "t2")
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

        incremental_sync(&mut client, &store, &mut map, "acc", "t1").expect("sync");
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

        incremental_sync(&mut client, &store, &mut map, "acc", "t1").expect("sync");
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

        incremental_sync(&mut client, &store, &mut map, "acc", "t1").expect("sync");
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
