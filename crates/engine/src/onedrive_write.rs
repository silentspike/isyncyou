//! Crash-safe OneDrive cloud-write orchestration over the operation ledger
//! (#onedrive-mobile 0D / #654).
//!
//! Every mutating OneDrive op (create-folder / rename / move / delete) records an
//! **idempotent intent in the ledger BEFORE it hits Graph**, so a crash between the Graph
//! mutation and the local reconcile is recoverable without a double effect. Structurally
//! this mirrors the restore ledger ([`crate::restore_recovery`]): the cloud side is
//! abstracted behind [`OneDriveWriteSink`] so the danger (a non-idempotent create) and the
//! recovery can be exercised deterministically with a crash injected at each unsafe point.
//!
//! Safety does **not** come from the cloud being idempotent — a folder create is not. It
//! comes from the ledger plus a per-kind recovery rule ([`CloudOpKind`]): `Delete` is a
//! blind-replay-safe 404=success; `Rename`/`Move` are id-stable so re-issuing the same
//! target is a no-op; only `Create` needs a probe (list the parent for the name) before it
//! may re-issue. Local/UI convergence is the authoritative job of the scoped delta sync
//! (`incremental_sync_scoped`, #653) plus the client re-list — this driver deliberately does
//! not eagerly mutate the item store, to avoid diverging from the scope-ownership rule.

use isyncyou_core::Config;
use isyncyou_store::{CloudOpKind, CloudWriteOp, Store};
use std::time::{SystemTime, UNIX_EPOCH};

const SERVICE: &str = "onedrive";

/// The cloud side of a OneDrive write, abstracted so recovery can be tested with an
/// injected crash. The production implementation calls Microsoft Graph.
pub trait OneDriveWriteSink {
    /// Create a child folder under `parent_id`; returns its new remote id. **Not**
    /// idempotent (a re-issue after success duplicates), so recovery probes with
    /// [`OneDriveWriteSink::find_child_folder`] first.
    fn create_folder(&self, parent_id: &str, name: &str) -> Result<String, String>;

    /// The id of an existing child **folder** named `name` under `parent_id`, if present.
    /// The probe that makes `Create` recovery safe after a crash.
    fn find_child_folder(&self, parent_id: &str, name: &str) -> Result<Option<String>, String>;

    /// Rename (`new_parent_id = None`) and/or move an item. Id-stable → re-applying the same
    /// target is a no-op, so recovery may re-issue without a probe.
    fn rename_or_move(
        &self,
        item_id: &str,
        new_parent_id: Option<&str>,
        new_name: &str,
    ) -> Result<(), String>;

    /// Delete an item. Idempotent (`404` == already gone → success): the only blind-replay-
    /// safe kind.
    fn delete(&self, item_id: &str) -> Result<(), String>;
}

impl OneDriveWriteSink for isyncyou_graph::GraphClient {
    fn create_folder(&self, parent_id: &str, name: &str) -> Result<String, String> {
        let v = isyncyou_graph::GraphClient::create_folder(self, parent_id, name)
            .map_err(|e| e.to_string())?;
        v.get("id")
            .and_then(|i| i.as_str())
            .map(str::to_string)
            .ok_or_else(|| "create_folder: Graph response had no id".to_string())
    }

    fn find_child_folder(&self, parent_id: &str, name: &str) -> Result<Option<String>, String> {
        let kids = self.list_children(parent_id).map_err(|e| e.to_string())?;
        Ok(kids
            .iter()
            .find(|c| {
                c.get("folder").is_some() && c.get("name").and_then(|n| n.as_str()) == Some(name)
            })
            .and_then(|c| c.get("id").and_then(|i| i.as_str()))
            .map(str::to_string))
    }

    fn rename_or_move(
        &self,
        item_id: &str,
        new_parent_id: Option<&str>,
        new_name: &str,
    ) -> Result<(), String> {
        isyncyou_graph::GraphClient::move_item(self, item_id, new_parent_id, new_name)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn delete(&self, item_id: &str) -> Result<(), String> {
        self.delete_item(item_id).map_err(|e| e.to_string())
    }
}

/// A OneDrive cloud-write request: enough to issue it, dedup a re-issue, and recover a crash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudWrite {
    pub kind: CloudOpKind,
    /// Parent id for `Create`; the item id for `Rename` / `Move` / `Delete`.
    pub target_id: String,
    /// New / child name for `Create` / `Rename` / `Move`; empty for `Delete`.
    pub name: String,
    /// Destination parent for `Move` (`None` for an in-place `Rename`).
    pub new_parent_id: Option<String>,
    /// Optimistic-concurrency etag, if known (recorded in the ledger; unused by the
    /// unconditional delete / id-stable move today).
    pub if_match: Option<String>,
}

