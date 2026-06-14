//! Crash-safe **ToDo** restore: a [`RestoreSink`] backed by Microsoft To Do, plus the
//! ledger-driven entry point that `restore_cloud` uses for ToDo.
//!
//! ## Why ToDo uses the weakest probe
//!
//! Microsoft To Do tasks (`/me/todo/lists/{id}/tasks`) have **no invisible marker**.
//! A live probe (`tools/live_todo_probe.py`) established that a todoTask **rejects** a
//! `singleValueExtendedProperties` `$filter` (HTTP 400 — no such property on the type),
//! unlike mail/contacts, and has no server-side de-dup like calendar's transactionId.
//! What does work: a marker embedded in the task **body** round-trips, and recovery can
//! find it by **listing the tasks in the parent list and scanning their bodies**.
//!
//! So ToDo uses the ledger + a LIST-scan marker probe. The create is non-idempotent, so
//! crash-safety comes from the ledger + probe (mail-shaped), but the marker is **visible
//! in the restored task body** — a documented fidelity trade-off (see the restore
//! fidelity matrix). The two Graph calls are behind [`ToDoApi`] so the wiring +
//! recovery are unit-tested deterministically; `GraphClient` is the real impl.

use crate::restore_key::{idempotency_key, load_or_create_secret, todo_marker};
use crate::restore_recovery::{recover_restore_op, run_restore_op, RestoreSink};
use isyncyou_core::Config;
use isyncyou_store::{RestoreState, Store};
use serde_json::Value;

/// The two Graph operations a crash-safe ToDo restore needs, abstracted so the ledger
/// wiring can be exercised without a network. Both are scoped to a single task list.
pub trait ToDoApi {
    /// Create a task in `list_id` from a POST-ready JSON body (already sanitized and
    /// carrying the marker in its body); returns the new cloud id.
    fn create_task(&self, list_id: &str, body: &Value) -> Result<String, String>;
    /// Find a task in `list_id` whose body contains `marker` by listing tasks (all
    /// pages) and scanning their bodies; returns its cloud id if present.
    fn find_by_marker(&self, list_id: &str, marker: &str) -> Result<Option<String>, String>;
}

impl ToDoApi for isyncyou_graph::GraphClient {
    fn create_task(&self, list_id: &str, body: &Value) -> Result<String, String> {
        let v = self
            .post_json(&format!("/me/todo/lists/{list_id}/tasks"), body)
            .map_err(|e| e.to_string())?;
        v.get("id")
            .and_then(|i| i.as_str())
            .map(String::from)
            .ok_or_else(|| "created task response has no id".to_string())
    }
    fn find_by_marker(&self, list_id: &str, marker: &str) -> Result<Option<String>, String> {
        // No $filter on a body substring is supported, so page through every task and
        // scan its body content. Follow @odata.nextLink to the end — never cap silently.
        let mut url = format!("/me/todo/lists/{list_id}/tasks?$top=100");
        loop {
            let page = self.get_json(&url).map_err(|e| e.to_string())?;
            if let Some(tasks) = page.get("value").and_then(|v| v.as_array()) {
                for t in tasks {
                    let body = t
                        .get("body")
                        .and_then(|b| b.get("content"))
                        .and_then(|c| c.as_str())
                        .unwrap_or("");
                    if body.contains(marker) {
                        if let Some(id) = t.get("id").and_then(|i| i.as_str()) {
                            return Ok(Some(id.to_string()));
                        }
                    }
                }
            }
            match page.get("@odata.nextLink").and_then(|l| l.as_str()) {
                Some(next) => url = next.to_string(),
                None => return Ok(None),
            }
        }
    }
}

/// A [`RestoreSink`] for ToDo, scoped to one task list. `create` sanitizes the archived
/// task, appends the marker to its body, then posts; `find_by_marker` scans the list.
pub struct ToDoSink<'a, A: ToDoApi> {
    pub api: &'a A,
    pub list_id: String,
}

