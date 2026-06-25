//! ToDo connector — per-list task delta into the store (plan §6).
//!
//! Microsoft To Do exposes task lists (`/me/todo/lists`) and a delta query per
//! list (`/me/todo/lists/{id}/tasks/delta`). This syncs the lists first, then
//! walks each list's task delta with a per-list cursor (`scope = list id`),
//! persisted across runs. Tasks are stored id-based (service `"todo"`,
//! `item_type = "task"`). The read app keeps least-privilege `Tasks.Read`.

use crate::archive::{ArchiveReport, JsonFetcher};
use crate::common::{fetch_pages, shard_path};
use crate::onedrive::SyncError;
use isyncyou_graph::{run_delta, DeltaCursor, Transport};
use isyncyou_store::{Item, Store};
use serde_json::Value;
use std::path::Path;

const SERVICE: &str = "todo";
const LISTS_URL: &str = "https://graph.microsoft.com/v1.0/me/todo/lists?$top=100";

/// What one ToDo sync changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TodoReport {
    pub lists: usize,
    pub upserted: usize,
    pub deleted: usize,
    pub skipped: usize,
}

struct TaskList {
    id: String,
    name: String,
    /// The full list resource (`isShared`/`isOwner`/`wellknownListName` and the
    /// rest), archived verbatim as the flank sidecar so the UI can read them
    /// (#567 B1). Kept whole rather than as typed fields: the webui consumes the
    /// sidecar JSON, and a whitelist of typed copies would just drift.
    raw: Value,
}

fn parse_lists(raw: &[Value]) -> Vec<TaskList> {
    raw.iter()
        .filter_map(|l| {
            let id = l.get("id").and_then(Value::as_str)?.to_string();
            let name = l
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Some(TaskList {
                id,
                name,
                raw: l.clone(),
            })
        })
        .collect()
}

/// Sync every task list's tasks incrementally into `store`. `now` is the RFC3339
/// tombstone timestamp.
pub fn incremental_sync_todo<T: Transport>(
    transport: &mut T,
    store: &Store,
    account: &str,
    now: &str,
) -> Result<TodoReport, SyncError> {
    let raw = fetch_pages(transport, LISTS_URL)?;
    let lists = parse_lists(&raw);
    let mut report = TodoReport {
        lists: lists.len(),
        ..Default::default()
    };

    for list in &lists {
        // Record the list itself so tasks can be grouped/restored under it.
        let mut li = Item::new(account, SERVICE, &list.id, &list.name, "list");
        li.sync_state = "remote_dirty".into();
        store.upsert_item(&li)?;

        let base = format!(
            "https://graph.microsoft.com/v1.0/me/todo/lists/{}/tasks/delta",
            list.id
        );
        let cursor = store
            .get_delta_cursor(account, SERVICE, &list.id)?
            .map(DeltaCursor::new);
        let out = run_delta(transport, &base, cursor.as_ref(), 5)?;
        for task in &out.items {
            match ingest_task(store, account, &list.id, task, now)? {
                Ingest::Upserted => report.upserted += 1,
                Ingest::Deleted => report.deleted += 1,
                Ingest::Skipped => report.skipped += 1,
            }
        }
        store.set_delta_cursor(account, SERVICE, &list.id, out.cursor.as_str())?;
    }
    Ok(report)
}

/// Upsert a JSON-snapshot store item under `service="todo"` and archive its
/// canonical JSON to `todo/<shard>/<id>.json` (atomic tmp+rename), recording the
/// relative path as `local_path`. Shared by the list flanks (#567 B1) and the
/// task sub-resource snapshots (#567 B2). Returns the byte count written.
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
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &abs)?;
    let rel = abs.strip_prefix(archive_root).unwrap_or(&abs);
    store.set_local_path(account, SERVICE, id, Some(&rel.to_string_lossy()))?;
    Ok(bytes.len() as u64)
}

