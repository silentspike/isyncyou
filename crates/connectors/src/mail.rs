//! Mail connector — per-folder message delta into the store (plan §6).
//!
//! Mail has no single account-wide delta: you sync the **folder tree** first,
//! then walk `/me/mailFolders/{id}/messages/delta` per folder, each with its own
//! persisted cursor (`scope = folder id`). Messages are stored id-based (service
//! `"mail"`) so a **move** — which Graph reports as `@removed reason:"deleted"`
//! in the source folder *and* an add in the destination folder — keeps its
//! identity instead of being lost: we only tombstone a removal if the message
//! still belongs to the folder reporting it.

use crate::archive::{ArchiveReport, JsonFetcher};
use crate::common::{fetch_pages, shard_path};
use crate::mime::extract_text;
use crate::onedrive::SyncError;
use isyncyou_graph::{run_delta, DeltaCursor, Transport};
use isyncyou_store::{Item, Store};
use serde_json::Value;
use std::path::Path;

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
    archive_root: &Path,
) -> Result<MailReport, SyncError> {
    // Outlook immutable-ID policy (plan §6): make ids stable across folder moves.
    transport.set_prefer_immutable_id(true);
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
            match ingest_message(store, account, &folder.id, msg, now, archive_root)? {
                Ingest::Upserted => report.upserted += 1,
                Ingest::Deleted => report.deleted += 1,
                Ingest::Skipped => report.skipped += 1,
            }
        }
        store.set_delta_cursor(account, SERVICE, &folder.id, out.cursor.as_str())?;
    }
    // Backfill senders for any pre-#89 messages still carrying NULL (cheap no-op
    // once done) so the list never shows "(unknown sender)" on the backlog (CC-1).
    let _ = backfill_mail_senders(store, account, archive_root);
    Ok(report)
}

enum Ingest {
    Upserted,
    Deleted,
    Skipped,
}

/// Fetches a message's full MIME (`.eml`) by id. Abstracted so the body-download
/// driver is unit-testable with a mock and live-tested with the real client.
pub trait MimeFetcher {
    fn fetch_mime(&self, message_id: &str) -> Result<Vec<u8>, String>;

    fn fetch_mime_for_sync(&self, message_id: &str) -> Result<Vec<u8>, SyncError> {
        self.fetch_mime(message_id).map_err(SyncError::Remote)
    }
}

#[cfg(feature = "http")]
impl MimeFetcher for isyncyou_graph::GraphClient {
    fn fetch_mime(&self, message_id: &str) -> Result<Vec<u8>, String> {
        self.download_message_mime(message_id)
            .map_err(|e| e.to_string())
    }

    fn fetch_mime_for_sync(&self, message_id: &str) -> Result<Vec<u8>, SyncError> {
        self.download_message_mime(message_id)
            .map_err(SyncError::Graph)
    }
}

/// What one body-download pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BodyReport {
    /// Messages whose MIME was fetched and written this pass.
    pub downloaded: usize,
    /// Messages skipped because their body was already on disk.
    pub skipped: usize,
    /// Total bytes written this pass.
    pub bytes: u64,
}

/// Download `.eml` MIME for stored mail messages that don't yet have a local
/// body, writing each (atomically) under `archive_root` in a sharded layout and
/// recording the relative path in the store. `limit` caps how many are fetched
/// in one pass (`0` = no limit). Already-downloaded messages are skipped, so
/// this is safe to call repeatedly and resumes where it left off.
pub fn backup_message_bodies<F: MimeFetcher>(
    fetcher: &F,
    store: &Store,
    account: &str,
    archive_root: &Path,
    limit: usize,
) -> Result<BodyReport, SyncError> {
    let mut report = BodyReport::default();
    for msg in store.items_by_type(account, SERVICE, "message")? {
        if msg.local_path.is_some() {
            report.skipped += 1;
            continue;
        }
        if limit != 0 && report.downloaded >= limit {
            break;
        }
        let mime = fetcher.fetch_mime_for_sync(&msg.remote_id)?;
        let abs = shard_path(archive_root, SERVICE, &msg.remote_id, "eml");
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // tmp + rename: a crash never leaves a half-written .eml in place.
        let tmp = abs.with_extension("eml.part");
        std::fs::write(&tmp, isyncyou_core::envelope::seal_for_disk(&mime))?;
        std::fs::rename(&tmp, &abs)?;

        let rel = abs.strip_prefix(archive_root).unwrap_or(&abs);
        store.set_local_path(
            account,
            SERVICE,
            &msg.remote_id,
            Some(&rel.to_string_lossy()),
        )?;
        report.downloaded += 1;
        report.bytes += mime.len() as u64;
    }
    Ok(report)
}

