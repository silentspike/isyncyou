//! Content archive — write each item's canonical JSON body to disk (plan §6/§9).
//!
//! The delta connectors store an *index* (metadata) in SQLite; the canonical
//! body — the JSON a restore re-creates from — lives on disk. This module
//! fetches each stored item's JSON by id and writes it (atomically, in the same
//! sharded layout as mail `.eml`) under the account's `archive_root`, recording
//! the relative path as `local_path`. It is the on-disk source the restore
//! engine reads (`restore_event`/`restore_task`/`restore_contact`).

use crate::common::shard_path;
use crate::onedrive::SyncError;
use isyncyou_store::{Item, Store};
use serde_json::Value;
use std::path::Path;

/// Fetches a Graph resource's canonical JSON by URL. Abstracted so the archive
/// driver is unit-testable with a mock and live-tested with the real client.
pub trait JsonFetcher {
    fn fetch_json(&self, url: &str) -> Result<Value, String>;
}

#[cfg(feature = "http")]
impl JsonFetcher for isyncyou_graph::GraphClient {
    fn fetch_json(&self, url: &str) -> Result<Value, String> {
        self.get_json(url).map_err(|e| e.to_string())
    }
}

/// Fetches a Graph resource's raw bytes by URL (for non-JSON bodies such as
/// OneNote page HTML). Abstracted so the archive driver is mockable.
pub trait BytesFetcher {
    fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, String>;
}

#[cfg(feature = "http")]
impl BytesFetcher for isyncyou_graph::GraphClient {
    fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
        self.get_bytes(url).map_err(|e| e.to_string())
    }
}

/// What one archive pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArchiveReport {
    /// Items whose JSON was fetched and written this pass.
    pub archived: usize,
    /// Items skipped because their body was already on disk.
    pub skipped: usize,
    /// Total bytes written this pass.
    pub bytes: u64,
}

/// Shared archive core: for each stored `(service, item_type)` item without a
/// local body, fetch its bytes via `fetch`, write `<archive_root>/<service>/
/// aa/bb/<hash>.<ext>` atomically (tmp+rename), record `local_path`, skip
/// already-archived items, and honor `limit` (`0` = no limit) so it resumes.
#[allow(clippy::too_many_arguments)]
fn archive_bodies<G>(
    store: &Store,
    account: &str,
    service: &str,
    item_type: &str,
    archive_root: &Path,
    ext: &str,
    limit: usize,
    fetch: G,
) -> Result<ArchiveReport, SyncError>
where
    G: Fn(&Item) -> Result<Vec<u8>, String>,
{
    let mut report = ArchiveReport::default();
    for item in store.items_by_type(account, service, item_type)? {
        if item.local_path.is_some() {
            report.skipped += 1;
            continue;
        }
        if limit != 0 && report.archived >= limit {
            break;
        }
        let bytes = fetch(&item).map_err(SyncError::Remote)?;
        let abs = shard_path(archive_root, service, &item.remote_id, ext);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = abs.with_extension(format!("{ext}.part"));
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &abs)?;
        let rel = abs.strip_prefix(archive_root).unwrap_or(&abs);
        store.set_local_path(
            account,
            service,
            &item.remote_id,
            Some(&rel.to_string_lossy()),
        )?;
        report.archived += 1;
        report.bytes += bytes.len() as u64;
    }
    Ok(report)
}

/// Archive the canonical **JSON** of every stored `(service, item_type)` item
/// lacking a local body. `url_for(item)` builds the GET URL for the item's
/// canonical resource; the response is pretty-printed to a `.json` file.
#[allow(clippy::too_many_arguments)]
pub fn backup_json_bodies<F, U>(
    fetcher: &F,
    store: &Store,
    account: &str,
    service: &str,
    item_type: &str,
    archive_root: &Path,
    url_for: U,
    limit: usize,
) -> Result<ArchiveReport, SyncError>
where
    F: JsonFetcher,
    U: Fn(&Item) -> String,
{
    archive_bodies(
        store,
        account,
        service,
        item_type,
        archive_root,
        "json",
        limit,
        |item| {
            let json = fetcher.fetch_json(&url_for(item))?;
            serde_json::to_vec_pretty(&json).map_err(|e| e.to_string())
        },
    )
}

