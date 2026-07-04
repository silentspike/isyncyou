//! Crash-safe OneDrive cloud-write orchestration over the operation ledger
//! (#onedrive-mobile 0D / #654).
//!
//! Every mutating OneDrive op (create-folder / rename / move / delete / **upload / replace**)
//! records an **idempotent intent in the ledger BEFORE it hits Graph**, so a crash between the
//! Graph mutation and the local reconcile is recoverable without a double effect. Structurally
//! this mirrors the restore ledger ([`crate::restore_recovery`]): the cloud side is
//! abstracted behind [`OneDriveWriteSink`] so the danger (a non-idempotent create/upload) and
//! the recovery can be exercised deterministically with a crash injected at each unsafe point.
//!
//! Safety does **not** come from the cloud being idempotent — a folder create is not. It
//! comes from the ledger plus a per-kind recovery rule ([`CloudOpKind`]): `Delete` is a
//! blind-replay-safe 404=success; `Rename`/`Move` are id-stable so re-issuing the same
//! target is a no-op; `Create`/`Upload` need a probe (list the parent for the name) before a
//! re-issue; `Replace` is etag-guarded (a 412 is a terminal keep-both conflict, never a blind
//! overwrite). Bodies are addressed by parent id + name and re-read from the recorded
//! `local_path` on recovery — the ledger never stores the body. Local/UI convergence is the
//! authoritative job of the scoped delta sync
//! (`incremental_sync_scoped`, #653) plus the client re-list — this driver deliberately does
//! not eagerly mutate the item store, to avoid diverging from the scope-ownership rule.

use isyncyou_core::Config;
use isyncyou_store::{CloudOpKind, CloudWriteOp, Store};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SERVICE: &str = "onedrive";

/// The outcome of a conditional content [`replace`](OneDriveWriteSink::replace_if_match).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplaceOutcome {
    /// The content was replaced; carries the item's new id (etag advanced cloud-side).
    Replaced(String),
    /// `412 Precondition Failed`: the cloud changed since we last saw the etag. The local
    /// edit is **not** overwritten — the caller resolves it keep-both.
    Conflict,
}

/// The terminal result of a ledger-backed cloud-write ([`run_cloud_write`] /
/// [`recover_cloud_write_op`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Applied. Carries the new cloud id for `Create`/`Upload`, else an empty string.
    Applied(String),
    /// A `Replace` hit a `412` conflict — terminal; the caller resolves it keep-both.
    Conflict,
}

/// The cloud side of a OneDrive write, abstracted so recovery can be tested with an
/// injected crash. The production implementation calls Microsoft Graph.
///
/// This is the **unified** cloud-write surface: metadata ops (create-folder / rename / move
/// / delete) **and** body ops (upload / replace). Bodies are addressed by **parent id + name**
/// (not a local path), so a caller with no local store row — a WebUI upload into an Online
/// folder (#657) — can drive it too. #655's offline writeback resolves the parent's remote id
/// from the store; #657 supplies it directly.
pub trait OneDriveWriteSink {
    /// Create a child folder under `parent_id`; returns its new remote id. **Not**
    /// idempotent (a re-issue after success duplicates), so recovery probes with
    /// [`OneDriveWriteSink::find_child_folder`] first.
    fn create_folder(&self, parent_id: &str, name: &str) -> Result<String, String>;

    /// The id of an existing child **folder** named `name` under `parent_id`, if present.
    /// The probe that makes `Create` recovery safe after a crash.
    fn find_child_folder(&self, parent_id: &str, name: &str) -> Result<Option<String>, String>;

    /// The id of an existing child of **any** type (file or folder) named `name` under
    /// `parent_id`. The probe that makes `Upload` recovery safe: an interrupted upload that
    /// had already landed is adopted rather than re-created (a duplicate).
    fn find_child(&self, parent_id: &str, name: &str) -> Result<Option<String>, String>;

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

    /// Upload `bytes` as a new child `name` under `parent_id` (`conflictBehavior=fail`).
    /// Returns the new item id. A name collision is an `Err` — **not** silently clobbered;
    /// recovery probes with [`find_child`](OneDriveWriteSink::find_child) before any re-upload.
    fn upload(&self, parent_id: &str, name: &str, bytes: &[u8]) -> Result<String, String>;

