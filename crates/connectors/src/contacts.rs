//! Contacts connector — default + per-folder contact delta into the store (plan §6).
//!
//! Contacts live in a **default collection** (`/me/contacts`) plus any number of
//! named **contact folders** (`/me/contactFolders/{id}/contacts`). Both expose a
//! delta query, so this syncs the default collection first (cursor scope
//! [`DEFAULT_SCOPE`]) and then each folder (cursor scope = folder id). Contacts
//! are stored id-based (service `"contacts"`, `item_type = "contact"`); the
//! canonical record is the raw JSON, vCard is an export concern handled elsewhere.

use crate::common::fetch_pages;
use crate::onedrive::SyncError;
use isyncyou_graph::{run_delta, DeltaCursor, Transport};
use isyncyou_store::{Item, Store};
use serde_json::Value;

const SERVICE: &str = "contacts";
const FOLDERS_URL: &str = "https://graph.microsoft.com/v1.0/me/contactFolders?$top=100";
const DEFAULT_DELTA: &str = "https://graph.microsoft.com/v1.0/me/contacts/delta";
/// Cursor scope for the default (folderless) contacts collection. Distinct from
/// any folder id and from the empty whole-service scope.
const DEFAULT_SCOPE: &str = "_default";

/// What one contacts sync changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContactsReport {
    /// Named contact folders synced (excludes the default collection).
    pub folders: usize,
    pub upserted: usize,
    pub deleted: usize,
    pub skipped: usize,
}

struct ContactFolder {
    id: String,
    name: String,
}

fn parse_folders(raw: &[Value]) -> Vec<ContactFolder> {
    raw.iter()
        .filter_map(|f| {
            let id = f.get("id").and_then(Value::as_str)?.to_string();
            let name = f
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Some(ContactFolder { id, name })
        })
        .collect()
}

/// Sync the default contacts collection and every contact folder incrementally
/// into `store`. `now` is the RFC3339 tombstone timestamp.
pub fn incremental_sync_contacts<T: Transport>(
    transport: &mut T,
    store: &Store,
    account: &str,
    now: &str,
) -> Result<ContactsReport, SyncError> {
    let mut report = ContactsReport::default();

    // Default (folderless) contacts — parent None, scope DEFAULT_SCOPE.
    sync_collection(
        transport,
        store,
        account,
        DEFAULT_DELTA,
        DEFAULT_SCOPE,
        None,
        now,
        &mut report,
    )?;

    // Named folders — parent = folder id, scope = folder id.
    let raw = fetch_pages(transport, FOLDERS_URL)?;
    let folders = parse_folders(&raw);
    report.folders = folders.len();
    for folder in &folders {
        let mut fi = Item::new(account, SERVICE, &folder.id, &folder.name, "folder");
        fi.sync_state = "remote_dirty".into();
        store.upsert_item(&fi)?;

        let base = format!(
            "https://graph.microsoft.com/v1.0/me/contactFolders/{}/contacts/delta",
            folder.id
        );
        sync_collection(
            transport,
            store,
            account,
            &base,
            &folder.id,
            Some(&folder.id),
            now,
            &mut report,
        )?;
    }
    Ok(report)
}

/// Walk one contact collection's delta and ingest it under `parent`/`scope`.
#[allow(clippy::too_many_arguments)]
fn sync_collection<T: Transport>(
    transport: &mut T,
    store: &Store,
    account: &str,
    base_url: &str,
    scope: &str,
    parent: Option<&str>,
    now: &str,
    report: &mut ContactsReport,
) -> Result<(), SyncError> {
    let cursor = store
        .get_delta_cursor(account, SERVICE, scope)?
        .map(DeltaCursor::new);
    let out = run_delta(transport, base_url, cursor.as_ref(), 5)?;
    for c in &out.items {
        match ingest_contact(store, account, parent, c, now)? {
            Ingest::Upserted => report.upserted += 1,
            Ingest::Deleted => report.deleted += 1,
            Ingest::Skipped => report.skipped += 1,
        }
    }
    store.set_delta_cursor(account, SERVICE, scope, out.cursor.as_str())?;
    Ok(())
}

enum Ingest {
    Upserted,
    Deleted,
    Skipped,
}