/// Back up the task-list flanks (#567 B1): one `item_type="list"` JSON sidecar per
/// list from `/me/todo/lists`, capturing `isShared`/`isOwner`/`wellknownListName`
/// and the rest so the UI can surface them. Re-fetched each pass (small data). The
/// delta pass also upserts a bare list row for task grouping; this enriches it
/// with the archived JSON.
pub fn backup_todo_list_flanks<F: JsonFetcher>(
    fetcher: &F,
    store: &Store,
    account: &str,
    archive_root: &Path,
) -> Result<ArchiveReport, SyncError> {
    let mut report = ArchiveReport::default();
    let lists = fetcher
        .fetch_json("/me/todo/lists?$top=100")
        .map_err(SyncError::Remote)?;
    let raw: Vec<Value> = lists
        .get("value")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for list in parse_lists(&raw) {
        let name = if list.name.is_empty() {
            list.id.as_str()
        } else {
            list.name.as_str()
        };
        report.bytes += archive_json_item(
            store,
            account,
            archive_root,
            &list.id,
            name,
            "list",
            &list.raw,
        )?;
        report.archived += 1;
    }
    Ok(report)
}

/// True when a Graph collection response (`{ "value": [...] }`) is non-empty.
fn has_values(v: &Value) -> bool {
    v.get("value")
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false)
}

/// Back up each task's sub-resources (#567 B2) — the content a plain task GET
/// drops: `checklistItems` (the core steps), `linkedResources`, and file
/// `attachments`. For every archived `task` item (bounded by `limit`, 0 = all):
/// snapshot `checklistItems`/`linkedResources` when non-empty, and `attachments`
/// only when the archived task JSON shows `hasAttachments` (so non-attachment
/// tasks cost no request). Each snapshot is a JSON sidecar under a synthetic id
/// (`_checklist_<id>` / `_linked_<id>` / `_taskatt_<id>`). Best-effort per task;
/// a single failing fetch never aborts the pass.
/// `att_fetcher` is a **separate** fetcher for the attachment list: Microsoft To
/// Do's `.../attachments` endpoint returns 401 `accessDenied` under the read scope
/// (`Tasks.Read`) and only works with `Tasks.ReadWrite`, so the daemon passes its
/// write-scope client here (and `None` when no write token is cached → attachments
/// are skipped, never an error). checklist/linked use the plain `fetcher`.
pub fn backup_task_subresources<F: JsonFetcher>(
    fetcher: &F,
    att_fetcher: Option<&F>,
    store: &Store,
    account: &str,
    archive_root: &Path,
    limit: usize,
) -> Result<ArchiveReport, SyncError> {
    let mut report = ArchiveReport::default();
    let mut processed = 0usize;
    for it in store.items_by_service(account, SERVICE)? {
        if it.item_type != "task" || it.deleted_at.is_some() {
            continue;
        }
        if limit != 0 && processed >= limit {
            break;
        }
        processed += 1;
        let Some(list_id) = it.parent_remote_id.as_deref() else {
            continue;
        };
        let base = format!("/me/todo/lists/{}/tasks/{}", list_id, it.remote_id);

        // checklistItems — the core task content (no flag on the task to gate on).
        if let Ok(cl) = fetcher.fetch_json(&format!("{base}/checklistItems")) {
            if has_values(&cl) {
                report.bytes += archive_json_item(
                    store,
                    account,
                    archive_root,
                    &format!("_checklist_{}", it.remote_id),
                    &format!("{} checklist", it.name),
                    "checklist",
                    &cl,
                )?;
                report.archived += 1;
            }
        }

        // linkedResources — references back to the app/source that created the task.
        if let Ok(lr) = fetcher.fetch_json(&format!("{base}/linkedResources")) {
            if has_values(&lr) {
                report.bytes += archive_json_item(
                    store,
                    account,
                    archive_root,
                    &format!("_linked_{}", it.remote_id),
                    &format!("{} linked", it.name),
                    "linked-resource",
                    &lr,
                )?;
                report.archived += 1;
            }
        }

        // attachments — gated on the archived task JSON's hasAttachments, and only
        // when a write-scope fetcher is available (the read scope is denied here).
        let has_att = it
            .local_path
            .as_deref()
            .and_then(|rel| std::fs::read(archive_root.join(rel)).ok())
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|t| t.get("hasAttachments").and_then(Value::as_bool))
            == Some(true);
        if let (true, Some(af)) = (has_att, att_fetcher) {
            if let Ok(atts) = af.fetch_json(&format!("{base}/attachments")) {
                // The attachments *list* omits `contentBytes` (the file bytes); fetch
                // each attachment in full so the archived snapshot carries the bytes.
                let full: Vec<Value> = atts
                    .get("value")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|a| a.get("id").and_then(Value::as_str))
                    .filter_map(|aid| af.fetch_json(&format!("{base}/attachments/{aid}")).ok())
                    .collect();
                if !full.is_empty() {
                    let snap = serde_json::json!({ "value": full });
                    report.bytes += archive_json_item(
                        store,
                        account,
                        archive_root,
                        &format!("_taskatt_{}", it.remote_id),
                        &format!("{} attachments", it.name),
                        "task-attachment",
                        &snap,
                    )?;
                    report.archived += 1;
                }
            }
        }
    }
    Ok(report)
}