    /// Replace `item_id`'s content, guarded by `etag` (If-Match). [`ReplaceOutcome::Conflict`]
    /// on a `412` (the cloud changed) — the local edit is never silently overwritten.
    fn replace_if_match(
        &self,
        item_id: &str,
        bytes: &[u8],
        etag: &str,
    ) -> Result<ReplaceOutcome, String>;
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

    fn find_child(&self, parent_id: &str, name: &str) -> Result<Option<String>, String> {
        let kids = self.list_children(parent_id).map_err(|e| e.to_string())?;
        Ok(kids
            .iter()
            .find(|c| c.get("name").and_then(|n| n.as_str()) == Some(name))
            .and_then(|c| c.get("id").and_then(|i| i.as_str()))
            .map(str::to_string))
    }

    fn delete(&self, item_id: &str) -> Result<(), String> {
        self.delete_item(item_id).map_err(|e| e.to_string())
    }

    fn upload(&self, parent_id: &str, name: &str, bytes: &[u8]) -> Result<String, String> {
        match self
            .upload_to_parent(parent_id, name, bytes)
            .map_err(|e| e.to_string())?
        {
            Some(v) => v
                .get("id")
                .and_then(|i| i.as_str())
                .map(str::to_string)
                .ok_or_else(|| "upload: Graph response had no id".to_string()),
            // conflictBehavior=fail collided: a child of this name already exists. Not clobbered.
            None => Err(format!(
                "upload conflict: a child named '{name}' already exists under {parent_id}"
            )),
        }
    }

    fn replace_if_match(
        &self,
        item_id: &str,
        bytes: &[u8],
        etag: &str,
    ) -> Result<ReplaceOutcome, String> {
        match self
            .replace_content_if_match(item_id, bytes, etag)
            .map_err(|e| e.to_string())?
        {
            Some(v) => Ok(ReplaceOutcome::Replaced(
                v.get("id")
                    .and_then(|i| i.as_str())
                    .map(str::to_string)
                    .unwrap_or_default(),
            )),
            None => Ok(ReplaceOutcome::Conflict),
        }
    }
}

/// A OneDrive cloud-write request: enough to issue it, dedup a re-issue, and recover a crash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudWrite {
    pub kind: CloudOpKind,
    /// Parent id for `Create` / `Upload`; the item id for `Rename` / `Move` / `Delete` /
    /// `Replace`.
    pub target_id: String,
    /// New / child name for `Create` / `Upload` / `Rename` / `Move`; empty for `Delete` /
    /// `Replace`.
    pub name: String,
    /// Destination parent for `Move` (`None` for an in-place `Rename`).
    pub new_parent_id: Option<String>,
    /// Optimistic-concurrency etag: the If-Match guard for `Replace`; recorded (unused) for
    /// the unconditional delete / id-stable move today.
    pub if_match: Option<String>,
    /// Local body source for `Upload` / `Replace`: the on-disk file whose (decrypted) bytes
    /// are uploaded. Re-read on recovery — the ledger never stores the body itself. `None`
    /// for metadata ops.
    pub local_path: Option<PathBuf>,
    /// A content discriminator folded into the idempotency key so two successive edits of the
    /// **same** item are distinct ledger ops (else the second would dedup to the first and be
    /// lost). Set to the local body hash for `Replace`; `None` for one-shot ops (`Create` /
    /// `Upload` are identified by parent + name).
    pub content_tag: Option<String>,
}

impl CloudWrite {
    /// The op payload persisted in the ledger so boot recovery can reconstruct the action.
    fn intent_json(&self) -> String {
        serde_json::json!({
            "name": self.name,
            "new_parent_id": self.new_parent_id,
            "local_path": self.local_path,
        })
        .to_string()
    }

