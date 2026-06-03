//! ToDo connector — per-list task delta into the store (plan §6).
//!
//! Microsoft To Do exposes task lists (`/me/todo/lists`) and a delta query per
//! list (`/me/todo/lists/{id}/tasks/delta`). This syncs the lists first, then
//! walks each list's task delta with a per-list cursor (`scope = list id`),
//! persisted across runs. Tasks are stored id-based (service `"todo"`,
//! `item_type = "task"`). The read app keeps least-privilege `Tasks.Read`.

use crate::common::fetch_pages;
use crate::onedrive::SyncError;
use isyncyou_graph::{run_delta, DeltaCursor, Transport};
use isyncyou_store::{Item, Store};
use serde_json::Value;

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
            Some(TaskList { id, name })
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