/// List a task's archived attachments from the `_taskatt_<id>` sidecar JSON
/// (Graph `taskFileAttachment[]`) as `(index, filename, content_type, size)`
/// (#567 B4). Empty when the bytes aren't a `{ "value": [...] }` collection.
pub fn list_task_attachments(json_bytes: &[u8]) -> Vec<(usize, String, String, u64)> {
    let v: Value = match serde_json::from_slice(json_bytes) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    v.get("value")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(i, a)| {
                    let name = a
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("attachment")
                        .to_string();
                    let ct = a
                        .get("contentType")
                        .and_then(Value::as_str)
                        .unwrap_or("application/octet-stream")
                        .to_string();
                    let size = a.get("size").and_then(Value::as_u64).unwrap_or(0);
                    (i, name, ct, size)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Extract one task attachment `(filename, content_type, decoded bytes)` from the
/// `_taskatt_<id>` sidecar by index (#567 B4); `None` if the index is out of range
/// or the entry has no inline `contentBytes`.
pub fn extract_task_attachment(json_bytes: &[u8], idx: usize) -> Option<(String, String, Vec<u8>)> {
    let v: Value = serde_json::from_slice(json_bytes).ok()?;
    let a = v.get("value").and_then(Value::as_array)?.get(idx)?;
    let name = a
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("attachment")
        .to_string();
    let ct = a
        .get("contentType")
        .and_then(Value::as_str)
        .unwrap_or("application/octet-stream")
        .to_string();
    let b64 = a.get("contentBytes").and_then(Value::as_str)?;
    Some((name, ct, crate::mime::base64_decode(b64.as_bytes())))
}

enum Ingest {
    Upserted,
    Deleted,
    Skipped,
}

/// Ingest one task-delta entry for a given list.
fn ingest_task(
    store: &Store,
    account: &str,
    list_id: &str,
    task: &Value,
    now: &str,
) -> Result<Ingest, SyncError> {
    let id = task
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| SyncError::Malformed("task has no id".into()))?;

    if task.get("@removed").is_some() {
        let still_here = store
            .get_item(account, SERVICE, id)?
            .map(|it| it.parent_remote_id.as_deref() == Some(list_id))
            .unwrap_or(true);
        if still_here {
            store.mark_deleted(account, SERVICE, id, now)?;
            return Ok(Ingest::Deleted);
        }
        return Ok(Ingest::Skipped);
    }

    let title = task
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("(no title)");
    let mut it = Item::new(account, SERVICE, id, title, "task");
    it.parent_remote_id = Some(list_id.to_string());
    it.etag = task
        .get("@odata.etag")
        .and_then(Value::as_str)
        .or_else(|| task.get("changeKey").and_then(Value::as_str))
        .map(String::from);
    it.remote_mtime = task
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

    fn list(id: &str, name: &str) -> Value {
        json!({ "id": id, "displayName": name })
    }
    fn task(id: &str, title: &str) -> Value {
        json!({
            "id": id,
            "title": title,
            "status": "notStarted",
            "@odata.etag": "W/\"TD\"",
            "lastModifiedDateTime": "2026-04-05T06:07:08Z"
        })
    }
    fn removed(id: &str) -> Value {
        json!({ "id": id, "@removed": { "reason": "deleted" } })
    }

    #[test]
    fn ingests_lists_tasks_and_per_list_cursors() {
        let store = Store::open_in_memory().unwrap();
        let mut t = MockTransport(
            vec![
                Response::ok(json!({ "value": [list("L1","Tasks"), list("L2","Groceries")] })),
                Response::ok(
                    json!({ "value": [task("t1","Write report"), task("t2","Call Bob")], "@odata.deltaLink": "DL1" }),
                ),
                Response::ok(
                    json!({ "value": [task("t3","Buy milk")], "@odata.deltaLink": "DL2" }),
                ),
            ],
            0,
        );
        let r = incremental_sync_todo(&mut t, &store, "acc", "2026-06-02T00:00:00Z").unwrap();
        assert_eq!(r.lists, 2);
        assert_eq!(r.upserted, 3);

        let l1 = store.get_item("acc", SERVICE, "L1").unwrap().unwrap();
        assert_eq!(l1.name, "Tasks");
        assert_eq!(l1.item_type, "list");
        let t1 = store.get_item("acc", SERVICE, "t1").unwrap().unwrap();
        assert_eq!(t1.name, "Write report");
        assert_eq!(t1.item_type, "task");
        assert_eq!(t1.parent_remote_id.as_deref(), Some("L1"));
        assert_eq!(t1.remote_mtime.as_deref(), Some("2026-04-05T06:07:08Z"));
        assert_eq!(
            store
                .get_delta_cursor("acc", SERVICE, "L1")
                .unwrap()
                .as_deref(),
            Some("DL1")
        );
        assert_eq!(
            store
                .get_delta_cursor("acc", SERVICE, "L2")
                .unwrap()
                .as_deref(),
            Some("DL2")
        );
    }

    #[test]
    fn completed_then_removed_task_is_tombstoned() {
        let store = Store::open_in_memory().unwrap();
        let mut t1 = MockTransport(
            vec![
                Response::ok(json!({ "value": [list("L1","Tasks")] })),
                Response::ok(json!({ "value": [task("t9","Old task")], "@odata.deltaLink": "D1" })),
            ],
            0,
        );
        incremental_sync_todo(&mut t1, &store, "acc", "t").unwrap();
        let mut t2 = MockTransport(
            vec![
                Response::ok(json!({ "value": [list("L1","Tasks")] })),
                Response::ok(json!({ "value": [removed("t9")], "@odata.deltaLink": "D2" })),
            ],
            0,
        );
        let r = incremental_sync_todo(&mut t2, &store, "acc", "2026-06-02T00:00:00Z").unwrap();
        assert_eq!(r.deleted, 1);
        assert!(store
            .get_item("acc", SERVICE, "t9")
            .unwrap()
            .unwrap()
            .deleted_at
            .is_some());
    }

    struct MockListFetcher;
    impl JsonFetcher for MockListFetcher {
        fn fetch_json(&self, url: &str) -> std::result::Result<Value, String> {
            if url.contains("/me/todo/lists") {
                Ok(json!({ "value": [
                    { "id": "L1", "displayName": "Tasks", "isShared": false, "isOwner": true, "wellknownListName": "defaultList" },
                    { "id": "L2", "displayName": "Shared groceries", "isShared": true, "isOwner": false, "wellknownListName": "none" },
                ]}))
            } else {
                Ok(json!({ "value": [] }))
            }
        }
    }

    #[test]
    fn backup_todo_list_flanks_writes_sidecars_with_list_fields() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let r = backup_todo_list_flanks(&MockListFetcher, &store, "acc", dir.path()).unwrap();
        assert_eq!(r.archived, 2);

        // each list is an item_type="list" row with a sidecar path on disk
        let l2 = store.get_item("acc", SERVICE, "L2").unwrap().unwrap();
        assert_eq!(l2.item_type, "list");
        let rel = l2
            .local_path
            .expect("list flank must record its sidecar path");
        let body: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join(&rel)).unwrap()).unwrap();
        // the sidecar carries the list-level fields the UI needs
        assert_eq!(body.get("isShared").and_then(Value::as_bool), Some(true));
        assert_eq!(body.get("isOwner").and_then(Value::as_bool), Some(false));
        let l1: Value = {
            let l1 = store.get_item("acc", SERVICE, "L1").unwrap().unwrap();
            serde_json::from_slice(&std::fs::read(dir.path().join(l1.local_path.unwrap())).unwrap())
                .unwrap()
        };
        assert_eq!(
            l1.get("wellknownListName").and_then(Value::as_str),
            Some("defaultList")
        );
    }

    fn seed_task(store: &Store, arch: &Path, id: &str, list: &str, has_att: bool) {
        let mut t = Item::new("acc", SERVICE, id, format!("Task {id}"), "task");
        t.parent_remote_id = Some(list.to_string());
        store.upsert_item(&t).unwrap();
        let body = json!({ "id": id, "title": format!("Task {id}"), "hasAttachments": has_att });
        let abs = shard_path(arch, SERVICE, id, "json");
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, serde_json::to_vec(&body).unwrap()).unwrap();
        let rel = abs.strip_prefix(arch).unwrap();
        store
            .set_local_path("acc", SERVICE, id, Some(&rel.to_string_lossy()))
            .unwrap();
    }

    struct SubFetcher;
    impl JsonFetcher for SubFetcher {
        fn fetch_json(&self, url: &str) -> std::result::Result<Value, String> {
            if url.contains("/checklistItems") {
                Ok(json!({ "value": [
                    { "id": "c1", "displayName": "step 1", "isChecked": true },
                    { "id": "c2", "displayName": "step 2", "isChecked": false },
                ]}))
            } else if url.contains("/linkedResources") {
                // only t1 has a linked resource
                if url.contains("/tasks/t1/") {
                    Ok(
                        json!({ "value": [{ "id": "lr1", "webUrl": "https://x", "applicationName": "Outlook" }] }),
                    )
                } else {
                    Ok(json!({ "value": [] }))
                }
            } else if url.contains("/attachments/") {
                // individual GET carries the file bytes (contentBytes), the list does not
                Ok(json!({ "id": "att1", "name": "spec.pdf",
                    "contentType": "application/pdf", "contentBytes": "QUJD" }))
            } else if url.ends_with("/attachments") {
                // the list omits contentBytes — only metadata + the id
                Ok(json!({ "value": [{ "id": "att1", "name": "spec.pdf",
                    "contentType": "application/pdf", "size": 3 }] }))
            } else {
                Ok(json!({ "value": [] }))
            }
        }
    }

    #[test]
    fn backup_task_subresources_snapshots_checklist_linked_and_gated_attachments() {
        let store = Store::open_in_memory().unwrap();
        let arch = tempfile::tempdir().unwrap();
        seed_task(&store, arch.path(), "t1", "L1", true); // checklist + linked + attachment
        seed_task(&store, arch.path(), "t2", "L1", false); // checklist only (no linked, no attachment gate)

        let r = backup_task_subresources(
            &SubFetcher,
            Some(&SubFetcher),
            &store,
            "acc",
            arch.path(),
            0,
        )
        .unwrap();
        assert_eq!(
            r.archived, 4,
            "t1: checklist+linked+attachment (3) + t2: checklist (1)"
        );

        // without the write-scope att_fetcher, attachments are skipped (Graph denies
        // the read scope on .../attachments): t1 -> checklist+linked, t2 -> checklist.
        let store2 = Store::open_in_memory().unwrap();
        let arch2 = tempfile::tempdir().unwrap();
        seed_task(&store2, arch2.path(), "t1", "L1", true);
        seed_task(&store2, arch2.path(), "t2", "L1", false);
        let r2 =
            backup_task_subresources(&SubFetcher, None, &store2, "acc", arch2.path(), 0).unwrap();
        assert_eq!(r2.archived, 3, "no att_fetcher -> attachments skipped");
        assert!(store2
            .get_item("acc", SERVICE, "_taskatt_t1")
            .unwrap()
            .is_none());

        // checklist sidecar carries the steps with their checked state (#567 core content)
        let cl = store
            .get_item("acc", SERVICE, "_checklist_t1")
            .unwrap()
            .unwrap();
        assert_eq!(cl.item_type, "checklist");
        let body: Value = serde_json::from_slice(
            &std::fs::read(arch.path().join(cl.local_path.unwrap())).unwrap(),
        )
        .unwrap();
        assert_eq!(
            body.pointer("/value/0/isChecked").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            store
                .get_item("acc", SERVICE, "_linked_t1")
                .unwrap()
                .unwrap()
                .item_type,
            "linked-resource"
        );
        let att = store
            .get_item("acc", SERVICE, "_taskatt_t1")
            .unwrap()
            .unwrap();
        assert_eq!(att.item_type, "task-attachment");
        // the attachment snapshot carries the file bytes from the individual GET
        let att_body: Value = serde_json::from_slice(
            &std::fs::read(arch.path().join(att.local_path.unwrap())).unwrap(),
        )
        .unwrap();
        assert_eq!(
            att_body
                .pointer("/value/0/contentBytes")
                .and_then(Value::as_str),
            Some("QUJD")
        );
        // t2 has no linked resource and hasAttachments=false -> only the checklist
        assert!(store
            .get_item("acc", SERVICE, "_linked_t2")
            .unwrap()
            .is_none());
        assert!(store
            .get_item("acc", SERVICE, "_taskatt_t2")
            .unwrap()
            .is_none());
        assert!(store
            .get_item("acc", SERVICE, "_checklist_t2")
            .unwrap()
            .is_some());
    }

    #[test]
    fn parse_lists_keeps_the_full_list_resource() {
        let raw = vec![json!({
            "id": "L9", "displayName": "Work", "isShared": true, "wellknownListName": "flaggedEmails"
        })];
        let lists = parse_lists(&raw);
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0].id, "L9");
        assert_eq!(
            lists[0].raw.get("isShared").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            lists[0]
                .raw
                .get("wellknownListName")
                .and_then(Value::as_str),
            Some("flaggedEmails")
        );
    }

    #[test]
    fn task_without_title_gets_placeholder() {
        let store = Store::open_in_memory().unwrap();
        let mut t = MockTransport(
            vec![
                Response::ok(json!({ "value": [list("L1","Tasks")] })),
                Response::ok(json!({ "value": [json!({"id":"tx"})], "@odata.deltaLink": "D" })),
            ],
            0,
        );
        incremental_sync_todo(&mut t, &store, "acc", "t").unwrap();
        assert_eq!(
            store.get_item("acc", SERVICE, "tx").unwrap().unwrap().name,
            "(no title)"
        );
    }

    /// Live: real per-list task delta -> store, against the throwaway account.
    /// Needs feature `http` + `ISYNCYOU_TEST_TOKEN` carrying `Tasks.Read`.
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_incremental_sync_todo() {
        let _gate = crate::live_test_gate();
        let token = match std::env::var("ISYNCYOU_TEST_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!("skipping live_incremental_sync_todo: ISYNCYOU_TEST_TOKEN not set");
                return;
            }
        };
        let store = Store::open_in_memory().unwrap();
        let mut client = isyncyou_graph::GraphClient::new(token);
        let report = incremental_sync_todo(&mut client, &store, "testuser", "2026-06-02T00:00:00Z")
            .expect("live todo sync should succeed");
        assert!(report.lists > 0, "expected at least one task list");
        eprintln!(
            "live todo sync: lists={} upserted={} deleted={} skipped={}",
            report.lists, report.upserted, report.deleted, report.skipped
        );
    }
}
