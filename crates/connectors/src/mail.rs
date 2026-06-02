//! Mail connector — per-folder message delta into the store (plan §6).
//!
//! Mail has no single account-wide delta: you sync the **folder tree** first,
//! then walk `/me/mailFolders/{id}/messages/delta` per folder, each with its own
//! persisted cursor (`scope = folder id`). Messages are stored id-based (service
//! `"mail"`) so a **move** — which Graph reports as `@removed reason:"deleted"`
//! in the source folder *and* an add in the destination folder — keeps its
//! identity instead of being lost: we only tombstone a removal if the message
//! still belongs to the folder reporting it.

use crate::onedrive::SyncError;
use isyncyou_graph::{run_delta, DeltaCursor, Transport};
use isyncyou_store::{Item, Store};
use serde_json::Value;

const SERVICE: &str = "mail";
const FOLDERS_URL: &str = "https://graph.microsoft.com/v1.0/me/mailFolders?$top=100";

/// What one mail sync changed (folder count + message deltas).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailReport {
    pub folders: usize,
    pub upserted: usize,
    pub deleted: usize,
    pub skipped: usize,
}

/// A mail folder as we track it.
struct Folder {
    id: String,
    name: String,
    parent: Option<String>,
}

/// Page through a non-delta Graph collection (`value[]` + `@odata.nextLink`).
/// Mail folders are a plain paged list, not a delta query, so this can't reuse
/// [`run_delta`] (which requires a terminating `@odata.deltaLink`).
fn fetch_pages<T: Transport>(transport: &mut T, start_url: &str) -> Result<Vec<Value>, SyncError> {
    let mut url = start_url.to_string();
    let mut out = Vec::new();
    loop {
        let resp = transport.get(&url);
        if !(200..300).contains(&resp.status) {
            return Err(SyncError::Remote(format!(
                "HTTP {} listing {url}",
                resp.status
            )));
        }
        let body = resp
            .body
            .ok_or_else(|| SyncError::Malformed("empty list page".into()))?;
        if let Some(arr) = body.get("value").and_then(Value::as_array) {
            out.extend(arr.iter().cloned());
        }
        match body.get("@odata.nextLink").and_then(Value::as_str) {
            Some(next) => url = next.to_string(),
            None => break,
        }
    }
    Ok(out)
}

fn parse_folders(raw: &[Value]) -> Vec<Folder> {
    raw.iter()
        .filter_map(|f| {
            let id = f.get("id").and_then(Value::as_str)?.to_string();
            let name = f
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let parent = f
                .get("parentFolderId")
                .and_then(Value::as_str)
                .map(String::from);
            Some(Folder { id, name, parent })
        })
        .collect()
}

/// Sync every mail folder's messages incrementally into `store`. `now` is the
/// RFC3339 tombstone timestamp (caller-supplied for deterministic tests).
pub fn incremental_sync_mail<T: Transport>(
    transport: &mut T,
    store: &Store,
    account: &str,
    now: &str,
) -> Result<MailReport, SyncError> {
    let raw = fetch_pages(transport, FOLDERS_URL)?;
    let folders = parse_folders(&raw);
    let mut report = MailReport {
        folders: folders.len(),
        ..Default::default()
    };

    for folder in &folders {
        // Record the folder itself so the tree is queryable / restorable.
        let mut fi = Item::new(account, SERVICE, &folder.id, &folder.name, "folder");
        fi.parent_remote_id = folder.parent.clone();
        fi.sync_state = "remote_dirty".into();
        store.upsert_item(&fi)?;

        let base = format!(
            "https://graph.microsoft.com/v1.0/me/mailFolders/{}/messages/delta",
            folder.id
        );
        let cursor = store
            .get_delta_cursor(account, SERVICE, &folder.id)?
            .map(DeltaCursor::new);
        let out = run_delta(transport, &base, cursor.as_ref(), 5)?;
        for msg in &out.items {
            match ingest_message(store, account, &folder.id, msg, now)? {
                Ingest::Upserted => report.upserted += 1,
                Ingest::Deleted => report.deleted += 1,
                Ingest::Skipped => report.skipped += 1,
            }
        }
        store.set_delta_cursor(account, SERVICE, &folder.id, out.cursor.as_str())?;
    }
    Ok(report)
}

enum Ingest {
    Upserted,
    Deleted,
    Skipped,
}