/// Extract and FTS-index the body text of every downloaded mail message (those
/// with a local `.eml`), feeding [`Store::index_body`] (plan §9). Idempotent;
/// `limit` caps one pass (`0` = all). Run after [`backup_message_bodies`].
pub fn index_mail_bodies(
    store: &Store,
    account: &str,
    archive_root: &Path,
    limit: usize,
) -> Result<usize, SyncError> {
    let mut indexed = 0;
    for msg in store.items_by_type(account, SERVICE, "message")? {
        let rel = match msg.local_path.as_deref() {
            Some(p) if p.ends_with(".eml") => p,
            _ => continue,
        };
        if limit != 0 && indexed >= limit {
            break;
        }
        let bytes = isyncyou_core::envelope::read_body(&archive_root.join(rel))?;
        let text = extract_text(&bytes);
        store.index_body(account, SERVICE, &msg.remote_id, &text)?;
        indexed += 1;
    }
    Ok(indexed)
}

/// Upsert a JSON-snapshot store item under `service="mail"` and archive its
/// canonical JSON to `mail/<shard>/<id>.json` (atomic tmp+rename), recording the
/// relative path as `local_path`. Shared by the mailbox-flank snapshots. Returns
/// the byte count written.
fn archive_json_item(
    store: &Store,
    account: &str,
    archive_root: &Path,
    id: &str,
    name: &str,
    item_type: &str,
    value: &Value,
) -> Result<u64, SyncError> {
    let mut it = Item::new(account, SERVICE, id, name, item_type);
    it.sync_state = "remote_dirty".into();
    store.upsert_item(&it)?;

    let abs = shard_path(archive_root, SERVICE, id, "json");
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(value).map_err(|e| SyncError::Malformed(e.to_string()))?;
    let tmp = abs.with_extension("json.part");
    std::fs::write(&tmp, isyncyou_core::envelope::seal_for_disk(&bytes))?;
    std::fs::rename(&tmp, &abs)?;
    let rel = abs.strip_prefix(archive_root).unwrap_or(&abs);
    store.set_local_path(account, SERVICE, id, Some(&rel.to_string_lossy()))?;
    Ok(bytes.len() as u64)
}

/// Back up the mailbox configuration flanks (#562) as JSON snapshots under
/// `service="mail"`: mailbox settings (`/me/mailboxSettings`, one
/// `mailbox-setting` item), inbox message rules
/// (`/me/mailFolders/inbox/messageRules`, one `rule-set` item), and master
/// categories (`/me/outlook/masterCategories`, one `category` item **per
/// category** so the UI can read each colour). Re-fetched each pass (the data is
/// small) so it stays current. Needs `MailboxSettings.Read` (S-P4.1, #558).
pub fn backup_mailbox_flanks<F: JsonFetcher>(
    fetcher: &F,
    store: &Store,
    account: &str,
    archive_root: &Path,
) -> Result<ArchiveReport, SyncError> {
    let mut report = ArchiveReport::default();

    let settings = fetcher.fetch_json_for_sync("/me/mailboxSettings")?;
    report.bytes += archive_json_item(
        store,
        account,
        archive_root,
        "_mailbox_settings",
        "Mailbox settings",
        "mailbox-setting",
        &settings,
    )?;
    report.archived += 1;

    let rules = fetcher.fetch_json_for_sync("/me/mailFolders/inbox/messageRules")?;
    report.bytes += archive_json_item(
        store,
        account,
        archive_root,
        "_inbox_rules",
        "Inbox rules",
        "rule-set",
        &rules,
    )?;
    report.archived += 1;

    let cats = fetcher.fetch_json_for_sync("/me/outlook/masterCategories")?;
    for cat in cats
        .get("value")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = cat.get("id").and_then(Value::as_str) else {
            continue;
        };
        let name = cat.get("displayName").and_then(Value::as_str).unwrap_or(id);
        report.bytes += archive_json_item(store, account, archive_root, id, name, "category", cat)?;
        report.archived += 1;
    }

    Ok(report)
}