/// Append the marker to a task's body content so a later LIST scan can find it, keeping
/// the original body. A todoTask body is `{contentType, content}`; if absent, create a
/// text body holding just the marker.
fn embed_marker_in_body(body: &mut Value, marker: &str) {
    let obj = body.as_object_mut().expect("task body is a JSON object");
    match obj.get_mut("body").and_then(|b| b.as_object_mut()) {
        Some(b) => {
            let existing = b.get("content").and_then(|c| c.as_str()).unwrap_or("");
            let joined = if existing.is_empty() {
                marker.to_string()
            } else {
                format!("{existing}\n\n{marker}")
            };
            b.insert("content".to_string(), Value::String(joined));
            b.entry("contentType")
                .or_insert(Value::String("text".to_string()));
        }
        None => {
            obj.insert(
                "body".to_string(),
                serde_json::json!({ "contentType": "text", "content": marker }),
            );
        }
    }
}

impl<A: ToDoApi> RestoreSink for ToDoSink<'_, A> {
    fn create(&self, marker: &str, payload: &[u8]) -> Result<String, String> {
        let task: Value = serde_json::from_slice(payload)
            .map_err(|e| format!("archived task is not JSON: {e}"))?;
        let mut body = isyncyou_connectors::sanitize_task(&task);
        if !body.is_object() {
            return Err("sanitized task is not a JSON object".to_string());
        }
        embed_marker_in_body(&mut body, marker);
        self.api.create_task(&self.list_id, &body)
    }
    fn find_by_marker(&self, marker: &str) -> Result<Option<String>, String> {
        self.api.find_by_marker(&self.list_id, marker)
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The parent task-list id for an archived todo item (its `parent_remote_id`).
fn list_id_of(item: &isyncyou_store::Item) -> Result<String, String> {
    item.parent_remote_id
        .clone()
        .ok_or_else(|| format!("todo item '{}' has no parent list id", item.remote_id))
}

/// Restore one archived task to the cloud **through the operation ledger**. Idempotent:
/// a repeat of the same content recognises the existing operation and either returns the
/// committed id or reconciles an interrupted one (by scanning the list for the body
/// marker) — never a duplicate. Returns the new cloud id.
pub fn restore_todo_via_ledger(
    cfg: &Config,
    account: &str,
    id: &str,
    token: String,
) -> Result<String, String> {
    let acc = cfg
        .accounts
        .iter()
        .find(|a| a.id == account)
        .ok_or_else(|| format!("no account '{account}' in config"))?;
    let (item, bytes) = crate::read_archived_body(cfg, account, "todo", id)?;
    let list_id = list_id_of(&item)?;
    let secret = load_or_create_secret(&acc.archive_root.join(".isyncyou-restore-secret"))?;
    let key = idempotency_key(&secret, account, "todo", id, &bytes);
    let op_id = format!("{account}:{key}");
    let marker = todo_marker(&key);
    let store =
        Store::open(acc.archive_root.join(".isyncyou-store.db")).map_err(|e| e.to_string())?;
    let client = isyncyou_graph::GraphClient::new(token);
    let sink = ToDoSink {
        api: &client,
        list_id,
    };
    finish_todo_restore(
        &store,
        &op_id,
        account,
        id,
        &key,
        &marker,
        &bytes,
        &sink,
        now_secs(),
    )
}

/// The idempotent ledger flow, separated so it can be tested with a fake sink.
#[allow(clippy::too_many_arguments)]
fn finish_todo_restore<S: RestoreSink>(
    store: &Store,
    op_id: &str,
    account: &str,
    source_id: &str,
    key: &str,
    marker: &str,
    payload: &[u8],
    sink: &S,
    now: i64,
) -> Result<String, String> {
    match store
        .get_restore_operation(op_id)
        .map_err(|e| e.to_string())?
    {
        // Already done: return the recorded id (no second create).
        Some(op) if op.state == RestoreState::Committed => op
            .new_cloud_id
            .ok_or_else(|| "committed operation has no cloud id".to_string()),
        // Interrupted earlier: recover (scan for the body marker, or resume).
        Some(_) => {
            recover_restore_op(store, op_id, payload, sink, now)?;
            store
                .get_restore_operation(op_id)
                .map_err(|e| e.to_string())?
                .and_then(|o| o.new_cloud_id)
                .ok_or_else(|| "recovery did not record a cloud id".to_string())
        }
        // Fresh: record intent, then drive the happy path.
        None => {
            store
                .create_restore_operation(op_id, account, "todo", source_id, key, now)
                .map_err(|e| e.to_string())?;
            let (new_id, _) = run_restore_op(store, op_id, marker, payload, sink, now)?;
            Ok(new_id)
        }
    }
}

/// How many non-terminal **todo** restore operations are pending for `account`.
pub fn pending_todo_restore_count(cfg: &Config, account: &str) -> Result<usize, String> {
    let acc = cfg
        .accounts
        .iter()
        .find(|a| a.id == account)
        .ok_or_else(|| format!("no account '{account}' in config"))?;
    let store =
        Store::open(acc.archive_root.join(".isyncyou-store.db")).map_err(|e| e.to_string())?;
    Ok(store
        .recoverable_restore_operations(account)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|o| o.service == "todo")
        .count())
}