/// Ingest one message-delta entry for a given folder.
fn ingest_message(
    store: &Store,
    account: &str,
    folder_id: &str,
    msg: &Value,
    now: &str,
) -> Result<Ingest, SyncError> {
    let id = msg
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| SyncError::Malformed("message has no id".into()))?;

    if msg.get("@removed").is_some() {
        // Removed from *this* folder. Graph reports a move identically
        // (reason "deleted"), so only tombstone if the message still belongs
        // here — a move-in already applied by another folder's delta must not
        // be clobbered. (A brand-new id we've never seen defaults to a real
        // deletion.)
        let still_here = store
            .get_item(account, SERVICE, id)?
            .map(|it| it.parent_remote_id.as_deref() == Some(folder_id))
            .unwrap_or(true);
        if still_here {
            store.mark_deleted(account, SERVICE, id, now)?;
            return Ok(Ingest::Deleted);
        }
        return Ok(Ingest::Skipped);
    }

    let subject = msg
        .get("subject")
        .and_then(Value::as_str)
        .unwrap_or("(no subject)");
    let mut it = Item::new(account, SERVICE, id, subject, "message");
    it.parent_remote_id = Some(folder_id.to_string());
    it.etag = msg
        .get("@odata.etag")
        .and_then(Value::as_str)
        .or_else(|| msg.get("changeKey").and_then(Value::as_str))
        .map(String::from);
    it.remote_mtime = msg
        .get("lastModifiedDateTime")
        .and_then(Value::as_str)
        .or_else(|| msg.get("receivedDateTime").and_then(Value::as_str))
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

    /// Returns queued responses in strict call order (ignores the url).
    struct MockTransport(Vec<Response>, usize);
    impl Transport for MockTransport {
        fn get(&mut self, _url: &str) -> Response {
            let r = self.0[self.1].clone();
            self.1 += 1;
            r
        }
    }

    fn folder(id: &str, name: &str) -> Value {
        json!({ "id": id, "displayName": name, "parentFolderId": "ROOT" })
    }
    fn msg(id: &str, subject: &str) -> Value {
        json!({
            "id": id,
            "subject": subject,
            "@odata.etag": "W/\"CQAAAB\"",
            "receivedDateTime": "2026-01-01T00:00:00Z"
        })
    }
    fn removed(id: &str) -> Value {
        json!({ "id": id, "@removed": { "reason": "deleted" } })
    }

    #[test]
    fn ingests_folders_messages_and_per_folder_cursors() {
        let store = Store::open_in_memory().unwrap();
        let mut t = MockTransport(
            vec![
                Response::ok(json!({ "value": [folder("FA", "Inbox"), folder("FB", "Archive")] })),
                Response::ok(
                    json!({ "value": [msg("m1","Hello"), msg("m2","Hi")], "@odata.deltaLink": "CA" }),
                ),
                Response::ok(json!({ "value": [msg("m3","Yo")], "@odata.deltaLink": "CB" })),
            ],
            0,
        );
        let r = incremental_sync_mail(&mut t, &store, "acc", "2026-06-02T00:00:00Z").unwrap();
        assert_eq!(r.folders, 2);
        assert_eq!(r.upserted, 3);
        assert_eq!(r.deleted, 0);

        // folder tree recorded
        let fa = store.get_item("acc", SERVICE, "FA").unwrap().unwrap();
        assert_eq!(fa.name, "Inbox");
        assert_eq!(fa.item_type, "folder");
        assert_eq!(fa.parent_remote_id.as_deref(), Some("ROOT"));
        // message recorded with subject + parent + etag + mtime
        let m1 = store.get_item("acc", SERVICE, "m1").unwrap().unwrap();
        assert_eq!(m1.name, "Hello");
        assert_eq!(m1.item_type, "message");
        assert_eq!(m1.parent_remote_id.as_deref(), Some("FA"));
        assert_eq!(m1.remote_mtime.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert!(m1.etag.is_some());
        // per-folder cursors persisted
        assert_eq!(
            store
                .get_delta_cursor("acc", SERVICE, "FA")
                .unwrap()
                .as_deref(),
            Some("CA")
        );
        assert_eq!(
            store
                .get_delta_cursor("acc", SERVICE, "FB")
                .unwrap()
                .as_deref(),
            Some("CB")
        );
    }

    #[test]
    fn move_in_is_not_clobbered_by_removal_in_old_folder() {
        // Folder list order [FB, FA]: the message is added to FB first, then FA's
        // delta reports it @removed. Since it now lives in FB, FA's removal is a
        // move-out and must be skipped (not tombstoned).
        let store = Store::open_in_memory().unwrap();
        let mut t = MockTransport(
            vec![
                Response::ok(json!({ "value": [folder("FB","Archive"), folder("FA","Inbox")] })),
                Response::ok(json!({ "value": [msg("m1","Moved")], "@odata.deltaLink": "CB" })),
                Response::ok(json!({ "value": [removed("m1")], "@odata.deltaLink": "CA" })),
            ],
            0,
        );
        let r = incremental_sync_mail(&mut t, &store, "acc", "2026-06-02T00:00:00Z").unwrap();
        assert_eq!(r.upserted, 1);
        assert_eq!(r.deleted, 0);
        assert_eq!(r.skipped, 1);
        let m1 = store.get_item("acc", SERVICE, "m1").unwrap().unwrap();
        assert!(m1.deleted_at.is_none(), "moved message must stay alive");
        assert_eq!(m1.parent_remote_id.as_deref(), Some("FB"));
    }

    #[test]
    fn real_deletion_in_owning_folder_tombstones() {
        let store = Store::open_in_memory().unwrap();
        // first sync: m9 arrives in FA
        let mut t1 = MockTransport(
            vec![
                Response::ok(json!({ "value": [folder("FA","Inbox")] })),
                Response::ok(json!({ "value": [msg("m9","Bye")], "@odata.deltaLink": "C1" })),
            ],
            0,
        );
        incremental_sync_mail(&mut t1, &store, "acc", "t").unwrap();
        // second sync: FA reports m9 removed -> it still belongs to FA -> tombstone
        let mut t2 = MockTransport(
            vec![
                Response::ok(json!({ "value": [folder("FA","Inbox")] })),
                Response::ok(json!({ "value": [removed("m9")], "@odata.deltaLink": "C2" })),
            ],
            0,
        );
        let r = incremental_sync_mail(&mut t2, &store, "acc", "2026-06-02T00:00:00Z").unwrap();
        assert_eq!(r.deleted, 1);
        assert!(store
            .get_item("acc", SERVICE, "m9")
            .unwrap()
            .unwrap()
            .deleted_at
            .is_some());
        // FA's cursor advanced to the second token
        assert_eq!(
            store
                .get_delta_cursor("acc", SERVICE, "FA")
                .unwrap()
                .as_deref(),
            Some("C2")
        );
    }

    #[test]
    fn message_without_subject_gets_placeholder() {
        let store = Store::open_in_memory().unwrap();
        let mut t = MockTransport(
            vec![
                Response::ok(json!({ "value": [folder("FA","Inbox")] })),
                Response::ok(json!({ "value": [json!({"id":"mx"})], "@odata.deltaLink": "C" })),
            ],
            0,
        );
        incremental_sync_mail(&mut t, &store, "acc", "t").unwrap();
        assert_eq!(
            store.get_item("acc", SERVICE, "mx").unwrap().unwrap().name,
            "(no subject)"
        );
    }

    /// Live: real per-folder mail delta -> store, against the throwaway account.
    /// Needs feature `http` + `ISYNCYOU_TEST_TOKEN` carrying `Mail.Read`.
    #[cfg(feature = "http")]
    #[test]
    fn live_incremental_sync_mail() {
        let token = match std::env::var("ISYNCYOU_TEST_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!("skipping live_incremental_sync_mail: ISYNCYOU_TEST_TOKEN not set");
                return;
            }
        };
        let store = Store::open_in_memory().unwrap();
        let mut client = isyncyou_graph::GraphClient::new(token);
        let report =
            incremental_sync_mail(&mut client, &store, "backupslave", "2026-06-02T00:00:00Z")
                .expect("live mail sync should succeed");
        assert!(report.folders > 0, "expected at least one mail folder");
        // every well-known folder must have a persisted cursor after the walk
        assert!(store
            .get_delta_cursor("backupslave", SERVICE, "")
            .unwrap()
            .is_none()); // mail uses per-folder scopes, never the "" whole-service scope
        eprintln!(
            "live mail sync: folders={} upserted={} deleted={} skipped={}",
            report.folders, report.upserted, report.deleted, report.skipped
        );
    }
}