impl CloudWrite {
    /// The op payload persisted in the ledger so boot recovery can reconstruct the action.
    fn intent_json(&self) -> String {
        serde_json::json!({ "name": self.name, "new_parent_id": self.new_parent_id }).to_string()
    }

    /// `(op_id, idempotency_key)` — the key binds account + kind + target + name + new-parent,
    /// so a re-issued identical intent maps to the same ledger row (never a second effect) and
    /// two different ops never collide.
    fn keys(&self, secret: &[u8], account: &str) -> (String, String) {
        let source = format!(
            "{}|{}|{}|{}",
            self.kind.as_str(),
            self.target_id,
            self.name,
            self.new_parent_id.as_deref().unwrap_or("")
        );
        let key = crate::restore_key::idempotency_key(secret, account, SERVICE, &source, &[]);
        (format!("{account}:{key}"), key)
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn non_empty(s: &str) -> Option<&str> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Issue the cloud call for a fresh op. Returns the new cloud id (`Create`) or `""`.
fn issue<S: OneDriveWriteSink>(sink: &S, w: &CloudWrite) -> Result<String, String> {
    match w.kind {
        CloudOpKind::Create => sink.create_folder(&w.target_id, &w.name),
        CloudOpKind::Rename => sink.rename_or_move(&w.target_id, None, &w.name).map(|_| String::new()),
        CloudOpKind::Move => sink
            .rename_or_move(&w.target_id, w.new_parent_id.as_deref(), &w.name)
            .map(|_| String::new()),
        CloudOpKind::Delete => sink.delete(&w.target_id).map(|_| String::new()),
        other => Err(format!("unsupported cloud-write kind '{}'", other.as_str())),
    }
}

/// Drive a fresh cloud-write to completion, recording the ledger intent **before** the Graph
/// call. Idempotent by key: a re-issued identical intent recovers the existing op instead of
/// creating a second cloud effect. Returns the resulting cloud id (`Create`) or `""`.
pub fn run_cloud_write<S: OneDriveWriteSink>(
    store: &Store,
    account: &str,
    w: &CloudWrite,
    sink: &S,
    secret: &[u8],
    now: i64,
) -> Result<String, String> {
    let (op_id, key) = w.keys(secret, account);
    let op = CloudWriteOp {
        op_id: op_id.clone(),
        account_id: account.to_string(),
        service: SERVICE.to_string(),
        op_kind: w.kind.as_str().to_string(),
        target_id: Some(w.target_id.clone()),
        idempotency_key: key.clone(),
        if_match_etag: w.if_match.clone(),
        state: "pending".to_string(),
        result_id: None,
        intent_json: Some(w.intent_json()),
        attempts: 0,
        last_error: None,
    };
    // Record the intent first. A `false` return means this exact intent was already issued
    // (crash/retry) — recover that row instead of re-issuing blindly.
    if !store.record_cloud_write(&op, now).map_err(|e| e.to_string())? {
        let existing = store
            .cloud_write_by_key(account, &key)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("ledger row for key {key} vanished"))?;
        return recover_cloud_write_op(store, &existing, sink, now);
    }
    store
        .set_cloud_write_state(&op_id, "inflight", None, None, now)
        .map_err(|e| e.to_string())?;
    match issue(sink, w) {
        Ok(id) => {
            store
                .set_cloud_write_state(&op_id, "applied", non_empty(&id), None, now)
                .map_err(|e| e.to_string())?;
            Ok(id)
        }
        Err(e) => {
            // The op genuinely failed (or its outcome is unknown). Leave it recoverable: a
            // user retry with the same key re-enters via `record_cloud_write`==false →
            // `recover_cloud_write_op`, which probes before any re-issue.
            store
                .set_cloud_write_state(&op_id, "inflight", None, Some(&e), now)
                .map_err(|x| x.to_string())?;
            Err(e)
        }
    }
}

/// Reconcile a non-`applied` ledger op to a terminal `applied` state **without a double
/// effect**, per its [`CloudOpKind`] recovery rule. Called both by boot recovery and by the
/// re-issue path of [`run_cloud_write`]. Returns the op's cloud id (`Create`) or `""`.
pub fn recover_cloud_write_op<S: OneDriveWriteSink>(
    store: &Store,
    op: &CloudWriteOp,
    sink: &S,
    now: i64,
) -> Result<String, String> {
    if op.state == "applied" {
        return Ok(op.result_id.clone().unwrap_or_default());
    }
    let kind = CloudOpKind::parse(&op.op_kind)
        .ok_or_else(|| format!("unknown ledger op_kind '{}'", op.op_kind))?;
    let target = op.target_id.clone().unwrap_or_default();
    let intent: serde_json::Value = op
        .intent_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(serde_json::Value::Null);
    let name = intent
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let new_parent = intent
        .get("new_parent_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let result_id = match kind {
        // 404 == already gone → success. The only blind-replay-safe kind.
        CloudOpKind::Delete => {
            sink.delete(&target)?;
            String::new()
        }
        // Id-stable: re-applying the same target name/parent is a no-op.
        CloudOpKind::Rename => {
            sink.rename_or_move(&target, None, &name)?;
            String::new()
        }
        CloudOpKind::Move => {
            sink.rename_or_move(&target, new_parent.as_deref(), &name)?;
            String::new()
        }
        // The dangerous kind: probe the parent for the name before re-creating.
        CloudOpKind::Create => match sink.find_child_folder(&target, &name)? {
            Some(id) => id,                            // the interrupted create had landed
            None => sink.create_folder(&target, &name)?, // it had not — safe to create
        },
        other => return Err(format!("recovery unimplemented for kind '{}'", other.as_str())),
    };
    store
        .set_cloud_write_state(&op.op_id, "applied", non_empty(&result_id), None, now)
        .map_err(|e| e.to_string())?;
    Ok(result_id)
}

/// Boot recovery: reconcile every not-yet-terminal cloud-write for an account. Returns the
/// number of ops reconciled. Call once at engine start, before serving writes.
pub fn recover_pending_cloud_writes<S: OneDriveWriteSink>(
    store: &Store,
    account: &str,
    sink: &S,
    now: i64,
) -> Result<usize, String> {
    let pending = store
        .pending_cloud_writes(account)
        .map_err(|e| e.to_string())?;
    for op in &pending {
        recover_cloud_write_op(store, op, sink, now)?;
    }
    Ok(pending.len())
}

// ---- public per-verb entries (the daemon/mobile handler calls these) ----------------------

/// Resolve the account's store + write token + a live Graph sink, then run one cloud-write
/// over the ledger. Mirrors `restore_mail_via_ledger`'s store/secret/sink construction.
fn with_ledger<R>(
    cfg: &Config,
    account: &str,
    run: impl FnOnce(&Store, &isyncyou_graph::GraphClient, &[u8]) -> Result<R, String>,
) -> Result<R, String> {
    let acc = cfg
        .accounts
        .iter()
        .find(|a| a.id == account)
        .ok_or_else(|| format!("no account '{account}' in config"))?;
    let secret =
        crate::restore_key::load_or_create_secret(&acc.archive_root.join(".isyncyou-cloudwrite-secret"))?;
    let store = Store::open(acc.archive_root.join(".isyncyou-store.db")).map_err(|e| e.to_string())?;
    let token = crate::auth::resolve_cached_sync_token(cfg, account)?;
    let client = isyncyou_graph::GraphClient::new(token);
    run(&store, &client, &secret)
}

/// Create a child folder named `name` under `parent_id`, ledger-backed. Returns its new id.
pub fn create_folder_via_ledger(
    cfg: &Config,
    account: &str,
    parent_id: &str,
    name: &str,
) -> Result<String, String> {
    with_ledger(cfg, account, |store, sink, secret| {
        let w = CloudWrite {
            kind: CloudOpKind::Create,
            target_id: parent_id.to_string(),
            name: name.to_string(),
            new_parent_id: None,
            if_match: None,
        };
        run_cloud_write(store, account, &w, sink, secret, now_secs())
    })
}

/// Rename an item in place, ledger-backed.
pub fn rename_via_ledger(
    cfg: &Config,
    account: &str,
    item_id: &str,
    new_name: &str,
) -> Result<(), String> {
    with_ledger(cfg, account, |store, sink, secret| {
        let w = CloudWrite {
            kind: CloudOpKind::Rename,
            target_id: item_id.to_string(),
            name: new_name.to_string(),
            new_parent_id: None,
            if_match: None,
        };
        run_cloud_write(store, account, &w, sink, secret, now_secs()).map(|_| ())
    })
}

/// Move an item to `new_parent_id` (optionally renaming it), ledger-backed.
pub fn move_via_ledger(
    cfg: &Config,
    account: &str,
    item_id: &str,
    new_parent_id: Option<&str>,
    new_name: &str,
) -> Result<(), String> {
    with_ledger(cfg, account, |store, sink, secret| {
        let w = CloudWrite {
            kind: CloudOpKind::Move,
            target_id: item_id.to_string(),
            name: new_name.to_string(),
            new_parent_id: new_parent_id.map(str::to_string),
            if_match: None,
        };
        run_cloud_write(store, account, &w, sink, secret, now_secs()).map(|_| ())
    })
}

/// Delete an item, ledger-backed.
pub fn delete_via_ledger(cfg: &Config, account: &str, item_id: &str) -> Result<(), String> {
    with_ledger(cfg, account, |store, sink, secret| {
        let w = CloudWrite {
            kind: CloudOpKind::Delete,
            target_id: item_id.to_string(),
            name: String::new(),
            new_parent_id: None,
            if_match: None,
        };
        run_cloud_write(store, account, &w, sink, secret, now_secs()).map(|_| ())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A non-idempotent fake OneDrive: `create_folder` always appends a new folder (modelling
    /// the real duplication danger). Safety must come from the recovery logic, not this sink.
    #[derive(Default)]
    struct FakeCloud {
        folders: RefCell<Vec<(String, String, String)>>, // (id, parent, name)
        seq: RefCell<u32>,
        create_calls: RefCell<u32>,
        delete_calls: RefCell<u32>,
        move_calls: RefCell<u32>,
    }
    impl FakeCloud {
        fn count(&self) -> usize {
            self.folders.borrow().len()
        }
    }
    impl OneDriveWriteSink for FakeCloud {
        fn create_folder(&self, parent_id: &str, name: &str) -> Result<String, String> {
            *self.create_calls.borrow_mut() += 1;
            let mut seq = self.seq.borrow_mut();
            *seq += 1;
            let id = format!("folder-{seq}");
            self.folders
                .borrow_mut()
                .push((id.clone(), parent_id.to_string(), name.to_string()));
            Ok(id)
        }
        fn find_child_folder(&self, parent_id: &str, name: &str) -> Result<Option<String>, String> {
            Ok(self
                .folders
                .borrow()
                .iter()
                .find(|(_, p, n)| p == parent_id && n == name)
                .map(|(id, _, _)| id.clone()))
        }
        fn rename_or_move(
            &self,
            _item_id: &str,
            _new_parent_id: Option<&str>,
            _new_name: &str,
        ) -> Result<(), String> {
            *self.move_calls.borrow_mut() += 1;
            Ok(())
        }
        fn delete(&self, item_id: &str) -> Result<(), String> {
            *self.delete_calls.borrow_mut() += 1;
            self.folders.borrow_mut().retain(|(id, _, _)| id != item_id);
            Ok(()) // 404 == already gone → success
        }
    }

    const SECRET: &[u8] = b"test-secret";
    const ACCT: &str = "me";

    fn create(target: &str, name: &str) -> CloudWrite {
        CloudWrite {
            kind: CloudOpKind::Create,
            target_id: target.into(),
            name: name.into(),
            new_parent_id: None,
            if_match: None,
        }
    }

    // ---- AC1 (host): a cloud-write is ledger-recorded (pending→applied) + issued once ----
    #[test]
    fn run_records_ledger_and_issues_once() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        let id = run_cloud_write(&s, ACCT, &create("parent", "Docs"), &cloud, SECRET, 10).unwrap();
        assert_eq!(id, "folder-1");
        assert_eq!(*cloud.create_calls.borrow(), 1);
        assert_eq!(cloud.count(), 1);
        let (_op_id, key) = create("parent", "Docs").keys(SECRET, ACCT);
        let row = s.cloud_write_by_key(ACCT, &key).unwrap().expect("ledger row exists");
        assert_eq!(row.state, "applied");
        assert_eq!(row.op_kind, "create");
        assert_eq!(row.result_id.as_deref(), Some("folder-1"));
    }

    #[test]
    fn rename_move_delete_are_ledger_recorded() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        for w in [
            CloudWrite { kind: CloudOpKind::Rename, target_id: "i1".into(), name: "New".into(), new_parent_id: None, if_match: None },
            CloudWrite { kind: CloudOpKind::Move, target_id: "i2".into(), name: "N".into(), new_parent_id: Some("p2".into()), if_match: None },
            CloudWrite { kind: CloudOpKind::Delete, target_id: "i3".into(), name: String::new(), new_parent_id: None, if_match: None },
        ] {
            run_cloud_write(&s, ACCT, &w, &cloud, SECRET, 10).unwrap();
            let (_id, key) = w.keys(SECRET, ACCT);
            assert_eq!(s.cloud_write_by_key(ACCT, &key).unwrap().unwrap().state, "applied");
        }
        assert_eq!(*cloud.move_calls.borrow(), 2); // rename + move
        assert_eq!(*cloud.delete_calls.borrow(), 1);
    }

    // ---- AC2: crash between the Graph mutation and the ledger update → no double effect ----
    #[test]
    fn create_crash_after_post_landed_no_duplicate() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        let w = create("parent", "Docs");
        let (op_id, _key) = w.keys(SECRET, ACCT);
        // Record intent + go inflight, then the POST LANDS out-of-band...
        let op = CloudWriteOp {
            op_id: op_id.clone(), account_id: ACCT.into(), service: SERVICE.into(),
            op_kind: "create".into(), target_id: Some("parent".into()),
            idempotency_key: w.keys(SECRET, ACCT).1, if_match_etag: None,
            state: "inflight".into(), result_id: None, intent_json: Some(w.intent_json()),
            attempts: 1, last_error: None,
        };
        s.record_cloud_write(&op, 10).unwrap();
        let _landed = cloud.create_folder("parent", "Docs").unwrap(); // the create that landed
        assert_eq!(cloud.count(), 1);
        // [CRASH] before the ledger was set to applied. Boot recovery runs:
        let n = recover_pending_cloud_writes(&s, ACCT, &cloud, 20).unwrap();
        assert_eq!(n, 1);
        assert_eq!(*cloud.create_calls.borrow(), 1, "recovery must NOT create a duplicate");
        assert_eq!(cloud.count(), 1);
        assert_eq!(s.cloud_write_by_key(ACCT, &w.keys(SECRET, ACCT).1).unwrap().unwrap().state, "applied");
    }

    #[test]
    fn create_crash_before_post_creates_one() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        let w = create("parent", "Docs");
        let (op_id, key) = w.keys(SECRET, ACCT);
        let op = CloudWriteOp {
            op_id, account_id: ACCT.into(), service: SERVICE.into(), op_kind: "create".into(),
            target_id: Some("parent".into()), idempotency_key: key, if_match_etag: None,
            state: "inflight".into(), result_id: None, intent_json: Some(w.intent_json()),
            attempts: 1, last_error: None,
        };
        s.record_cloud_write(&op, 10).unwrap();
        // [CRASH] the POST never happened → the folder is not in the cloud.
        recover_pending_cloud_writes(&s, ACCT, &cloud, 20).unwrap();
        assert_eq!(*cloud.create_calls.borrow(), 1);
        assert_eq!(cloud.count(), 1);
    }

    #[test]
    fn reissued_identical_intent_makes_no_second_effect() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        let w = create("parent", "Docs");
        let id1 = run_cloud_write(&s, ACCT, &w, &cloud, SECRET, 10).unwrap();
        // A retry of the exact same intent (same key) must adopt, not create again.
        let id2 = run_cloud_write(&s, ACCT, &w, &cloud, SECRET, 20).unwrap();
        assert_eq!(id1, id2);
        assert_eq!(*cloud.create_calls.borrow(), 1, "dedup: no second create");
        assert_eq!(cloud.count(), 1);
    }

    #[test]
    fn delete_recovery_is_blind_replay_safe() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        // Nothing in the cloud (already deleted). A delete op re-issued on recovery succeeds.
        let w = CloudWrite { kind: CloudOpKind::Delete, target_id: "gone".into(), name: String::new(), new_parent_id: None, if_match: None };
        run_cloud_write(&s, ACCT, &w, &cloud, SECRET, 10).unwrap();
        assert_eq!(*cloud.delete_calls.borrow(), 1);
        assert_eq!(s.cloud_write_by_key(ACCT, &w.keys(SECRET, ACCT).1).unwrap().unwrap().state, "applied");
    }
}