/// Archive the raw **bytes** of every stored `(service, item_type)` item lacking
/// a local body, written with extension `ext` (e.g. OneNote page HTML).
#[allow(clippy::too_many_arguments)]
pub fn backup_byte_bodies<F, U>(
    fetcher: &F,
    store: &Store,
    account: &str,
    service: &str,
    item_type: &str,
    archive_root: &Path,
    ext: &str,
    url_for: U,
    limit: usize,
) -> Result<ArchiveReport, SyncError>
where
    F: BytesFetcher,
    U: Fn(&Item) -> String,
{
    archive_bodies(
        store,
        account,
        service,
        item_type,
        archive_root,
        ext,
        limit,
        |item| fetcher.fetch_bytes(&url_for(item)),
    )
}

/// Archive calendar-event JSON (`GET /me/events/{id}`).
pub fn backup_calendar_bodies<F: JsonFetcher>(
    fetcher: &F,
    store: &Store,
    account: &str,
    archive_root: &Path,
    limit: usize,
) -> Result<ArchiveReport, SyncError> {
    backup_json_bodies(
        fetcher,
        store,
        account,
        "calendar",
        "event",
        archive_root,
        |it| format!("/me/events/{}", it.remote_id),
        limit,
    )
}

/// Archive contact JSON (`GET /me/contacts/{id}`).
pub fn backup_contacts_bodies<F: JsonFetcher>(
    fetcher: &F,
    store: &Store,
    account: &str,
    archive_root: &Path,
    limit: usize,
) -> Result<ArchiveReport, SyncError> {
    backup_json_bodies(
        fetcher,
        store,
        account,
        "contacts",
        "contact",
        archive_root,
        |it| format!("/me/contacts/{}", it.remote_id),
        limit,
    )
}

/// Archive todo-task JSON (`GET /me/todo/lists/{list}/tasks/{id}`). A task's
/// parent list id is its `parent_remote_id`.
pub fn backup_todo_bodies<F: JsonFetcher>(
    fetcher: &F,
    store: &Store,
    account: &str,
    archive_root: &Path,
    limit: usize,
) -> Result<ArchiveReport, SyncError> {
    backup_json_bodies(
        fetcher,
        store,
        account,
        "todo",
        "task",
        archive_root,
        |it| {
            let list = it.parent_remote_id.as_deref().unwrap_or_default();
            format!("/me/todo/lists/{list}/tasks/{}", it.remote_id)
        },
        limit,
    )
}