/// Read one archived task's JSON + its parent list id from an already-open store.
fn archived_todo(
    store: &Store,
    acc: &isyncyou_core::AccountConfig,
    source_id: &str,
) -> Result<(String, Vec<u8>), String> {
    let item = store
        .get_item(&acc.id, "todo", source_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no archived todo item '{source_id}'"))?;
    let list_id = list_id_of(&item)?;
    let rel = item
        .local_path
        .ok_or_else(|| format!("item '{source_id}' has no archived body"))?;
    let bytes = std::fs::read(acc.archive_root.join(&rel)).map_err(|e| e.to_string())?;
    Ok((list_id, bytes))
}

/// Drive every pending todo restore operation for `account` to a terminal state using
/// `api` (a sink is built per op with that op's task-list id) — the boot-recovery core,
/// with the cloud abstracted so it is testable. Returns `(recovered, still_failing)`.
pub fn recover_pending_todo_restores_with<A: ToDoApi>(
    cfg: &Config,
    account: &str,
    api: &A,
) -> Result<(usize, usize), String> {
    let acc = cfg
        .accounts
        .iter()
        .find(|a| a.id == account)
        .ok_or_else(|| format!("no account '{account}' in config"))?;
    let store =
        Store::open(acc.archive_root.join(".isyncyou-store.db")).map_err(|e| e.to_string())?;
    let ops = store
        .recoverable_restore_operations(account)
        .map_err(|e| e.to_string())?;
    let now = now_secs();
    let (mut ok, mut failed) = (0usize, 0usize);
    for op in ops.into_iter().filter(|o| o.service == "todo") {
        let res = archived_todo(&store, acc, &op.source_item_id).and_then(|(list_id, bytes)| {
            let sink = ToDoSink { api, list_id };
            recover_restore_op(&store, &op.op_id, &bytes, &sink, now).map(|_| ())
        });
        match res {
            Ok(()) => ok += 1,
            Err(_) => failed += 1,
        }
    }
    Ok((ok, failed))
}

/// Boot recovery against the live Graph using `token`. Thin wrapper over
/// [`recover_pending_todo_restores_with`].
pub fn recover_pending_todo_restores(
    cfg: &Config,
    account: &str,
    token: String,
) -> Result<(usize, usize), String> {
    let client = isyncyou_graph::GraphClient::new(token);
    recover_pending_todo_restores_with(cfg, account, &client)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A fake To Do list. `create_task` stores the task keyed by the marker scanned out
    /// of the posted body (so it exercises the real `embed_marker_in_body`), scoped to a
    /// list id, and is deliberately **non-idempotent** — proving the ledger + LIST-scan
    /// probe is what prevents duplicates. Flags simulate the two crash interleavings.
    #[derive(Default)]
    struct FakeToDoApi {
        tasks: RefCell<Vec<(String, String, String)>>, // (list_id, body content, cloud id)
        seq: RefCell<u32>,
        creates: RefCell<u32>,
        crash_after_store: RefCell<bool>,
        fail_before_store: RefCell<bool>,
    }
    impl FakeToDoApi {
        fn count(&self) -> usize {
            self.tasks.borrow().len()
        }
        fn creates(&self) -> u32 {
            *self.creates.borrow()
        }
    }
    impl ToDoApi for FakeToDoApi {
        fn create_task(&self, list_id: &str, body: &Value) -> Result<String, String> {
            *self.creates.borrow_mut() += 1;
            if *self.fail_before_store.borrow() {
                return Err("network failed before reaching Graph".into());
            }
            let content = body
                .get("body")
                .and_then(|b| b.get("content"))
                .and_then(|c| c.as_str())
                .unwrap_or_default()
                .to_string();
            let mut seq = self.seq.borrow_mut();
            *seq += 1;
            let id = format!("task-{}", *seq);
            self.tasks
                .borrow_mut()
                .push((list_id.to_string(), content, id.clone()));
            if *self.crash_after_store.borrow() {
                return Err("network dropped after create".into());
            }
            Ok(id)
        }
        fn find_by_marker(&self, list_id: &str, marker: &str) -> Result<Option<String>, String> {
            Ok(self
                .tasks
                .borrow()
                .iter()
                .find(|(l, content, _)| l == list_id && content.contains(marker))
                .map(|(_, _, id)| id.clone()))
        }
    }

    fn task_json() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "title": "Write the report",
            "body": { "contentType": "text", "content": "draft it" },
            "status": "notStarted",
            // non-writable field the sanitizer must drop:
            "id": "AAMkTaskOLD==",
        }))
        .unwrap()
    }

    fn key_marker(payload: &[u8]) -> (String, String) {
        let key = idempotency_key(b"secret", "acc", "todo", "src1", payload);
        let marker = todo_marker(&key);
        (key, marker)
    }

    #[test]
    fn embed_marker_keeps_existing_body_and_appends() {
        let mut body = serde_json::json!({ "title": "x", "body": { "contentType": "text", "content": "orig" } });
        embed_marker_in_body(&mut body, "MARK");
        let c = body["body"]["content"].as_str().unwrap();
        assert!(c.starts_with("orig"));
        assert!(c.contains("MARK"));
    }

    #[test]
    fn embed_marker_creates_body_when_absent() {
        let mut body = serde_json::json!({ "title": "x" });
        embed_marker_in_body(&mut body, "MARK");
        assert_eq!(body["body"]["content"].as_str(), Some("MARK"));
        assert_eq!(body["body"]["contentType"].as_str(), Some("text"));
    }

    #[test]
    fn happy_path_creates_one_and_is_idempotent_on_repeat() {
        let s = Store::open_in_memory().unwrap();
        let api = FakeToDoApi::default();
        let sink = ToDoSink {
            api: &api,
            list_id: "L1".into(),
        };
        let payload = task_json();
        let (key, marker) = key_marker(&payload);
        let op = format!("acc:{key}");

        let id1 = finish_todo_restore(&s, &op, "acc", "src1", &key, &marker, &payload, &sink, 10)
            .unwrap();
        let id2 = finish_todo_restore(&s, &op, "acc", "src1", &key, &marker, &payload, &sink, 20)
            .unwrap();
        assert_eq!(id1, id2);
        assert_eq!(api.count(), 1);
        assert_eq!(api.creates(), 1);
    }

    #[test]
    fn create_embeds_marker_and_find_scans_for_it() {
        let api = FakeToDoApi::default();
        let sink = ToDoSink {
            api: &api,
            list_id: "L1".into(),
        };
        let payload = task_json();
        let (_key, marker) = key_marker(&payload);
        let id = sink.create(&marker, &payload).unwrap();
        assert_eq!(sink.find_by_marker(&marker).unwrap(), Some(id));
        // a different list id must not match
        assert_eq!(api.find_by_marker("OTHER", &marker).unwrap(), None);
    }

    #[test]
    fn crash_after_post_landed_does_not_duplicate() {
        let s = Store::open_in_memory().unwrap();
        let api = FakeToDoApi::default();
        *api.crash_after_store.borrow_mut() = true;
        let sink = ToDoSink {
            api: &api,
            list_id: "L1".into(),
        };
        let payload = task_json();
        let (key, marker) = key_marker(&payload);
        let op = format!("acc:{key}");

        let first = finish_todo_restore(&s, &op, "acc", "src1", &key, &marker, &payload, &sink, 10);
        assert!(first.is_err());
        assert_eq!(api.count(), 1, "the POST landed");

        *api.crash_after_store.borrow_mut() = false;
        let id = finish_todo_restore(&s, &op, "acc", "src1", &key, &marker, &payload, &sink, 20)
            .unwrap();
        assert!(!id.is_empty());
        assert_eq!(
            api.count(),
            1,
            "no duplicate after recovery (found by marker scan)"
        );
        assert_eq!(api.creates(), 1, "create was not called a second time");
    }

    #[test]
    fn crash_before_post_landed_creates_exactly_one_on_recovery() {
        let s = Store::open_in_memory().unwrap();
        let api = FakeToDoApi::default();
        *api.fail_before_store.borrow_mut() = true;
        let sink = ToDoSink {
            api: &api,
            list_id: "L1".into(),
        };
        let payload = task_json();
        let (key, marker) = key_marker(&payload);
        let op = format!("acc:{key}");

        let first = finish_todo_restore(&s, &op, "acc", "src1", &key, &marker, &payload, &sink, 10);
        assert!(first.is_err());
        assert_eq!(api.count(), 0, "nothing was created");

        *api.fail_before_store.borrow_mut() = false;
        let id = finish_todo_restore(&s, &op, "acc", "src1", &key, &marker, &payload, &sink, 20)
            .unwrap();
        assert!(!id.is_empty());
        assert_eq!(api.count(), 1);
    }

    #[test]
    fn boot_recovery_reconciles_a_pending_op_without_creating() {
        let dir = std::env::temp_dir().join(format!("isyncyou-td-recover-{}", std::process::id()));
        let arch = dir.join("arch");
        std::fs::create_dir_all(arch.join("todo/aa")).unwrap();
        let payload = task_json();
        std::fs::write(arch.join("todo/aa/t.json"), &payload).unwrap();
        let (key, marker) = key_marker(&payload);
        let op_id = format!("acc:{key}");
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut it = isyncyou_store::Item::new("acc", "todo", "src1", "Write", "task");
            it.parent_remote_id = Some("L1".into());
            it.local_path = Some("todo/aa/t.json".into());
            store.upsert_item(&it).unwrap();
            store
                .create_restore_operation(&op_id, "acc", "todo", "src1", &key, 1)
                .unwrap();
            store
                .transition_restore(
                    &op_id,
                    RestoreState::PreflightChecked,
                    2,
                    None,
                    None,
                    Some(&marker),
                )
                .unwrap();
            store
                .transition_restore(&op_id, RestoreState::Committing, 3, None, None, None)
                .unwrap();
            // [CRASH] before committed
        }
        let cfg = Config {
            accounts: vec![isyncyou_core::AccountConfig {
                id: "acc".into(),
                username: "you@example.com".into(),
                sync_root: dir.join("od"),
                archive_root: arch.clone(),
                mount_point: None,
            }],
            ..Default::default()
        };
        // the POST had landed -> the fake already holds the task (in L1, body carries marker)
        let api = FakeToDoApi::default();
        api.tasks.borrow_mut().push((
            "L1".into(),
            format!("draft it\n\n{marker}"),
            "task-1".into(),
        ));
        assert_eq!(pending_todo_restore_count(&cfg, "acc").unwrap(), 1);
        let (ok, failed) = recover_pending_todo_restores_with(&cfg, "acc", &api).unwrap();
        assert_eq!((ok, failed), (1, 0));
        assert_eq!(
            api.creates(),
            0,
            "recovery reconciled by scan; no new create"
        );
        assert_eq!(pending_todo_restore_count(&cfg, "acc").unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