/// Archive the full Graph message JSON beside the `.eml` (`mail/<shard>/<id>.json`)
/// so the backup captures every Outlook property the raw MIME can't carry cleanly
/// — categories, isRead, flag, importance, cc/bcc, conversationId, webLink,
/// isDraft, receipt flags (#562). Written at ingest straight from the delta
/// payload (no extra fetch, no new scope) and rewritten on every change so the
/// structured fields stay current. Atomic tmp+rename, like the `.eml` path.
fn write_message_json(archive_root: &Path, id: &str, msg: &Value) -> Result<(), SyncError> {
    let abs = shard_path(archive_root, SERVICE, id, "json");
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(msg).map_err(|e| SyncError::Malformed(e.to_string()))?;
    let tmp = abs.with_extension("json.part");
    std::fs::write(&tmp, isyncyou_core::envelope::seal_for_disk(&bytes))?;
    std::fs::rename(&tmp, &abs)?;
    Ok(())
}

/// Drop the archived JSON sidecar when its message is tombstoned.
fn remove_message_json(archive_root: &Path, id: &str) {
    let _ = std::fs::remove_file(shard_path(archive_root, SERVICE, id, "json"));
}

/// Build a mail item's display sender ("Name <addr>" / address / name) from a Graph
/// message's `from.emailAddress`. Shared by ingest and the backfill so both yield
/// the same value. `None` only when neither a name nor an address is present.
pub(crate) fn sender_display(msg: &Value) -> Option<String> {
    let ea = msg.get("from").and_then(|f| f.get("emailAddress"))?;
    let name = ea.get("name").and_then(Value::as_str).unwrap_or("").trim();
    let addr = ea
        .get("address")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    match (name.is_empty(), addr.is_empty()) {
        (false, false) => Some(format!("{name} <{addr}>")),
        (true, false) => Some(addr.to_string()),
        (false, true) => Some(name.to_string()),
        (true, true) => None,
    }
}

/// Backfill `sender` for mail items that predate the `sender` column (NULL) by
/// re-reading the archived Graph JSON sidecar (`<id>.json`, written for every
/// ingested message). Idempotent and self-limiting: only rows with `sender IS NULL`
/// are touched, so after the first pass there is nothing to do. Closes the
/// pre-#89 "(unknown sender)" backlog (the delta never re-delivers unchanged mail,
/// so without this those rows would stay NULL forever). Returns the count filled.
pub fn backfill_mail_senders(
    store: &Store,
    account: &str,
    archive_root: &Path,
) -> Result<usize, SyncError> {
    let mut filled = 0;
    for it in store.items_by_type(account, SERVICE, "message")? {
        if it.sender.is_some() {
            continue;
        }
        let p = shard_path(archive_root, SERVICE, &it.remote_id, "json");
        let Ok(bytes) = isyncyou_core::envelope::read_body(&p) else {
            continue; // no sidecar (e.g. never body-archived) — leave for next sync
        };
        let Ok(o) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if let Some(sender) = sender_display(&o) {
            store.set_sender(account, SERVICE, &it.remote_id, &sender)?;
            filled += 1;
        }
    }
    Ok(filled)
}