/// Ingest one contact-delta entry under the given parent folder (`None` = default).
fn ingest_contact(
    store: &Store,
    account: &str,
    parent: Option<&str>,
    c: &Value,
    now: &str,
) -> Result<Ingest, SyncError> {
    let id = c
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| SyncError::Malformed("contact has no id".into()))?;

    if c.get("@removed").is_some() {
        // Tombstone only if it still belongs to this collection (move-safe, like mail).
        let still_here = store
            .get_item(account, SERVICE, id)?
            .map(|it| it.parent_remote_id.as_deref() == parent)
            .unwrap_or(true);
        if still_here {
            store.mark_deleted(account, SERVICE, id, now)?;
            return Ok(Ingest::Deleted);
        }
        return Ok(Ingest::Skipped);
    }

    // displayName is usually present; fall back to assembled name parts.
    let name = c
        .get("displayName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| {
            let given = c.get("givenName").and_then(Value::as_str).unwrap_or("");
            let sur = c.get("surname").and_then(Value::as_str).unwrap_or("");
            let joined = format!("{given} {sur}").trim().to_string();
            (!joined.is_empty()).then_some(joined)
        })
        .unwrap_or_else(|| "(no name)".to_string());

    let mut it = Item::new(account, SERVICE, id, name, "contact");
    it.parent_remote_id = parent.map(String::from);
    it.etag = c
        .get("@odata.etag")
        .and_then(Value::as_str)
        .or_else(|| c.get("changeKey").and_then(Value::as_str))
        .map(String::from);
    it.remote_mtime = c
        .get("lastModifiedDateTime")
        .and_then(Value::as_str)
        .map(String::from);
    it.sync_state = "remote_dirty".into();
    store.upsert_item(&it)?;
    Ok(Ingest::Upserted)
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

    fn folder(id: &str, name: &str) -> Value {
        json!({ "id": id, "displayName": name })
    }
    fn contact(id: &str, name: &str) -> Value {
        json!({
            "id": id,
            "displayName": name,
            "@odata.etag": "W/\"CT\"",
            "lastModifiedDateTime": "2026-03-04T05:06:07Z"
        })
    }
    fn removed(id: &str) -> Value {
        json!({ "id": id, "@removed": { "reason": "deleted" } })
    }

    #[test]
    fn ingests_default_and_folder_contacts_with_scoped_cursors() {
        let store = Store::open_in_memory().unwrap();
        let mut t = MockTransport(
            vec![
                // default contacts delta
                Response::ok(
                    json!({ "value": [contact("d1","Ada Lovelace")], "@odata.deltaLink": "DEF" }),
                ),
                // contact folders list
                Response::ok(json!({ "value": [folder("CF1","Work")] })),
                // CF1 contacts delta
                Response::ok(
                    json!({ "value": [contact("c1","Alan Turing")], "@odata.deltaLink": "CUR1" }),
                ),
            ],
            0,
        );
        let r = incremental_sync_contacts(&mut t, &store, "acc", "2026-06-02T00:00:00Z").unwrap();
        assert_eq!(r.folders, 1);
        assert_eq!(r.upserted, 2);

        let d1 = store.get_item("acc", SERVICE, "d1").unwrap().unwrap();
        assert_eq!(d1.name, "Ada Lovelace");
        assert_eq!(d1.item_type, "contact");
        assert_eq!(d1.parent_remote_id, None); // default collection
        assert_eq!(d1.remote_mtime.as_deref(), Some("2026-03-04T05:06:07Z"));

        let c1 = store.get_item("acc", SERVICE, "c1").unwrap().unwrap();
        assert_eq!(c1.parent_remote_id.as_deref(), Some("CF1"));

        assert_eq!(
            store
                .get_delta_cursor("acc", SERVICE, DEFAULT_SCOPE)
                .unwrap()
                .as_deref(),
            Some("DEF")
        );
        assert_eq!(
            store
                .get_delta_cursor("acc", SERVICE, "CF1")
                .unwrap()
                .as_deref(),
            Some("CUR1")
        );
    }

    #[test]
    fn assembles_name_from_parts_when_displayname_missing() {
        let store = Store::open_in_memory().unwrap();
        let mut t = MockTransport(
            vec![
                Response::ok(
                    json!({ "value": [json!({"id":"x","givenName":"Grace","surname":"Hopper"})], "@odata.deltaLink": "D" }),
                ),
                Response::ok(json!({ "value": [] })), // no folders
            ],
            0,
        );
        incremental_sync_contacts(&mut t, &store, "acc", "t").unwrap();
        assert_eq!(
            store.get_item("acc", SERVICE, "x").unwrap().unwrap().name,
            "Grace Hopper"
        );
    }

    #[test]
    fn deleted_default_contact_is_tombstoned() {
        let store = Store::open_in_memory().unwrap();
        let mut t1 = MockTransport(
            vec![
                Response::ok(
                    json!({ "value": [contact("d9","Bye Person")], "@odata.deltaLink": "D1" }),
                ),
                Response::ok(json!({ "value": [] })),
            ],
            0,
        );
        incremental_sync_contacts(&mut t1, &store, "acc", "t").unwrap();
        let mut t2 = MockTransport(
            vec![
                Response::ok(json!({ "value": [removed("d9")], "@odata.deltaLink": "D2" })),
                Response::ok(json!({ "value": [] })),
            ],
            0,
        );
        let r = incremental_sync_contacts(&mut t2, &store, "acc", "2026-06-02T00:00:00Z").unwrap();
        assert_eq!(r.deleted, 1);
        assert!(store
            .get_item("acc", SERVICE, "d9")
            .unwrap()
            .unwrap()
            .deleted_at
            .is_some());
    }

    /// Live: real default + per-folder contact delta -> store, against the
    /// throwaway account. Needs feature `http` + `ISYNCYOU_TEST_TOKEN` carrying
    /// `Contacts.Read`.
    #[cfg(feature = "http")]
    #[test]
    fn live_incremental_sync_contacts() {
        let token = match std::env::var("ISYNCYOU_TEST_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!("skipping live_incremental_sync_contacts: ISYNCYOU_TEST_TOKEN not set");
                return;
            }
        };
        let store = Store::open_in_memory().unwrap();
        let mut client = isyncyou_graph::GraphClient::new(token);
        let report =
            incremental_sync_contacts(&mut client, &store, "backupslave", "2026-06-02T00:00:00Z")
                .expect("live contacts sync should succeed");
        // The default collection's cursor must always be persisted, even with
        // zero contacts (proves the delta walk completed).
        assert!(store
            .get_delta_cursor("backupslave", SERVICE, DEFAULT_SCOPE)
            .unwrap()
            .is_some());
        eprintln!(
            "live contacts sync: folders={} upserted={} deleted={} skipped={}",
            report.folders, report.upserted, report.deleted, report.skipped
        );
    }
}
