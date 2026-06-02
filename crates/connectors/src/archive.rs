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

/// Archive the canonical JSON of every stored `(service, item_type)` item that
/// has no local body yet. `url_for(item)` builds the GET URL for an item's
/// canonical resource. Writes `<archive_root>/<service>/aa/bb/<hash>.json`
/// atomically (tmp+rename), records `local_path`, skips already-archived items,
/// and honors `limit` (`0` = no limit) so it resumes across passes.
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
    let mut report = ArchiveReport::default();
    for item in store.items_by_type(account, service, item_type)? {
        if item.local_path.is_some() {
            report.skipped += 1;
            continue;
        }
        if limit != 0 && report.archived >= limit {
            break;
        }
        let json = fetcher
            .fetch_json(&url_for(&item))
            .map_err(SyncError::Remote)?;
        let bytes =
            serde_json::to_vec_pretty(&json).map_err(|e| SyncError::Malformed(e.to_string()))?;
        let abs = shard_path(archive_root, service, &item.remote_id, "json");
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = abs.with_extension("json.part");
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

    /// Live: index the calendar, then archive a few events' canonical JSON and
    /// confirm each file is valid JSON with an `id`. Needs feature `http` +
    /// `ISYNCYOU_TEST_TOKEN` (`Calendars.Read`).
    #[cfg(feature = "http")]
    #[test]
    fn live_archive_calendar_bodies() {
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
            "backupslave",
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
        let r = backup_calendar_bodies(&client, &store, "backupslave", dir.path(), 3)
            .expect("archive should succeed");
        assert!(r.archived >= 1, "expected to archive at least one event");

        let one = store
            .items_by_type("backupslave", "calendar", "event")
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
}
