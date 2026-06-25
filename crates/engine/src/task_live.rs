//! Live ToDo **write** layer (#567 B5): the live client's task + checklist + list
//! verbs behind [`TaskWriter`] so the engine wiring + the daemon handler are
//! unit-tested deterministically without a network; `GraphClient` is the real
//! impl.
//!
//! Like `calendar_live`/`contacts_live` (and unlike the crash-safe, ledger-backed
//! `restore_todo`), this is the *interactive* path: the user creates/edits/
//! completes a task, ticks checklist items, or manages lists in the live client
//! and the change is pushed straight to Microsoft 365. The write token is the full
//! restore-scope token (`Tasks.ReadWrite`, from #558), resolved from the cached
//! `login --write`. Create/update task bodies are sanitized to the writable task
//! whitelist (`sanitize_task`); ids are URL-encoded by the graph layer.

use isyncyou_core::Config;
use serde_json::{json, Value};

/// The live ToDo write operations, object-safe so the daemon can hold a
/// `&dyn TaskWriter` and tests can swap in a fake.
pub trait TaskWriter {
    /// Create a task in a list from a composed/archived task JSON (sanitized);
    /// returns the new cloud id.
    fn create(&self, list_id: &str, task: &Value) -> Result<String, String>;
    /// Update a task's writable fields (the patch is sanitized first).
    fn update(&self, list_id: &str, task_id: &str, patch: &Value) -> Result<(), String>;
    /// Mark a task completed (`status = "completed"`).
    fn complete(&self, list_id: &str, task_id: &str) -> Result<(), String>;
    /// Delete a task.
    fn delete(&self, list_id: &str, task_id: &str) -> Result<(), String>;
    /// Add a checklist item (step) to a task; returns the new item id.
    fn checklist_add(&self, list_id: &str, task_id: &str, title: &str) -> Result<String, String>;
    /// Tick / untick a checklist item.
    fn checklist_toggle(
        &self,
        list_id: &str,
        task_id: &str,
        item_id: &str,
        checked: bool,
    ) -> Result<(), String>;
    /// Delete a checklist item.
    fn checklist_delete(&self, list_id: &str, task_id: &str, item_id: &str) -> Result<(), String>;
    /// Create a task list; returns the new list id.
    fn list_create(&self, name: &str) -> Result<String, String>;
    /// Delete a task list.
    fn list_delete(&self, list_id: &str) -> Result<(), String>;
}

fn id_of(v: &Value, what: &str) -> Result<String, String> {
    v.get("id")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| format!("{what} response has no id"))
}