/// Ingest one message-delta entry for a given folder.
fn ingest_message(
    store: &Store,
    account: &str,
    folder_id: &str,
    msg: &Value,
    now: &str,
    archive_root: &Path,
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
            remove_message_json(archive_root, id);
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
    // Immutable-ID companions (plan §6): changeKey for optimistic concurrency,
    // internetMessageId as a move/dedup-stable identifier.
    it.change_key = msg
        .get("changeKey")
        .and_then(Value::as_str)
        .map(String::from);
    it.internet_message_id = msg
        .get("internetMessageId")
        .and_then(Value::as_str)
        .map(String::from);
    // Display sender ("Name <addr>"), captured straight from the delta so the list
    // shows who a message is from even before its .eml body is cached (#89).
    it.sender = sender_display(msg);
    it.sync_state = "remote_dirty".into();
    store.upsert_item(&it)?;
    // Capture the full Graph message JSON beside the .eml (completeness, #562).
    write_message_json(archive_root, id, msg)?;
    Ok(Ingest::Upserted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use isyncyou_graph::client::Response;
    use serde_json::json;

    /// Returns queued responses in strict call order (ignores the url); records
    /// whether the immutable-ID policy was enabled by the connector.
    struct MockTransport {
        queue: Vec<Response>,
        calls: usize,
        prefer_immutable_id: bool,
    }
    impl MockTransport {
        fn new(queue: Vec<Response>) -> Self {
            MockTransport {
                queue,
                calls: 0,
                prefer_immutable_id: false,
            }
        }
    }
    impl Transport for MockTransport {
        fn get(&mut self, _url: &str) -> Response {
            let r = self.queue[self.calls].clone();
            self.calls += 1;
            r
        }
        fn set_prefer_immutable_id(&mut self, on: bool) {
            self.prefer_immutable_id = on;
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
            "changeKey": "CQAAAB",
            "internetMessageId": format!("<{id}@mail.example.com>"),
            "receivedDateTime": "2026-01-01T00:00:00Z"
        })
    }
    fn removed(id: &str) -> Value {
        json!({ "id": id, "@removed": { "reason": "deleted" } })
    }

    /// Run a mail sync against a throwaway archive dir. The JSON sidecar (#562)
    /// needs a writable root; tests that don't inspect it just discard the dir.
    fn sync_mail(
        t: &mut MockTransport,
        store: &Store,
        account: &str,
        now: &str,
    ) -> Result<MailReport, SyncError> {
        let arch = tempfile::tempdir().unwrap();
        incremental_sync_mail(t, store, account, now, arch.path())
    }

    #[test]
    fn ingest_writes_full_message_json_sidecar() {
        let store = Store::open_in_memory().unwrap();
        let arch = tempfile::tempdir().unwrap();
        let rich = json!({
            "id": "mr1", "subject": "Rich",
            "@odata.etag": "W/\"x\"", "changeKey": "x",
            "receivedDateTime": "2026-01-01T00:00:00Z",
            "categories": ["Red category", "Work"],
            "isRead": false,
            "flag": { "flagStatus": "flagged" },
            "importance": "high",
            "ccRecipients": [{ "emailAddress": { "address": "c@x.com" } }],
            "isDraft": false,
            "webLink": "https://outlook.live.com/mail/0/x"
        });
        let mut t = MockTransport::new(vec![
            Response::ok(json!({ "value": [folder("FA", "Inbox")] })),
            Response::ok(json!({ "value": [rich], "@odata.deltaLink": "C" })),
        ]);
        incremental_sync_mail(&mut t, &store, "acc", "t", arch.path()).unwrap();

        let p = shard_path(arch.path(), SERVICE, "mr1", "json");
        let v: Value =
            serde_json::from_slice(&std::fs::read(&p).expect("sidecar json written")).unwrap();
        assert_eq!(v["categories"][0], "Red category");
        assert_eq!(v["isRead"], false);
        assert_eq!(v["flag"]["flagStatus"], "flagged");
        assert_eq!(v["importance"], "high");
        assert_eq!(v["ccRecipients"][0]["emailAddress"]["address"], "c@x.com");
        assert_eq!(v["webLink"], "https://outlook.live.com/mail/0/x");
    }

    #[test]
    fn ingest_captures_display_sender_into_the_item() {
        // #89: the sender is captured at ingest into the item's `sender` column so
        // the list shows who a mail is from without its .eml body being cached.
        let store = Store::open_in_memory().unwrap();
        let arch = tempfile::tempdir().unwrap();
        let with_name = json!({
            "id": "ms1", "subject": "Hi",
            "from": { "emailAddress": { "name": "Grace Hopper", "address": "grace@example.com" } },
        });
        let addr_only = json!({
            "id": "ms2", "subject": "No name",
            "from": { "emailAddress": { "address": "noname@example.com" } },
        });
        let mut t = MockTransport::new(vec![
            Response::ok(json!({ "value": [folder("FA", "Inbox")] })),
            Response::ok(json!({ "value": [with_name, addr_only], "@odata.deltaLink": "C" })),
        ]);
        incremental_sync_mail(&mut t, &store, "acc", "t", arch.path()).unwrap();
        assert_eq!(
            store
                .get_item("acc", SERVICE, "ms1")
                .unwrap()
                .unwrap()
                .sender
                .as_deref(),
            Some("Grace Hopper <grace@example.com>")
        );
        assert_eq!(
            store
                .get_item("acc", SERVICE, "ms2")
                .unwrap()
                .unwrap()
                .sender
                .as_deref(),
            Some("noname@example.com")
        );
    }

    #[test]
    fn backfill_fills_null_senders_from_json_sidecar() {
        // CC-1: a pre-#89 message (sender NULL) gets its sender from the archived
        // Graph JSON sidecar; idempotent; rows without a sidecar are left untouched.
        let store = Store::open_in_memory().unwrap();
        let arch = tempfile::tempdir().unwrap();
        let msg = json!({
            "id": "mb1", "subject": "Old mail",
            "from": { "emailAddress": { "name": "Ada Lovelace", "address": "ada@example.com" } },
        });
        write_message_json(arch.path(), "mb1", &msg).unwrap();
        let it = Item::new("acc", SERVICE, "mb1", "Old mail", "message");
        assert!(it.sender.is_none());
        store.upsert_item(&it).unwrap();
        // a second message with no sidecar on disk — must be left as-is, no crash
        store
            .upsert_item(&Item::new("acc", SERVICE, "mb2", "No sidecar", "message"))
            .unwrap();

        let filled = backfill_mail_senders(&store, "acc", arch.path()).unwrap();
        assert_eq!(filled, 1);
        assert_eq!(
            store
                .get_item("acc", SERVICE, "mb1")
                .unwrap()
                .unwrap()
                .sender
                .as_deref(),
            Some("Ada Lovelace <ada@example.com>")
        );
        assert!(store
            .get_item("acc", SERVICE, "mb2")
            .unwrap()
            .unwrap()
            .sender
            .is_none());
        // idempotent: second pass touches nothing
        assert_eq!(
            backfill_mail_senders(&store, "acc", arch.path()).unwrap(),
            0
        );
    }

    #[test]
    fn tombstone_removes_the_json_sidecar() {
        let store = Store::open_in_memory().unwrap();
        let arch = tempfile::tempdir().unwrap();
        let mut t1 = MockTransport::new(vec![
            Response::ok(json!({ "value": [folder("FA", "Inbox")] })),
            Response::ok(json!({ "value": [msg("m9", "Bye")], "@odata.deltaLink": "C1" })),
        ]);
        incremental_sync_mail(&mut t1, &store, "acc", "t", arch.path()).unwrap();
        let p = shard_path(arch.path(), SERVICE, "m9", "json");
        assert!(p.exists(), "sidecar present after ingest");

        let mut t2 = MockTransport::new(vec![
            Response::ok(json!({ "value": [folder("FA", "Inbox")] })),
            Response::ok(json!({ "value": [removed("m9")], "@odata.deltaLink": "C2" })),
        ]);
        incremental_sync_mail(&mut t2, &store, "acc", "2026-06-02T00:00:00Z", arch.path()).unwrap();
        assert!(!p.exists(), "sidecar removed after tombstone");
    }

    /// Canned JsonFetcher keyed by url, for the flank-connector test.
    struct MockJson(std::collections::HashMap<String, Value>);
    impl JsonFetcher for MockJson {
        fn fetch_json(&self, url: &str) -> Result<Value, String> {
            self.0
                .get(url)
                .cloned()
                .ok_or_else(|| format!("no mock for {url}"))
        }
    }

    #[test]
    fn backup_mailbox_flanks_snapshots_settings_rules_and_categories() {
        let store = Store::open_in_memory().unwrap();
        let arch = tempfile::tempdir().unwrap();
        let mut m = std::collections::HashMap::new();
        m.insert(
            "/me/mailboxSettings".to_string(),
            json!({ "timeZone": "UTC", "language": { "locale": "en-US" } }),
        );
        m.insert(
            "/me/mailFolders/inbox/messageRules".to_string(),
            json!({ "value": [{ "id": "r1", "displayName": "To Archive" }] }),
        );
        m.insert(
            "/me/outlook/masterCategories".to_string(),
            json!({ "value": [
                { "id": "c1", "displayName": "Red category", "color": "preset0" },
                { "id": "c2", "displayName": "Work", "color": "preset5" }
            ] }),
        );
        let report = backup_mailbox_flanks(&MockJson(m), &store, "acc", arch.path()).unwrap();
        assert_eq!(report.archived, 4); // settings + rules + 2 categories

        // store items with the right types
        assert_eq!(
            store
                .get_item("acc", SERVICE, "_mailbox_settings")
                .unwrap()
                .unwrap()
                .item_type,
            "mailbox-setting"
        );
        assert_eq!(
            store
                .get_item("acc", SERVICE, "_inbox_rules")
                .unwrap()
                .unwrap()
                .item_type,
            "rule-set"
        );
        let c1 = store.get_item("acc", SERVICE, "c1").unwrap().unwrap();
        assert_eq!(c1.item_type, "category");
        assert_eq!(c1.name, "Red category");

        // each category archived with displayName + color
        let pc = shard_path(arch.path(), SERVICE, "c1", "json");
        let vc: Value = serde_json::from_slice(&std::fs::read(&pc).unwrap()).unwrap();
        assert_eq!(vc["displayName"], "Red category");
        assert_eq!(vc["color"], "preset0");

        // settings snapshot archived
        let ps = shard_path(arch.path(), SERVICE, "_mailbox_settings", "json");
        let vs: Value = serde_json::from_slice(&std::fs::read(&ps).unwrap()).unwrap();
        assert_eq!(vs["timeZone"], "UTC");
    }

    #[test]
    fn ingests_folders_messages_and_per_folder_cursors() {
        let store = Store::open_in_memory().unwrap();
        let mut t = MockTransport::new(vec![
            Response::ok(json!({ "value": [folder("FA", "Inbox"), folder("FB", "Archive")] })),
            Response::ok(
                json!({ "value": [msg("m1","Hello"), msg("m2","Hi")], "@odata.deltaLink": "CA" }),
            ),
            Response::ok(json!({ "value": [msg("m3","Yo")], "@odata.deltaLink": "CB" })),
        ]);
        let r = sync_mail(&mut t, &store, "acc", "2026-06-02T00:00:00Z").unwrap();
        assert_eq!(r.folders, 2);
        assert_eq!(r.upserted, 3);
        assert_eq!(r.deleted, 0);
        // the connector enabled the Outlook immutable-ID policy (plan §6)
        assert!(
            t.prefer_immutable_id,
            "mail sync must request immutable ids"
        );

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
        // immutable-ID companions stored (plan §6)
        assert_eq!(m1.change_key.as_deref(), Some("CQAAAB"));
        assert_eq!(
            m1.internet_message_id.as_deref(),
            Some("<m1@mail.example.com>")
        );
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
        let mut t = MockTransport::new(vec![
            Response::ok(json!({ "value": [folder("FB","Archive"), folder("FA","Inbox")] })),
            Response::ok(json!({ "value": [msg("m1","Moved")], "@odata.deltaLink": "CB" })),
            Response::ok(json!({ "value": [removed("m1")], "@odata.deltaLink": "CA" })),
        ]);
        let r = sync_mail(&mut t, &store, "acc", "2026-06-02T00:00:00Z").unwrap();
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
        let mut t1 = MockTransport::new(vec![
            Response::ok(json!({ "value": [folder("FA","Inbox")] })),
            Response::ok(json!({ "value": [msg("m9","Bye")], "@odata.deltaLink": "C1" })),
        ]);
        sync_mail(&mut t1, &store, "acc", "t").unwrap();
        // second sync: FA reports m9 removed -> it still belongs to FA -> tombstone
        let mut t2 = MockTransport::new(vec![
            Response::ok(json!({ "value": [folder("FA","Inbox")] })),
            Response::ok(json!({ "value": [removed("m9")], "@odata.deltaLink": "C2" })),
        ]);
        let r = sync_mail(&mut t2, &store, "acc", "2026-06-02T00:00:00Z").unwrap();
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
        let mut t = MockTransport::new(vec![
            Response::ok(json!({ "value": [folder("FA","Inbox")] })),
            Response::ok(json!({ "value": [json!({"id":"mx"})], "@odata.deltaLink": "C" })),
        ]);
        sync_mail(&mut t, &store, "acc", "t").unwrap();
        assert_eq!(
            store.get_item("acc", SERVICE, "mx").unwrap().unwrap().name,
            "(no subject)"
        );
    }

    struct MockFetcher(Vec<u8>);
    impl MimeFetcher for MockFetcher {
        fn fetch_mime(&self, _id: &str) -> Result<Vec<u8>, String> {
            Ok(self.0.clone())
        }
    }

    fn store_with_two_messages() -> Store {
        let store = Store::open_in_memory().unwrap();
        for (id, sub) in [("m1", "Hello"), ("m2", "Hi")] {
            let mut it = Item::new("acc", SERVICE, id, sub, "message");
            it.parent_remote_id = Some("FA".into());
            store.upsert_item(&it).unwrap();
        }
        store
    }

    #[test]
    fn downloads_bodies_and_records_local_path() {
        let store = store_with_two_messages();
        let dir = tempfile::tempdir().unwrap();
        let mime = b"From: a@example.com\r\nSubject: Hello\r\n\r\nBody text\r\n".to_vec();
        let f = MockFetcher(mime.clone());

        let r = backup_message_bodies(&f, &store, "acc", dir.path(), 0).unwrap();
        assert_eq!(r.downloaded, 2);
        assert_eq!(r.skipped, 0);
        assert_eq!(r.bytes, (mime.len() * 2) as u64);

        // local_path recorded, file present with the MIME content
        let m1 = store.get_item("acc", SERVICE, "m1").unwrap().unwrap();
        let rel = m1.local_path.expect("local_path set");
        assert!(rel.starts_with("mail/"));
        let path = dir.path().join(&rel);
        assert!(path.exists());
        assert_eq!(std::fs::read(&path).unwrap(), mime);
        // no leftover .part temp file
        assert!(!path.with_extension("eml.part").exists());

        // second pass skips already-downloaded bodies
        let r2 = backup_message_bodies(&f, &store, "acc", dir.path(), 0).unwrap();
        assert_eq!(r2.downloaded, 0);
        assert_eq!(r2.skipped, 2);
    }

    #[test]
    fn index_mail_bodies_extracts_and_indexes_for_search() {
        let store = Store::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // a downloaded message with a real .eml on disk
        let mut m = Item::new("acc", SERVICE, "m1", "Invoice", "message");
        m.local_path = Some("mail/aa/bb/m1.eml".into());
        store.upsert_item(&m).unwrap();
        // a message without a body is skipped
        store
            .upsert_item(&Item::new("acc", SERVICE, "m2", "No body", "message"))
            .unwrap();
        let p = dir.path().join("mail/aa/bb");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(
            p.join("m1.eml"),
            b"Subject: Invoice\r\nContent-Type: text/plain\r\n\r\nThe quarterly shipment of widgets arrived.\r\n",
        )
        .unwrap();

        let n = index_mail_bodies(&store, "acc", dir.path(), 0).unwrap();
        assert_eq!(n, 1, "only the message with an .eml is indexed");
        // the body text is now full-text searchable
        assert_eq!(
            store.search_bodies("acc", "widgets").unwrap(),
            vec![("mail".to_string(), "m1".to_string())]
        );
        // subject is body-searchable too
        assert_eq!(store.search_bodies("acc", "invoice").unwrap().len(), 1);
        // a term not present matches nothing
        assert!(store
            .search_bodies("acc", "nonexistentxyz")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn limit_caps_downloads_per_pass() {
        let store = store_with_two_messages();
        let dir = tempfile::tempdir().unwrap();
        let f = MockFetcher(b"x".to_vec());
        let r = backup_message_bodies(&f, &store, "acc", dir.path(), 1).unwrap();
        assert_eq!(r.downloaded, 1);
        // the other message still has no body -> a later pass picks it up
        let r2 = backup_message_bodies(&f, &store, "acc", dir.path(), 1).unwrap();
        assert_eq!(r2.downloaded, 1);
        assert_eq!(r2.skipped, 1);
    }

    /// Live: real per-folder mail delta -> store, against the throwaway account.
    /// Needs feature `http` + `ISYNCYOU_TEST_TOKEN` carrying `Mail.Read`.
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_incremental_sync_mail() {
        let _gate = crate::live_test_gate();
        let token = match std::env::var("ISYNCYOU_TEST_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!("skipping live_incremental_sync_mail: ISYNCYOU_TEST_TOKEN not set");
                return;
            }
        };
        let store = Store::open_in_memory().unwrap();
        let arch = tempfile::tempdir().unwrap();
        let mut client = isyncyou_graph::GraphClient::new(token);
        let report = incremental_sync_mail(
            &mut client,
            &store,
            "testuser",
            "2026-06-02T00:00:00Z",
            arch.path(),
        )
        .expect("live mail sync should succeed");
        assert!(report.folders > 0, "expected at least one mail folder");
        // every well-known folder must have a persisted cursor after the walk
        assert!(store
            .get_delta_cursor("testuser", SERVICE, "")
            .unwrap()
            .is_none()); // mail uses per-folder scopes, never the "" whole-service scope
        eprintln!(
            "live mail sync: folders={} upserted={} deleted={} skipped={}",
            report.folders, report.upserted, report.deleted, report.skipped
        );
    }

    /// Live: index the mailbox, then download a few real `.eml` bodies and check
    /// they are valid MIME. Needs feature `http` + `ISYNCYOU_TEST_TOKEN`
    /// (`Mail.Read`).
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_download_message_bodies() {
        let _gate = crate::live_test_gate();
        let token = match std::env::var("ISYNCYOU_TEST_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!("skipping live_download_message_bodies: ISYNCYOU_TEST_TOKEN not set");
                return;
            }
        };
        let store = Store::open_in_memory().unwrap();
        let arch = tempfile::tempdir().unwrap();
        let mut client = isyncyou_graph::GraphClient::new(token);
        let idx = incremental_sync_mail(
            &mut client,
            &store,
            "testuser",
            "2026-06-02T00:00:00Z",
            arch.path(),
        )
        .expect("index sync should succeed");
        if idx.upserted == 0 {
            eprintln!("no messages to download; skipping body assertions");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let r = backup_message_bodies(&client, &store, "testuser", dir.path(), 3)
            .expect("body download should succeed");
        assert!(r.downloaded >= 1, "expected at least one .eml");
        assert!(r.bytes > 0, "expected non-empty MIME");

        // verify one downloaded file is plausibly MIME (has a header line).
        let one = store
            .items_by_type("testuser", SERVICE, "message")
            .unwrap()
            .into_iter()
            .find(|m| m.local_path.is_some())
            .unwrap();
        let bytes = std::fs::read(dir.path().join(one.local_path.unwrap())).unwrap();
        assert!(!bytes.is_empty());
        assert!(bytes.contains(&b':'), "MIME should contain header colons");
        eprintln!(
            "live body download: downloaded={} bytes={} (first file {} bytes)",
            r.downloaded,
            r.bytes,
            bytes.len()
        );

        // ...then index those bodies and confirm full-text body search works.
        let n = index_mail_bodies(&store, "testuser", dir.path(), 0)
            .expect("body indexing should succeed");
        assert!(n >= 1, "expected to index at least one body");
        // 'the' is overwhelmingly common in real mail bodies/subjects
        let hits = store.search_bodies("testuser", "the").unwrap();
        eprintln!("live body index: indexed={n}, 'the' hits={}", hits.len());
        assert!(
            !hits.is_empty(),
            "expected body-search hits for a common word"
        );
    }
}