/// Archive OneNote page **HTML** (`GET /me/onenote/pages/{id}/content`). Page
/// resources (images) referenced by the HTML are a later refinement.
pub fn backup_onenote_bodies<F: BytesFetcher>(
    fetcher: &F,
    store: &Store,
    account: &str,
    archive_root: &Path,
    limit: usize,
) -> Result<ArchiveReport, SyncError> {
    backup_byte_bodies(
        fetcher,
        store,
        account,
        "onenote",
        "page",
        archive_root,
        "html",
        |it| format!("/me/onenote/pages/{}/content", it.remote_id),
        limit,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    /// Records the GET urls and echoes each back inside the returned JSON.
    struct MockJson {
        urls: RefCell<Vec<String>>,
    }
    impl MockJson {
        fn new() -> Self {
            MockJson {
                urls: RefCell::new(Vec::new()),
            }
        }
    }
    impl JsonFetcher for MockJson {
        fn fetch_json(&self, url: &str) -> Result<Value, String> {
            self.urls.borrow_mut().push(url.to_string());
            Ok(json!({ "id": "echoed", "url": url }))
        }
    }

    fn event_item(store: &Store, id: &str) {
        let mut it = Item::new("acc", "calendar", id, "Event", "event");
        it.parent_remote_id = Some("C1".into());
        store.upsert_item(&it).unwrap();
    }

    #[test]
    fn archives_event_json_and_records_local_path() {
        let store = Store::open_in_memory().unwrap();
        event_item(&store, "e1");
        event_item(&store, "e2");
        let dir = tempfile::tempdir().unwrap();
        let m = MockJson::new();

        let r = backup_calendar_bodies(&m, &store, "acc", dir.path(), 0).unwrap();
        assert_eq!(r.archived, 2);
        assert_eq!(r.skipped, 0);
        assert!(r.bytes > 0);
        assert!(m.urls.borrow().contains(&"/me/events/e1".to_string()));

        let e1 = store.get_item("acc", "calendar", "e1").unwrap().unwrap();
        let rel = e1.local_path.expect("local_path set");
        assert!(rel.starts_with("calendar/") && rel.ends_with(".json"));
        let bytes = std::fs::read(dir.path().join(&rel)).unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["url"], "/me/events/e1");
        assert!(!dir.path().join(&rel).with_extension("json.part").exists());

        // second pass skips already-archived bodies
        let r2 = backup_calendar_bodies(&m, &store, "acc", dir.path(), 0).unwrap();
        assert_eq!(r2.archived, 0);
        assert_eq!(r2.skipped, 2);
    }

    #[test]
    fn contacts_use_the_contacts_url() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_item(&Item::new("acc", "contacts", "c1", "Ada", "contact"))
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let m = MockJson::new();
        let r = backup_contacts_bodies(&m, &store, "acc", dir.path(), 0).unwrap();
        assert_eq!(r.archived, 1);
        assert_eq!(m.urls.borrow().as_slice(), &["/me/contacts/c1".to_string()]);
        let rel = store
            .get_item("acc", "contacts", "c1")
            .unwrap()
            .unwrap()
            .local_path
            .unwrap();
        assert!(rel.starts_with("contacts/"));
    }

    #[test]
    fn limit_caps_archive_per_pass() {
        let store = Store::open_in_memory().unwrap();
        event_item(&store, "e1");
        event_item(&store, "e2");
        let dir = tempfile::tempdir().unwrap();
        let m = MockJson::new();
        assert_eq!(
            backup_calendar_bodies(&m, &store, "acc", dir.path(), 1)
                .unwrap()
                .archived,
            1
        );
        let second = backup_calendar_bodies(&m, &store, "acc", dir.path(), 1).unwrap();
        assert_eq!(second.archived, 1);
        assert_eq!(second.skipped, 1);
    }

    /// Echoes fixed HTML bytes and records the GET urls.
    struct MockBytes {
        urls: RefCell<Vec<String>>,
    }
    impl MockBytes {
        fn new() -> Self {
            MockBytes {
                urls: RefCell::new(Vec::new()),
            }
        }
    }
    impl BytesFetcher for MockBytes {
        fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
            self.urls.borrow_mut().push(url.to_string());
            Ok(b"<html><body>page</body></html>".to_vec())
        }
    }

    #[test]
    fn todo_uses_the_list_scoped_task_url() {
        let store = Store::open_in_memory().unwrap();
        let mut t = Item::new("acc", "todo", "t1", "Write report", "task");
        t.parent_remote_id = Some("LIST9".into());
        store.upsert_item(&t).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let m = MockJson::new();
        let r = backup_todo_bodies(&m, &store, "acc", dir.path(), 0).unwrap();
        assert_eq!(r.archived, 1);
        assert_eq!(
            m.urls.borrow().as_slice(),
            &["/me/todo/lists/LIST9/tasks/t1".to_string()]
        );
        let rel = store
            .get_item("acc", "todo", "t1")
            .unwrap()
            .unwrap()
            .local_path
            .unwrap();
        assert!(rel.starts_with("todo/") && rel.ends_with(".json"));
    }

    #[test]
    fn onenote_archives_page_html() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_item(&Item::new("acc", "onenote", "p1", "Ideas", "page"))
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let m = MockBytes::new();
        let r = backup_onenote_bodies(&m, &store, "acc", dir.path(), 0).unwrap();
        assert_eq!(r.archived, 1);
        assert_eq!(
            m.urls.borrow().as_slice(),
            &["/me/onenote/pages/p1/content".to_string()]
        );
        let rel = store
            .get_item("acc", "onenote", "p1")
            .unwrap()
            .unwrap()
            .local_path
            .unwrap();
        assert!(rel.starts_with("onenote/") && rel.ends_with(".html"));
        let bytes = std::fs::read(dir.path().join(&rel)).unwrap();
        assert!(bytes.starts_with(b"<html>"));
    }

    /// Live: index the calendar, then archive a few events' canonical JSON and
    /// confirm each file is valid JSON with an `id`. Needs feature `http` +
    /// `ISYNCYOU_TEST_TOKEN` (`Calendars.Read`).
    #[cfg(feature = "http")]
    #[test]
    fn live_archive_calendar_bodies() {
        let _gate = crate::live_test_gate();
        let token = match std::env::var("ISYNCYOU_TEST_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!("skipping live_archive_calendar_bodies: ISYNCYOU_TEST_TOKEN not set");
                return;
            }
        };
        let store = Store::open_in_memory().unwrap();
        let mut client = isyncyou_graph::GraphClient::new(token);
        let idx = crate::incremental_sync_calendar(
            &mut client,
            &store,
            "testuser",
            "2019-01-01T00:00:00Z",
            "2030-01-01T00:00:00Z",
            "2026-06-02T00:00:00Z",
        )
        .expect("index sync should succeed");
        if idx.upserted == 0 {
            eprintln!("no events to archive; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let r = backup_calendar_bodies(&client, &store, "testuser", dir.path(), 3)
            .expect("archive should succeed");
        assert!(r.archived >= 1, "expected to archive at least one event");

        let one = store
            .items_by_type("testuser", "calendar", "event")
            .unwrap()
            .into_iter()
            .find(|i| i.local_path.is_some())
            .unwrap();
        let bytes = std::fs::read(dir.path().join(one.local_path.unwrap())).unwrap();
        let v: Value = serde_json::from_slice(&bytes).expect("archived file is valid JSON");
        assert!(
            v.get("id").and_then(Value::as_str).is_some(),
            "event JSON has an id"
        );
        eprintln!(
            "live calendar archive: archived={} bytes={}",
            r.archived, r.bytes
        );
    }

    /// Live: index ToDo, archive a few tasks' canonical JSON, confirm valid JSON
    /// with an `id`. Needs feature `http` + `ISYNCYOU_TEST_TOKEN` (`Tasks.Read`).
    #[cfg(feature = "http")]
    #[test]
    fn live_archive_todo_bodies() {
        let _gate = crate::live_test_gate();
        let token = match std::env::var("ISYNCYOU_TEST_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!("skipping live_archive_todo_bodies: ISYNCYOU_TEST_TOKEN not set");
                return;
            }
        };
        let store = Store::open_in_memory().unwrap();
        let mut client = isyncyou_graph::GraphClient::new(token);
        let idx =
            crate::incremental_sync_todo(&mut client, &store, "testuser", "2026-06-02T00:00:00Z")
                .expect("todo index sync should succeed");
        if idx.upserted == 0 {
            eprintln!("no tasks to archive; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let r = backup_todo_bodies(&client, &store, "testuser", dir.path(), 3)
            .expect("todo archive should succeed");
        assert!(r.archived >= 1);
        let one = store
            .items_by_type("testuser", "todo", "task")
            .unwrap()
            .into_iter()
            .find(|i| i.local_path.is_some())
            .unwrap();
        let bytes = std::fs::read(dir.path().join(one.local_path.unwrap())).unwrap();
        let v: Value = serde_json::from_slice(&bytes).expect("archived task is valid JSON");
        assert!(v.get("id").and_then(Value::as_str).is_some());
        eprintln!(
            "live todo archive: archived={} bytes={}",
            r.archived, r.bytes
        );
    }

    /// Live: index OneNote, archive page HTML. The throwaway account has no
    /// notebook, so this proves the walk runs (0 pages is a valid outcome).
    /// Needs feature `http` + `ISYNCYOU_TEST_TOKEN` (`Notes.Read`).
    #[cfg(feature = "http")]
    #[test]
    fn live_archive_onenote_bodies() {
        let _gate = crate::live_test_gate();
        let token = match std::env::var("ISYNCYOU_TEST_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!("skipping live_archive_onenote_bodies: ISYNCYOU_TEST_TOKEN not set");
                return;
            }
        };
        let store = Store::open_in_memory().unwrap();
        let mut client = isyncyou_graph::GraphClient::new(token);
        crate::incremental_sync_onenote(&mut client, &store, "testuser", "2026-06-02T00:00:00Z")
            .expect("onenote index sync should succeed");
        let dir = tempfile::tempdir().unwrap();
        let r = backup_onenote_bodies(&client, &store, "testuser", dir.path(), 3)
            .expect("onenote archive should succeed");
        // every archived page must be a non-empty .html file
        if r.archived > 0 {
            let one = store
                .items_by_type("testuser", "onenote", "page")
                .unwrap()
                .into_iter()
                .find(|i| i.local_path.is_some())
                .unwrap();
            let bytes = std::fs::read(dir.path().join(one.local_path.unwrap())).unwrap();
            assert!(!bytes.is_empty());
        }
        eprintln!(
            "live onenote archive: archived={} bytes={}",
            r.archived, r.bytes
        );
    }
}