// Inherent GraphClient methods share names with the trait (`create`/`update`/
// `delete`), so each delegation is fully qualified to call the inherent (HTTP)
// method, never recurse.
impl TaskWriter for isyncyou_graph::GraphClient {
    fn create(&self, list_id: &str, task: &Value) -> Result<String, String> {
        let body = isyncyou_connectors::sanitize_task(task);
        let v = isyncyou_graph::GraphClient::create_task(self, list_id, &body)
            .map_err(|e| e.to_string())?;
        id_of(&v, "created task")
    }
    fn update(&self, list_id: &str, task_id: &str, patch: &Value) -> Result<(), String> {
        let body = isyncyou_connectors::sanitize_task(patch);
        isyncyou_graph::GraphClient::update_task(self, list_id, task_id, &body)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    fn complete(&self, list_id: &str, task_id: &str) -> Result<(), String> {
        isyncyou_graph::GraphClient::update_task(
            self,
            list_id,
            task_id,
            &json!({"status":"completed"}),
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
    }
    fn delete(&self, list_id: &str, task_id: &str) -> Result<(), String> {
        isyncyou_graph::GraphClient::delete_task(self, list_id, task_id).map_err(|e| e.to_string())
    }
    fn checklist_add(&self, list_id: &str, task_id: &str, title: &str) -> Result<String, String> {
        let v = isyncyou_graph::GraphClient::create_checklist_item(
            self,
            list_id,
            task_id,
            &json!({ "displayName": title }),
        )
        .map_err(|e| e.to_string())?;
        id_of(&v, "created checklist item")
    }
    fn checklist_toggle(
        &self,
        list_id: &str,
        task_id: &str,
        item_id: &str,
        checked: bool,
    ) -> Result<(), String> {
        isyncyou_graph::GraphClient::update_checklist_item(
            self,
            list_id,
            task_id,
            item_id,
            &json!({ "isChecked": checked }),
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
    }
    fn checklist_delete(&self, list_id: &str, task_id: &str, item_id: &str) -> Result<(), String> {
        isyncyou_graph::GraphClient::delete_checklist_item(self, list_id, task_id, item_id)
            .map_err(|e| e.to_string())
    }
    fn list_create(&self, name: &str) -> Result<String, String> {
        let v =
            isyncyou_graph::GraphClient::create_todo_list(self, &json!({ "displayName": name }))
                .map_err(|e| e.to_string())?;
        id_of(&v, "created list")
    }
    fn list_delete(&self, list_id: &str) -> Result<(), String> {
        isyncyou_graph::GraphClient::delete_todo_list(self, list_id).map_err(|e| e.to_string())
    }
}

/// Resolve the full write token (restore scopes incl. `Tasks.ReadWrite`) and
/// build a ready `GraphClient` for the live-ToDo write ops. The token is silently
/// refreshed from the cached `login --write`; a missing cache is an error. This is
/// the daemon's entry point into the layer.
pub fn task_writer(cfg: &Config, account: &str) -> Result<isyncyou_graph::GraphClient, String> {
    let token = crate::auth::resolve_cached_restore_token(cfg, account)?;
    Ok(isyncyou_graph::GraphClient::new(token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakeTasks {
        log: RefCell<Vec<String>>,
    }
    impl TaskWriter for FakeTasks {
        fn create(&self, list_id: &str, task: &Value) -> Result<String, String> {
            self.log.borrow_mut().push(format!(
                "create list={list_id} title={}",
                task.get("title").and_then(Value::as_str).unwrap_or("")
            ));
            Ok("new-task-1".into())
        }
        fn update(&self, list_id: &str, task_id: &str, _patch: &Value) -> Result<(), String> {
            self.log
                .borrow_mut()
                .push(format!("update list={list_id} id={task_id}"));
            Ok(())
        }
        fn complete(&self, list_id: &str, task_id: &str) -> Result<(), String> {
            self.log
                .borrow_mut()
                .push(format!("complete list={list_id} id={task_id}"));
            Ok(())
        }
        fn delete(&self, list_id: &str, task_id: &str) -> Result<(), String> {
            self.log
                .borrow_mut()
                .push(format!("delete list={list_id} id={task_id}"));
            Ok(())
        }
        fn checklist_add(
            &self,
            list_id: &str,
            task_id: &str,
            title: &str,
        ) -> Result<String, String> {
            self.log.borrow_mut().push(format!(
                "cl_add list={list_id} task={task_id} title={title}"
            ));
            Ok("ci-1".into())
        }
        fn checklist_toggle(
            &self,
            _list_id: &str,
            task_id: &str,
            item_id: &str,
            checked: bool,
        ) -> Result<(), String> {
            self.log.borrow_mut().push(format!(
                "cl_toggle task={task_id} item={item_id} checked={checked}"
            ));
            Ok(())
        }
        fn checklist_delete(
            &self,
            _list_id: &str,
            task_id: &str,
            item_id: &str,
        ) -> Result<(), String> {
            self.log
                .borrow_mut()
                .push(format!("cl_del task={task_id} item={item_id}"));
            Ok(())
        }
        fn list_create(&self, name: &str) -> Result<String, String> {
            self.log.borrow_mut().push(format!("list_create {name}"));
            Ok("L-new".into())
        }
        fn list_delete(&self, list_id: &str) -> Result<(), String> {
            self.log.borrow_mut().push(format!("list_delete {list_id}"));
            Ok(())
        }
    }

    #[test]
    fn task_writer_is_object_safe_and_ops_carry_ids() {
        let f = FakeTasks::default();
        let w: &dyn TaskWriter = &f; // compiles only if the trait is object-safe
        assert_eq!(
            w.create("L1", &json!({ "title": "Ship" })).unwrap(),
            "new-task-1"
        );
        w.update("L1", "t1", &json!({ "importance": "high" }))
            .unwrap();
        w.complete("L1", "t1").unwrap();
        assert_eq!(w.checklist_add("L1", "t1", "step 1").unwrap(), "ci-1");
        w.checklist_toggle("L1", "t1", "ci1", true).unwrap();
        w.checklist_delete("L1", "t1", "ci1").unwrap();
        assert_eq!(w.list_create("Groceries").unwrap(), "L-new");
        w.delete("L1", "t1").unwrap();
        w.list_delete("L1").unwrap();
        let log = f.log.borrow();
        assert_eq!(log[0], "create list=L1 title=Ship");
        assert_eq!(log[1], "update list=L1 id=t1");
        assert_eq!(log[2], "complete list=L1 id=t1");
        assert_eq!(log[3], "cl_add list=L1 task=t1 title=step 1");
        assert_eq!(log[4], "cl_toggle task=t1 item=ci1 checked=true");
        assert_eq!(log[5], "cl_del task=t1 item=ci1");
        assert_eq!(log[6], "list_create Groceries");
        assert_eq!(log[7], "delete list=L1 id=t1");
        assert_eq!(log[8], "list_delete L1");
    }
}