    /// `(op_id, idempotency_key)` — the key binds account + kind + target + name + new-parent
    /// (+ a content tag when set), so a re-issued identical intent maps to the same ledger row
    /// (never a second effect) and two different ops never collide. The content tag is appended
    /// only when present, so metadata ops keep their #654 keys byte-for-byte.
    fn keys(&self, secret: &[u8], account: &str) -> (String, String) {
        let mut source = format!(
            "{}|{}|{}|{}",
            self.kind.as_str(),
            self.target_id,
            self.name,
            self.new_parent_id.as_deref().unwrap_or("")
        );
        if let Some(tag) = &self.content_tag {
            source.push('|');
            source.push_str(tag);
        }
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

/// The result of issuing one op: applied (with an optional new cloud id) or a `Replace`
/// conflict (a `412` — terminal, keep-both).
enum Issued {
    Done(String),
    Conflict,
}

/// Read the (decrypted) body for an `Upload` / `Replace` from its local source. The ledger
/// never stores the body — it is re-read from disk here (fresh and on recovery), so the
/// bytes always reflect the current on-disk state.
fn read_local_body(local_path: &Option<PathBuf>) -> Result<Vec<u8>, String> {
    let path: &Path = local_path
        .as_deref()
        .ok_or("cloud-write body op has no local_path")?;
    isyncyou_core::envelope::read_body(path).map_err(|e| e.to_string())
}

/// Issue the cloud call for a fresh op. Returns the new cloud id (`Create` / `Upload`) or `""`.
fn issue<S: OneDriveWriteSink>(sink: &S, w: &CloudWrite) -> Result<Issued, String> {
    match w.kind {
        CloudOpKind::Create => sink.create_folder(&w.target_id, &w.name).map(Issued::Done),
        CloudOpKind::Rename => sink
            .rename_or_move(&w.target_id, None, &w.name)
            .map(|_| Issued::Done(String::new())),
        CloudOpKind::Move => sink
            .rename_or_move(&w.target_id, w.new_parent_id.as_deref(), &w.name)
            .map(|_| Issued::Done(String::new())),
        CloudOpKind::Delete => sink.delete(&w.target_id).map(|_| Issued::Done(String::new())),
        CloudOpKind::Upload => {
            let bytes = read_local_body(&w.local_path)?;
            sink.upload(&w.target_id, &w.name, &bytes).map(Issued::Done)
        }
        CloudOpKind::Replace => {
            let bytes = read_local_body(&w.local_path)?;
            match sink.replace_if_match(&w.target_id, &bytes, w.if_match.as_deref().unwrap_or(""))? {
                ReplaceOutcome::Replaced(id) => Ok(Issued::Done(id)),
                ReplaceOutcome::Conflict => Ok(Issued::Conflict),
            }
        }
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
) -> Result<WriteOutcome, String> {
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
    if !store
        .record_cloud_write(&op, now)
        .map_err(|e| e.to_string())?
    {
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
        Ok(Issued::Done(id)) => {
            store
                .set_cloud_write_state(&op_id, "applied", non_empty(&id), None, now)
                .map_err(|e| e.to_string())?;
            Ok(WriteOutcome::Applied(id))
        }
        Ok(Issued::Conflict) => {
            // A `Replace` 412: terminal, not retryable — the cloud changed. Mark `conflict`
            // (never re-picked by `pending_cloud_writes`); the caller resolves it keep-both.
            store
                .set_cloud_write_state(&op_id, "conflict", None, None, now)
                .map_err(|e| e.to_string())?;
            Ok(WriteOutcome::Conflict)
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
) -> Result<WriteOutcome, String> {
    if op.state == "applied" {
        return Ok(WriteOutcome::Applied(op.result_id.clone().unwrap_or_default()));
    }
    if op.state == "conflict" {
        return Ok(WriteOutcome::Conflict); // terminal keep-both — nothing to re-issue
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
    let local_path = intent
        .get("local_path")
        .and_then(|v| v.as_str())
        .map(PathBuf::from);

    let outcome = match kind {
        // 404 == already gone → success. The only blind-replay-safe kind.
        CloudOpKind::Delete => {
            sink.delete(&target)?;
            WriteOutcome::Applied(String::new())
        }
        // Id-stable: re-applying the same target name/parent is a no-op.
        CloudOpKind::Rename => {
            sink.rename_or_move(&target, None, &name)?;
            WriteOutcome::Applied(String::new())
        }
        CloudOpKind::Move => {
            sink.rename_or_move(&target, new_parent.as_deref(), &name)?;
            WriteOutcome::Applied(String::new())
        }
        // The dangerous kinds: probe the parent for the name before re-creating/re-uploading.
        CloudOpKind::Create => match sink.find_child_folder(&target, &name)? {
            Some(id) => WriteOutcome::Applied(id), // the interrupted create had landed
            None => WriteOutcome::Applied(sink.create_folder(&target, &name)?), // safe to create
        },
        CloudOpKind::Upload => match sink.find_child(&target, &name)? {
            Some(id) => WriteOutcome::Applied(id), // the interrupted upload had landed → adopt
            None => {
                // It had not landed — re-read the (still on-disk) local body and re-upload. The
                // upload is `conflictBehavior=fail`, so a concurrent create surfaces as an Err
                // rather than a silent clobber.
                let bytes = read_local_body(&local_path)?;
                WriteOutcome::Applied(sink.upload(&target, &name, &bytes)?)
            }
        },
        CloudOpKind::Replace => {
            // Etag-guarded re-send from the current on-disk body; a 412 (the cloud changed
            // meanwhile) is a terminal conflict — never a blind overwrite.
            let bytes = read_local_body(&local_path)?;
            match sink.replace_if_match(&target, &bytes, op.if_match_etag.as_deref().unwrap_or(""))?
            {
                ReplaceOutcome::Replaced(id) => WriteOutcome::Applied(id),
                ReplaceOutcome::Conflict => WriteOutcome::Conflict,
            }
        }
        other => {
            return Err(format!(
                "recovery unimplemented for kind '{}'",
                other.as_str()
            ))
        }
    };
    match &outcome {
        WriteOutcome::Applied(id) => store
            .set_cloud_write_state(&op.op_id, "applied", non_empty(id), None, now)
            .map_err(|e| e.to_string())?,
        WriteOutcome::Conflict => store
            .set_cloud_write_state(&op.op_id, "conflict", None, None, now)
            .map_err(|e| e.to_string())?,
    };
    Ok(outcome)
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

/// Number of not-yet-terminal cloud-writes for an account — a cheap boot-time probe so the
/// daemon only resolves a token + reconciles when there is actually pending work. `0` when the
/// account's store does not exist yet.
pub fn pending_cloud_write_count(cfg: &Config, account: &str) -> Result<usize, String> {
    let acc = cfg
        .accounts
        .iter()
        .find(|a| a.id == account)
        .ok_or_else(|| format!("no account '{account}' in config"))?;
    let db = acc.archive_root.join(".isyncyou-store.db");
    if !db.exists() {
        return Ok(0);
    }
    let store = Store::open(db).map_err(|e| e.to_string())?;
    Ok(store
        .pending_cloud_writes(account)
        .map_err(|e| e.to_string())?
        .len())
}

/// Boot-recovery entry for the daemon/CLI: resolve the account's store + write client, then
/// reconcile every pending cloud-write (the #654 metadata ops a desktop WebUI may leave
/// mid-flight; the mobile offline pass runs its own recovery inline). Returns the count
/// reconciled.
pub fn recover_pending_cloud_writes_for(cfg: &Config, account: &str) -> Result<usize, String> {
    with_ledger(cfg, account, |store, sink, _secret| {
        recover_pending_cloud_writes(store, account, sink, now_secs())
    })
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
    let secret = crate::restore_key::load_or_create_secret(
        &acc.archive_root.join(".isyncyou-cloudwrite-secret"),
    )?;
    let store =
        Store::open(acc.archive_root.join(".isyncyou-store.db")).map_err(|e| e.to_string())?;
    let token = crate::auth::resolve_cached_sync_token(cfg, account)?;
    let client = isyncyou_graph::GraphClient::new(token);
    run(&store, &client, &secret)
}

/// The applied cloud id from a [`WriteOutcome`] (`""` for a non-create/non-upload or a
/// conflict) — a convenience for the metadata `*_via_ledger` wrappers, which never conflict.
fn applied_id(o: WriteOutcome) -> String {
    match o {
        WriteOutcome::Applied(id) => id,
        WriteOutcome::Conflict => String::new(),
    }
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
            local_path: None,
            content_tag: None,
        };
        run_cloud_write(store, account, &w, sink, secret, now_secs()).map(applied_id)
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
            local_path: None,
            content_tag: None,
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
            local_path: None,
            content_tag: None,
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
            local_path: None,
            content_tag: None,
        };
        run_cloud_write(store, account, &w, sink, secret, now_secs()).map(|_| ())
    })
}

/// Upload the local file at `local_path` as a new child `name` under `parent_id`,
/// ledger-backed. Returns the new item id. The parent is addressed by id (not a path), so a
/// caller without a local store row for the parent — a WebUI upload into an Online folder
/// (#657) — can use it too.
pub fn upload_via_ledger(
    cfg: &Config,
    account: &str,
    parent_id: &str,
    name: &str,
    local_path: &Path,
) -> Result<String, String> {
    with_ledger(cfg, account, |store, sink, secret| {
        let w = CloudWrite {
            kind: CloudOpKind::Upload,
            target_id: parent_id.to_string(),
            name: name.to_string(),
            new_parent_id: None,
            if_match: None,
            local_path: Some(local_path.to_path_buf()),
            content_tag: None,
        };
        run_cloud_write(store, account, &w, sink, secret, now_secs()).map(applied_id)
    })
}

/// Replace item `item_id`'s content with the local file at `local_path`, guarded by `etag`,
/// ledger-backed. Returns the [`WriteOutcome`] so the caller can resolve a `Conflict`
/// keep-both.
pub fn replace_via_ledger(
    cfg: &Config,
    account: &str,
    item_id: &str,
    etag: &str,
    local_path: &Path,
) -> Result<WriteOutcome, String> {
    with_ledger(cfg, account, |store, sink, secret| {
        let bytes = isyncyou_core::envelope::read_body(local_path).map_err(|e| e.to_string())?;
        let w = CloudWrite {
            kind: CloudOpKind::Replace,
            target_id: item_id.to_string(),
            name: String::new(),
            new_parent_id: None,
            if_match: Some(etag.to_string()),
            local_path: Some(local_path.to_path_buf()),
            content_tag: Some(isyncyou_connectors::quickxor_base64(&bytes)),
        };
        run_cloud_write(store, account, &w, sink, secret, now_secs())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A fake cloud file: `(id, parent, name, bytes)`.
    type FakeFile = (String, String, String, Vec<u8>);

    /// A non-idempotent fake OneDrive: `create_folder`/`upload` always append (modelling the
    /// real duplication danger). Safety must come from the recovery logic, not this sink.
    #[derive(Default)]
    struct FakeCloud {
        folders: RefCell<Vec<(String, String, String)>>, // (id, parent, name)
        files: RefCell<Vec<FakeFile>>,                    // (id, parent, name, bytes)
        etags: RefCell<std::collections::HashMap<String, String>>, // item_id -> current etag
        seq: RefCell<u32>,
        create_calls: RefCell<u32>,
        delete_calls: RefCell<u32>,
        move_calls: RefCell<u32>,
        upload_calls: RefCell<u32>,
        replace_calls: RefCell<u32>,
    }
    impl FakeCloud {
        fn count(&self) -> usize {
            self.folders.borrow().len()
        }
        fn file_count(&self) -> usize {
            self.files.borrow().len()
        }
        fn etag_of(&self, id: &str) -> String {
            self.etags.borrow().get(id).cloned().unwrap_or_default()
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
        fn find_child(&self, parent_id: &str, name: &str) -> Result<Option<String>, String> {
            if let Some((id, _, _)) = self
                .folders
                .borrow()
                .iter()
                .find(|(_, p, n)| p == parent_id && n == name)
            {
                return Ok(Some(id.clone()));
            }
            Ok(self
                .files
                .borrow()
                .iter()
                .find(|(_, p, n, _)| p == parent_id && n == name)
                .map(|(id, _, _, _)| id.clone()))
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
        fn upload(&self, parent_id: &str, name: &str, bytes: &[u8]) -> Result<String, String> {
            *self.upload_calls.borrow_mut() += 1;
            // conflictBehavior=fail: a same-named child is a 409 → Err (never a silent clobber).
            if self
                .files
                .borrow()
                .iter()
                .any(|(_, p, n, _)| p == parent_id && n == name)
            {
                return Err(format!("upload conflict: '{name}' exists under {parent_id}"));
            }
            let id = {
                let mut seq = self.seq.borrow_mut();
                *seq += 1;
                format!("file-{seq}")
            };
            self.files.borrow_mut().push((
                id.clone(),
                parent_id.to_string(),
                name.to_string(),
                bytes.to_vec(),
            ));
            self.etags
                .borrow_mut()
                .insert(id.clone(), format!("etag-{id}-1"));
            Ok(id)
        }
        fn replace_if_match(
            &self,
            item_id: &str,
            bytes: &[u8],
            etag: &str,
        ) -> Result<ReplaceOutcome, String> {
            *self.replace_calls.borrow_mut() += 1;
            if self.etag_of(item_id) != etag {
                return Ok(ReplaceOutcome::Conflict); // 412: the cloud moved on
            }
            // Matched → replace the bytes and advance the etag.
            let new_etag = {
                let mut seq = self.seq.borrow_mut();
                *seq += 1;
                format!("etag-{item_id}-{seq}")
            };
            self.etags.borrow_mut().insert(item_id.to_string(), new_etag);
            if let Some(f) = self
                .files
                .borrow_mut()
                .iter_mut()
                .find(|(id, _, _, _)| id == item_id)
            {
                f.3 = bytes.to_vec();
            }
            Ok(ReplaceOutcome::Replaced(item_id.to_string()))
        }
    }

    const SECRET: &[u8] = b"test-secret";
    const ACCT: &str = "me";

    /// A temp file holding `content`, for the Upload/Replace body-source. Returned so the
    /// caller keeps it alive (drop = delete).
    fn body_file(content: &[u8]) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), content).unwrap();
        f
    }

    fn create(target: &str, name: &str) -> CloudWrite {
        CloudWrite {
            kind: CloudOpKind::Create,
            target_id: target.into(),
            name: name.into(),
            new_parent_id: None,
            if_match: None,
            local_path: None,
            content_tag: None,
        }
    }

    // ---- AC1 (host): a cloud-write is ledger-recorded (pending→applied) + issued once ----
    #[test]
    fn run_records_ledger_and_issues_once() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        let out = run_cloud_write(&s, ACCT, &create("parent", "Docs"), &cloud, SECRET, 10).unwrap();
        assert_eq!(out, WriteOutcome::Applied("folder-1".into()));
        assert_eq!(*cloud.create_calls.borrow(), 1);
        assert_eq!(cloud.count(), 1);
        let (_op_id, key) = create("parent", "Docs").keys(SECRET, ACCT);
        let row = s
            .cloud_write_by_key(ACCT, &key)
            .unwrap()
            .expect("ledger row exists");
        assert_eq!(row.state, "applied");
        assert_eq!(row.op_kind, "create");
        assert_eq!(row.result_id.as_deref(), Some("folder-1"));
    }

    #[test]
    fn rename_move_delete_are_ledger_recorded() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        for w in [
            CloudWrite {
                kind: CloudOpKind::Rename,
                target_id: "i1".into(),
                name: "New".into(),
                new_parent_id: None,
                if_match: None,
                local_path: None,
                content_tag: None,
            },
            CloudWrite {
                kind: CloudOpKind::Move,
                target_id: "i2".into(),
                name: "N".into(),
                new_parent_id: Some("p2".into()),
                if_match: None,
                local_path: None,
                content_tag: None,
            },
            CloudWrite {
                kind: CloudOpKind::Delete,
                target_id: "i3".into(),
                name: String::new(),
                new_parent_id: None,
                if_match: None,
                local_path: None,
                content_tag: None,
            },
        ] {
            run_cloud_write(&s, ACCT, &w, &cloud, SECRET, 10).unwrap();
            let (_id, key) = w.keys(SECRET, ACCT);
            assert_eq!(
                s.cloud_write_by_key(ACCT, &key).unwrap().unwrap().state,
                "applied"
            );
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
            op_id: op_id.clone(),
            account_id: ACCT.into(),
            service: SERVICE.into(),
            op_kind: "create".into(),
            target_id: Some("parent".into()),
            idempotency_key: w.keys(SECRET, ACCT).1,
            if_match_etag: None,
            state: "inflight".into(),
            result_id: None,
            intent_json: Some(w.intent_json()),
            attempts: 1,
            last_error: None,
        };
        s.record_cloud_write(&op, 10).unwrap();
        let _landed = cloud.create_folder("parent", "Docs").unwrap(); // the create that landed
        assert_eq!(cloud.count(), 1);
        // [CRASH] before the ledger was set to applied. Boot recovery runs:
        let n = recover_pending_cloud_writes(&s, ACCT, &cloud, 20).unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            *cloud.create_calls.borrow(),
            1,
            "recovery must NOT create a duplicate"
        );
        assert_eq!(cloud.count(), 1);
        assert_eq!(
            s.cloud_write_by_key(ACCT, &w.keys(SECRET, ACCT).1)
                .unwrap()
                .unwrap()
                .state,
            "applied"
        );
    }

    #[test]
    fn create_crash_before_post_creates_one() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        let w = create("parent", "Docs");
        let (op_id, key) = w.keys(SECRET, ACCT);
        let op = CloudWriteOp {
            op_id,
            account_id: ACCT.into(),
            service: SERVICE.into(),
            op_kind: "create".into(),
            target_id: Some("parent".into()),
            idempotency_key: key,
            if_match_etag: None,
            state: "inflight".into(),
            result_id: None,
            intent_json: Some(w.intent_json()),
            attempts: 1,
            last_error: None,
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
        let w = CloudWrite {
            kind: CloudOpKind::Delete,
            target_id: "gone".into(),
            name: String::new(),
            new_parent_id: None,
            if_match: None,
            local_path: None,
            content_tag: None,
        };
        run_cloud_write(&s, ACCT, &w, &cloud, SECRET, 10).unwrap();
        assert_eq!(*cloud.delete_calls.borrow(), 1);
        assert_eq!(
            s.cloud_write_by_key(ACCT, &w.keys(SECRET, ACCT).1)
                .unwrap()
                .unwrap()
                .state,
            "applied"
        );
    }

    // ---- #655: Upload / Replace over the same ledger ------------------------------------

    fn upload(target: &str, name: &str, path: &std::path::Path) -> CloudWrite {
        CloudWrite {
            kind: CloudOpKind::Upload,
            target_id: target.into(),
            name: name.into(),
            new_parent_id: None,
            if_match: None,
            local_path: Some(path.to_path_buf()),
            content_tag: None,
        }
    }

    #[test]
    fn upload_records_ledger_and_issues_once() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        let f = body_file(b"hello");
        let w = upload("parent", "n.txt", f.path());
        let out = run_cloud_write(&s, ACCT, &w, &cloud, SECRET, 10).unwrap();
        assert_eq!(out, WriteOutcome::Applied("file-1".into()));
        assert_eq!(*cloud.upload_calls.borrow(), 1);
        assert_eq!(cloud.file_count(), 1);
        let (_op, key) = w.keys(SECRET, ACCT);
        let row = s.cloud_write_by_key(ACCT, &key).unwrap().unwrap();
        assert_eq!(row.state, "applied");
        assert_eq!(row.op_kind, "upload");
        assert_eq!(row.result_id.as_deref(), Some("file-1"));
    }

