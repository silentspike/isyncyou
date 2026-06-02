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

    /// Live local→remote: upload via the connector + GraphClient, confirm the
    /// store has a clean row, then push-delete (removes from OneDrive + tombstones).
    #[cfg(feature = "http")]
    #[test]
    fn live_push_upload_then_delete() {
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
}
