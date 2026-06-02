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
) -> Result<String, SyncError> {
    let item = writer.upload(dest_path, data).map_err(SyncError::Remote)?;
    ingest_item(store, map, account, &item, "", "clean")?;
    item.get("id")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| SyncError::Malformed("upload response had no id".into()))
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
}

/// Guard against a malformed parent cycle when walking to the drive root.
const MAX_PATH_DEPTH: usize = 256;

/// Build an item's path **relative to the sync root** by walking `parent_remote_id`
/// up through `by_id`, collecting each ancestor's mapped local name. Stops at the
/// drive root (whose id is absent from the store). `None` if an item on the chain
/// has no local name or a cycle is hit.
fn local_rel_path(by_id: &HashMap<&str, &Item>, it: &Item) -> Option<PathBuf> {
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

/// Materialize **remote-dirty** OneDrive items to disk under `sync_root`: create
/// folders, download file content, write it atomically, and mark each item
/// `clean` so a re-run skips it. This is the missing half of the remote→local
/// sync (ingest records metadata; this writes the actual files).
///
/// v1 scope: downloads + folder creation only. Local deletion of removed items
/// (with the mass-delete guard) and quickXorHash-based skip are follow-ups; the
/// `clean` state-marking already prevents redundant re-downloads.
pub fn materialize_downloads<D: Downloader>(
    store: &Store,
    downloader: &D,
    account: &str,
    sync_root: &Path,
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
            } else {
                if let Some(parent) = full.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match downloader.download(&it.remote_id) {
                    Ok(bytes) => match atomic_write(&full, &bytes) {
                        Ok(()) => {
                            store.set_sync_state(account, SERVICE, &it.remote_id, "clean")?;
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
    }
    impl MockWriter {
        fn new() -> Self {
            MockWriter {
                uploaded: Default::default(),
                deleted: Default::default(),
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
    }

    #[test]
    fn push_upload_stores_clean_item() {
        let store = Store::open_in_memory().unwrap();
        let mut map = MappingTable::new();
        let w = MockWriter::new();
        let id = push_upload(&w, &store, &mut map, "acc", "/Docs/note.txt", b"hello").unwrap();
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
        store.upsert_item(&file).unwrap();

        let dl = MockDownloader(
            [("a1".to_string(), b"JPEGDATA".to_vec())]
                .into_iter()
                .collect(),
        );
        let dir = tempfile::tempdir().unwrap();
        let report = materialize_downloads(&store, &dl, "acc", dir.path()).unwrap();
        assert_eq!(report.downloaded, 1);
        assert_eq!(report.dirs_created, 1);
        assert_eq!(report.failed, 0);

        // the file is on disk under Photos/ with the right content
        let path = dir.path().join("Photos").join("IMG.jpg");
        assert_eq!(std::fs::read(&path).unwrap(), b"JPEGDATA");
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
        let r2 = materialize_downloads(&store, &dl, "acc", dir.path()).unwrap();
        assert_eq!(r2.downloaded, 0);
        assert_eq!(r2.dirs_created, 0);
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
        let r = materialize_downloads(&store, &dl, "acc", dir.path()).unwrap();
        assert_eq!(r.failed, 1);
        assert_eq!(r.downloaded, 0);
    }

    /// Live local→remote: upload via the connector + GraphClient, confirm the
    /// store has a clean row, then push-delete (removes from OneDrive + tombstones).
    #[cfg(feature = "http")]
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
            "backupslave",
            "/iSyncYou-livetest/push.txt",
            &data,
        )
        .expect("push_upload should succeed");
        let it = store
            .get_item("backupslave", SERVICE, &id)
            .unwrap()
            .unwrap();
        assert_eq!(it.sync_state, "clean");
        eprintln!("pushed item {id} (state={})", it.sync_state);
        push_delete(&client, &store, "backupslave", &id, "2026-06-02T00:00:00Z")
            .expect("push_delete should succeed");
        assert!(store
            .get_item("backupslave", SERVICE, &id)
            .unwrap()
            .unwrap()
            .deleted_at
            .is_some());
        eprintln!("deleted item {id}");
    }

    /// Live end-to-end: real OneDrive delta -> store, against the throwaway
    /// account. Needs feature `http` + `ISYNCYOU_TEST_TOKEN` (Files.Read).
    #[cfg(feature = "http")]
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
            "backupslave",
            "2026-06-02T00:00:00Z",
        )
        .expect("live incremental sync should succeed");
        assert!(report.upserted > 0, "expected to ingest some items");
        assert!(store
            .get_delta_cursor("backupslave", SERVICE, "")
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
        incremental_sync(&mut client, &store, &mut map, "backupslave", "t")
            .expect("live sync should succeed");
        let dir = tempfile::tempdir().unwrap();
        let report = materialize_downloads(&store, &client, "backupslave", dir.path())
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
}