    #[test]
    fn upload_crash_recovery_adopts_landed_upload_no_duplicate() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        let f = body_file(b"hello");
        let w = upload("parent", "n.txt", f.path());
        let (op_id, key) = w.keys(SECRET, ACCT);
        let op = CloudWriteOp {
            op_id,
            account_id: ACCT.into(),
            service: SERVICE.into(),
            op_kind: "upload".into(),
            target_id: Some("parent".into()),
            idempotency_key: key,
            if_match_etag: None,
            state: "inflight".into(),
            result_id: None,
            intent_json: Some(w.intent_json()),
            attempts: 1,
            last_error: None,
        };
        s.record_cloud_write(&op, 10).unwrap();
        // The upload LANDED out-of-band, then [CRASH] before the ledger was set applied.
        let _landed = cloud.upload("parent", "n.txt", b"hello").unwrap();
        assert_eq!(cloud.file_count(), 1);
        let n = recover_pending_cloud_writes(&s, ACCT, &cloud, 20).unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            *cloud.upload_calls.borrow(),
            1,
            "recovery must adopt the landed upload, not duplicate it"
        );
        assert_eq!(cloud.file_count(), 1);
        assert_eq!(
            s.cloud_write_by_key(ACCT, &w.keys(SECRET, ACCT).1)
                .unwrap()
                .unwrap()
                .state,
            "applied"
        );
    }

    #[test]
    fn replace_records_ledger_and_issues_once() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        let fid = cloud.upload("p", "f.txt", b"v1").unwrap();
        let etag = cloud.etag_of(&fid);
        let f = body_file(b"v2");
        let w = CloudWrite {
            kind: CloudOpKind::Replace,
            target_id: fid.clone(),
            name: String::new(),
            new_parent_id: None,
            if_match: Some(etag),
            local_path: Some(f.path().to_path_buf()),
            content_tag: Some("tag-v2".into()),
        };
        let out = run_cloud_write(&s, ACCT, &w, &cloud, SECRET, 10).unwrap();
        assert!(matches!(out, WriteOutcome::Applied(_)));
        assert_eq!(*cloud.replace_calls.borrow(), 1);
        // The cloud body was actually replaced.
        assert_eq!(
            cloud
                .files
                .borrow()
                .iter()
                .find(|(id, _, _, _)| id == &fid)
                .unwrap()
                .3,
            b"v2"
        );
        let (_op, key) = w.keys(SECRET, ACCT);
        assert_eq!(
            s.cloud_write_by_key(ACCT, &key).unwrap().unwrap().state,
            "applied"
        );
    }

    #[test]
    fn replace_stale_etag_marks_conflict_and_is_terminal() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        let fid = cloud.upload("p", "f.txt", b"v1").unwrap();
        let f = body_file(b"v2");
        // We hold a STALE etag → the cloud moved on → 412 conflict, never a blind overwrite.
        let w = CloudWrite {
            kind: CloudOpKind::Replace,
            target_id: fid.clone(),
            name: String::new(),
            new_parent_id: None,
            if_match: Some("stale-etag".into()),
            local_path: Some(f.path().to_path_buf()),
            content_tag: Some("tag-v2".into()),
        };
        let out = run_cloud_write(&s, ACCT, &w, &cloud, SECRET, 10).unwrap();
        assert_eq!(out, WriteOutcome::Conflict);
        // The cloud body is untouched.
        assert_eq!(
            cloud
                .files
                .borrow()
                .iter()
                .find(|(id, _, _, _)| id == &fid)
                .unwrap()
                .3,
            b"v1"
        );
        let (_op, key) = w.keys(SECRET, ACCT);
        assert_eq!(
            s.cloud_write_by_key(ACCT, &key).unwrap().unwrap().state,
            "conflict"
        );
        // A conflict row is terminal — boot recovery does not re-pick it.
        let n = recover_pending_cloud_writes(&s, ACCT, &cloud, 20).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn replace_content_tag_distinguishes_successive_edits() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        let fid = cloud.upload("p", "f.txt", b"v1").unwrap();
        let e1 = cloud.etag_of(&fid);
        let f = body_file(b"v2");
        let w1 = CloudWrite {
            kind: CloudOpKind::Replace,
            target_id: fid.clone(),
            name: String::new(),
            new_parent_id: None,
            if_match: Some(e1),
            local_path: Some(f.path().to_path_buf()),
            content_tag: Some("tag-v2".into()),
        };
        run_cloud_write(&s, ACCT, &w1, &cloud, SECRET, 10).unwrap();
        // A second, distinct edit (new content_tag) is a DIFFERENT ledger op, not a dedup.
        let e2 = cloud.etag_of(&fid);
        std::fs::write(f.path(), b"v3").unwrap();
        let w2 = CloudWrite {
            kind: CloudOpKind::Replace,
            target_id: fid.clone(),
            name: String::new(),
            new_parent_id: None,
            if_match: Some(e2),
            local_path: Some(f.path().to_path_buf()),
            content_tag: Some("tag-v3".into()),
        };
        run_cloud_write(&s, ACCT, &w2, &cloud, SECRET, 20).unwrap();
        assert_eq!(
            *cloud.replace_calls.borrow(),
            2,
            "distinct content_tags → two real replaces (no dedup-to-first)"
        );
        assert_ne!(w1.keys(SECRET, ACCT).1, w2.keys(SECRET, ACCT).1);
    }
}
