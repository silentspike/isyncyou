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
use isyncyou_graph::{DrivePermission, InviteOutcome};
use isyncyou_store::{CloudOpKind, CloudWriteOp, Store};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SERVICE: &str = "onedrive";
const INVITE_NOT_STARTED: &str = "invite_not_started_user_retry_required";
const INVITE_AMBIGUOUS: &str = "invite_recovery_ambiguous";
const INVITE_PARTIAL_SUCCESS: &str = "invite_partial_success";

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareKind {
    Link,
    Invite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareIntent {
    pub kind: ShareKind,
    pub item_id: String,
    pub link_type: Option<String>,
    pub scope: Option<String>,
    pub role: Option<String>,
    pub recipient_hashes: Vec<String>,
    pub recipient_count: usize,
    pub require_sign_in: bool,
    pub send_invitation: bool,
}

impl ShareIntent {
    pub fn link(item_id: &str, link_type: &str, scope: &str) -> Result<Self, String> {
        validate_share_item_id(item_id)?;
        validate_share_link_type(link_type)?;
        validate_share_link_scope(scope)?;
        Ok(Self {
            kind: ShareKind::Link,
            item_id: item_id.to_string(),
            link_type: Some(link_type.to_string()),
            scope: Some(scope.to_string()),
            role: None,
            recipient_hashes: Vec::new(),
            recipient_count: 0,
            require_sign_in: false,
            send_invitation: false,
        })
    }

    pub fn invite(
        account: &str,
        item_id: &str,
        emails: &[String],
        role: &str,
        secret: &[u8],
    ) -> Result<(Self, Vec<String>), String> {
        validate_share_item_id(item_id)?;
        validate_invite_role(role)?;
        let normalized = normalize_invite_recipients(emails)?;
        let recipient_hashes = normalized
            .iter()
            .map(|email| recipient_hash(secret, account, email))
            .collect::<Vec<_>>();
        Ok((
            Self {
                kind: ShareKind::Invite,
                item_id: item_id.to_string(),
                link_type: None,
                scope: None,
                role: Some(role.to_string()),
                recipient_count: recipient_hashes.len(),
                recipient_hashes,
                require_sign_in: true,
                send_invitation: true,
            },
            normalized,
        ))
    }

    fn from_op(op: &CloudWriteOp) -> Result<Self, String> {
        let target = op
            .target_id
            .as_deref()
            .ok_or("share ledger row has no target item id")?;
        let intent: serde_json::Value = op
            .intent_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .ok_or("share ledger row has malformed intent_json")?;
        match intent.get("share_kind").and_then(|v| v.as_str()) {
            Some("link") => {
                let link_type = intent
                    .get("link_type")
                    .and_then(|v| v.as_str())
                    .ok_or("share link intent has no link_type")?;
                let scope = intent
                    .get("scope")
                    .and_then(|v| v.as_str())
                    .ok_or("share link intent has no scope")?;
                Self::link(target, link_type, scope)
            }
            Some("invite") => {
                validate_share_item_id(target)?;
                let role = intent
                    .get("role")
                    .and_then(|v| v.as_str())
                    .ok_or("invite intent has no role")?;
                validate_invite_role(role)?;
                let recipient_hashes = intent
                    .get("recipient_hashes")
                    .and_then(|v| v.as_array())
                    .ok_or("invite intent has no recipient_hashes")?
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(str::to_string)
                            .ok_or_else(|| "invite recipient hash is not a string".to_string())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let recipient_count = intent
                    .get("recipient_count")
                    .and_then(|v| v.as_u64())
                    .ok_or("invite intent has no recipient_count")?
                    as usize;
                if recipient_hashes.is_empty() || recipient_hashes.len() != recipient_count {
                    return Err("invite recipient hash count mismatch".into());
                }
                Ok(Self {
                    kind: ShareKind::Invite,
                    item_id: target.to_string(),
                    link_type: None,
                    scope: None,
                    role: Some(role.to_string()),
                    recipient_hashes,
                    recipient_count,
                    require_sign_in: intent
                        .get("require_sign_in")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true),
                    send_invitation: intent
                        .get("send_invitation")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true),
                })
            }
            Some(other) => Err(format!("unsupported share_kind '{other}'")),
            None => Err("share ledger row has no share_kind".into()),
        }
    }

    fn intent_json(&self) -> String {
        match self.kind {
            ShareKind::Link => serde_json::json!({
                "share_kind": "link",
                "link_type": self.link_type,
                "scope": self.scope,
            })
            .to_string(),
            ShareKind::Invite => serde_json::json!({
                "share_kind": "invite",
                "role": self.role,
                "recipient_hashes": self.recipient_hashes,
                "recipient_count": self.recipient_count,
                "require_sign_in": self.require_sign_in,
                "send_invitation": self.send_invitation,
            })
            .to_string(),
        }
    }

    fn keys(&self, secret: &[u8], account: &str) -> (String, String) {
        let source = match self.kind {
            ShareKind::Link => format!(
                "share|link|{}|{}|{}",
                self.item_id,
                self.link_type.as_deref().unwrap_or(""),
                self.scope.as_deref().unwrap_or("")
            ),
            ShareKind::Invite => format!(
                "share|invite|{}|{}|{}|{}|{}",
                self.item_id,
                self.role.as_deref().unwrap_or(""),
                self.require_sign_in,
                self.send_invitation,
                self.recipient_hashes.join(",")
            ),
        };
        let key = crate::restore_key::idempotency_key(secret, account, SERVICE, &source, &[]);
        (format!("{account}:{key}"), key)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareOutcome {
    Link {
        web_url: String,
    },
    Invite {
        permission_ids: Vec<String>,
        summary: String,
    },
    FailedClosed {
        reason: String,
    },
}

pub trait OneDriveShareSink {
    fn create_share_link(
        &self,
        item_id: &str,
        link_type: &str,
        scope: &str,
    ) -> Result<DrivePermission, String>;

    fn list_share_permissions(&self, item_id: &str) -> Result<Vec<DrivePermission>, String>;

    fn invite_share(
        &self,
        item_id: &str,
        emails: &[String],
        roles: &[&str],
        require_sign_in: bool,
        send_invitation: bool,
    ) -> Result<InviteOutcome, String>;
}

impl OneDriveShareSink for isyncyou_graph::GraphClient {
    fn create_share_link(
        &self,
        item_id: &str,
        link_type: &str,
        scope: &str,
    ) -> Result<DrivePermission, String> {
        self.create_link_detailed(item_id, link_type, scope, None, None, None)
            .map_err(|e| e.to_string())
    }

    fn list_share_permissions(&self, item_id: &str) -> Result<Vec<DrivePermission>, String> {
        self.list_permissions_detailed(item_id)
            .map_err(|e| e.to_string())
    }

    fn invite_share(
        &self,
        item_id: &str,
        emails: &[String],
        roles: &[&str],
        require_sign_in: bool,
        send_invitation: bool,
    ) -> Result<InviteOutcome, String> {
        self.invite_detailed(
            item_id,
            emails,
            roles,
            require_sign_in,
            send_invitation,
            "",
            None,
            None,
        )
        .map_err(|e| e.to_string())
    }
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

fn validate_share_item_id(item_id: &str) -> Result<(), String> {
    if item_id.is_empty() {
        return Err("share item id is empty".into());
    }
    if item_id.len() > 512 {
        return Err("share item id is too long".into());
    }
    Ok(())
}

fn validate_share_link_type(link_type: &str) -> Result<(), String> {
    match link_type {
        "view" | "edit" | "embed" => Ok(()),
        _ => Err("invalid share link type".into()),
    }
}

fn validate_share_link_scope(scope: &str) -> Result<(), String> {
    match scope {
        "anonymous" | "organization" | "users" => Ok(()),
        _ => Err("invalid share link scope".into()),
    }
}

fn validate_invite_role(role: &str) -> Result<(), String> {
    match role {
        "read" | "write" => Ok(()),
        _ => Err("invalid invite role".into()),
    }
}

fn normalize_invite_recipients(emails: &[String]) -> Result<Vec<String>, String> {
    if emails.is_empty() {
        return Err("invite requires at least one recipient".into());
    }
    if emails.len() > 20 {
        return Err("invite recipient count exceeds limit".into());
    }
    let mut normalized = Vec::new();
    for email in emails {
        let trimmed = email.trim();
        if trimmed.is_empty() {
            return Err("invite recipient is empty".into());
        }
        if trimmed.len() > 320 {
            return Err("invite recipient is too long".into());
        }
        if trimmed.chars().any(char::is_control) || !trimmed.contains('@') {
            return Err("invite recipient is malformed".into());
        }
        let lowered = trimmed.to_ascii_lowercase();
        if !normalized.contains(&lowered) {
            normalized.push(lowered);
        }
    }
    normalized.sort();
    Ok(normalized)
}

fn recipient_hash(secret: &[u8], account: &str, normalized_email: &str) -> String {
    crate::restore_key::idempotency_key(
        secret,
        account,
        SERVICE,
        &format!("share|recipient|{normalized_email}"),
        &[],
    )
}

fn permission_email_hashes(
    permission: &DrivePermission,
    secret: &[u8],
    account: &str,
) -> Vec<String> {
    permission
        .granted_emails
        .iter()
        .chain(permission.invitation_emails.iter())
        .map(|email| recipient_hash(secret, account, &email.trim().to_ascii_lowercase()))
        .collect()
}

fn invite_role_matches(permission: &DrivePermission, intent: &ShareIntent) -> bool {
    let Some(role) = intent.role.as_deref() else {
        return false;
    };
    permission.roles.iter().any(|r| r == role)
}

fn invite_recovered_permission_ids(
    permissions: &[DrivePermission],
    intent: &ShareIntent,
    secret: &[u8],
    account: &str,
) -> Option<Vec<String>> {
    let mut matches: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        intent
            .recipient_hashes
            .iter()
            .map(|hash| (hash.clone(), std::collections::BTreeSet::new()))
            .collect();
    for permission in permissions {
        if permission.inherited || !invite_role_matches(permission, intent) {
            continue;
        }
        let hashes = permission_email_hashes(permission, secret, account);
        if hashes.is_empty() {
            continue;
        }
        for expected in &intent.recipient_hashes {
            if hashes.iter().any(|hash| hash == expected) {
                matches
                    .get_mut(expected)
                    .expect("expected hash was pre-seeded")
                    .insert(permission.id.clone());
            }
        }
    }
    let mut permission_ids = Vec::new();
    for ids in matches.values() {
        if ids.len() != 1 {
            return None;
        }
        let id = ids.iter().next().expect("single id").clone();
        if !permission_ids.contains(&id) {
            permission_ids.push(id);
        }
    }
    Some(permission_ids)
}

fn share_permission_web_url(permission: &DrivePermission) -> Result<String, String> {
    permission
        .link_web_url
        .clone()
        .ok_or_else(|| "share link permission had no webUrl".to_string())
}

fn exact_link_matches<'a>(
    permissions: &'a [DrivePermission],
    intent: &ShareIntent,
) -> Vec<&'a DrivePermission> {
    let link_type = intent.link_type.as_deref();
    let scope = intent.scope.as_deref();
    permissions
        .iter()
        .filter(|permission| {
            !permission.inherited
                && permission.link_web_url.is_some()
                && permission.link_type.as_deref() == link_type
                && permission.link_scope.as_deref() == scope
        })
        .collect()
}

fn share_policy_final_error(intent: &ShareIntent, error: &str) -> bool {
    if intent.link_type.as_deref() != Some("embed") {
        return false;
    }
    let lower = error.to_ascii_lowercase();
    lower.contains("unsupported")
        || lower.contains("not supported")
        || lower.contains("not available")
        || lower.contains("onedrive personal")
        || lower.contains("http 400")
        || lower.contains("http 403")
}

fn redacted_share_error(error: &str) -> String {
    let lower = error.to_ascii_lowercase();
    if lower.contains("http://")
        || lower.contains("https://")
        || lower.contains("1drv.ms")
        || error.contains('@')
    {
        return "share_error".to_string();
    }
    error.chars().take(200).collect()
}

fn set_share_error_state(
    store: &Store,
    op_id: &str,
    intent: &ShareIntent,
    error: &str,
    now: i64,
) -> Result<(), String> {
    if share_policy_final_error(intent, error) {
        store
            .set_cloud_write_state(op_id, "failed", None, Some("share_policy_unsupported"), now)
            .map_err(|e| e.to_string())
    } else {
        let redacted = redacted_share_error(error);
        store
            .set_cloud_write_state(op_id, "inflight", None, Some(&redacted), now)
            .map_err(|e| e.to_string())
    }
}

fn apply_share_link_permission(
    store: &Store,
    op_id: &str,
    permission: &DrivePermission,
    now: i64,
) -> Result<ShareOutcome, String> {
    let web_url = share_permission_web_url(permission)?;
    store
        .set_cloud_write_state(op_id, "applied", non_empty(&permission.id), None, now)
        .map_err(|e| e.to_string())?;
    Ok(ShareOutcome::Link { web_url })
}

fn create_and_apply_share_link<S: OneDriveShareSink>(
    store: &Store,
    op_id: &str,
    intent: &ShareIntent,
    sink: &S,
    now: i64,
) -> Result<ShareOutcome, String> {
    let item_id = &intent.item_id;
    let link_type = intent
        .link_type
        .as_deref()
        .ok_or("share link intent missing link_type")?;
    let scope = intent
        .scope
        .as_deref()
        .ok_or("share link intent missing scope")?;
    match sink.create_share_link(item_id, link_type, scope) {
        Ok(permission) => match apply_share_link_permission(store, op_id, &permission, now) {
            Ok(outcome) => Ok(outcome),
            Err(e) => {
                set_share_error_state(store, op_id, intent, &e, now)?;
                Err(e)
            }
        },
        Err(e) => {
            set_share_error_state(store, op_id, intent, &e, now)?;
            Err(e)
        }
    }
}

fn apply_invite_success(
    store: &Store,
    op_id: &str,
    permission_ids: Vec<String>,
    recipient_count: usize,
    now: i64,
) -> Result<ShareOutcome, String> {
    let result_id = if permission_ids.len() == 1 {
        permission_ids.first().map(String::as_str)
    } else {
        None
    };
    store
        .set_cloud_write_state(op_id, "applied", result_id, None, now)
        .map_err(|e| e.to_string())?;
    Ok(ShareOutcome::Invite {
        permission_ids,
        summary: format!("invited {recipient_count} recipient(s)"),
    })
}

fn set_invite_failed(
    store: &Store,
    op_id: &str,
    reason: &str,
    now: i64,
) -> Result<ShareOutcome, String> {
    store
        .set_cloud_write_state(op_id, "failed", None, Some(reason), now)
        .map_err(|e| e.to_string())?;
    Ok(ShareOutcome::FailedClosed {
        reason: reason.to_string(),
    })
}

fn issue_invite<S: OneDriveShareSink>(
    store: &Store,
    op_id: &str,
    intent: &ShareIntent,
    emails: &[String],
    sink: &S,
    now: i64,
) -> Result<ShareOutcome, String> {
    let role = intent.role.as_deref().ok_or("invite intent has no role")?;
    match sink.invite_share(
        &intent.item_id,
        emails,
        &[role],
        intent.require_sign_in,
        intent.send_invitation,
    ) {
        Ok(InviteOutcome::Applied { permission_ids }) => {
            apply_invite_success(store, op_id, permission_ids, intent.recipient_count, now)
        }
        Ok(InviteOutcome::Partial { .. }) => {
            set_invite_failed(store, op_id, INVITE_PARTIAL_SUCCESS, now)?;
            Err(INVITE_PARTIAL_SUCCESS.to_string())
        }
        Err(e) => {
            let redacted = redacted_share_error(&e);
            store
                .set_cloud_write_state(op_id, "inflight", None, Some(&redacted), now)
                .map_err(|store_err| store_err.to_string())?;
            Err(e)
        }
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
        CloudOpKind::Delete => sink
            .delete(&w.target_id)
            .map(|_| Issued::Done(String::new())),
        CloudOpKind::Upload => {
            let bytes = read_local_body(&w.local_path)?;
            sink.upload(&w.target_id, &w.name, &bytes).map(Issued::Done)
        }
        CloudOpKind::Replace => {
            let bytes = read_local_body(&w.local_path)?;
            match sink.replace_if_match(
                &w.target_id,
                &bytes,
                w.if_match.as_deref().unwrap_or(""),
            )? {
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
pub fn run_cloud_write<S: OneDriveWriteSink + OneDriveShareSink>(
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
        return recover_cloud_write_op(store, &existing, sink, secret, now);
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

#[allow(clippy::too_many_arguments)]
pub fn run_share_link<S: OneDriveShareSink>(
    store: &Store,
    account: &str,
    item_id: &str,
    link_type: &str,
    scope: &str,
    sink: &S,
    secret: &[u8],
    now: i64,
) -> Result<ShareOutcome, String> {
    let intent = ShareIntent::link(item_id, link_type, scope)?;
    let (op_id, key) = intent.keys(secret, account);
    let op = CloudWriteOp {
        op_id: op_id.clone(),
        account_id: account.to_string(),
        service: SERVICE.to_string(),
        op_kind: CloudOpKind::Share.as_str().to_string(),
        target_id: Some(item_id.to_string()),
        idempotency_key: key.clone(),
        if_match_etag: None,
        state: "pending".to_string(),
        result_id: None,
        intent_json: Some(intent.intent_json()),
        attempts: 0,
        last_error: None,
    };
    if !store
        .record_cloud_write(&op, now)
        .map_err(|e| e.to_string())?
    {
        let existing = store
            .cloud_write_by_key(account, &key)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("share ledger row for key {key} vanished"))?;
        return recover_share_write_op(store, &existing, sink, secret, now);
    }
    store
        .set_cloud_write_state(&op_id, "inflight", None, None, now)
        .map_err(|e| e.to_string())?;
    create_and_apply_share_link(store, &op_id, &intent, sink, now)
}

#[allow(clippy::too_many_arguments)]
pub fn run_invite<S: OneDriveShareSink>(
    store: &Store,
    account: &str,
    item_id: &str,
    emails: &[String],
    role: &str,
    sink: &S,
    secret: &[u8],
    now: i64,
) -> Result<ShareOutcome, String> {
    let (intent, normalized_emails) = ShareIntent::invite(account, item_id, emails, role, secret)?;
    let (op_id, key) = intent.keys(secret, account);
    let op = CloudWriteOp {
        op_id: op_id.clone(),
        account_id: account.to_string(),
        service: SERVICE.to_string(),
        op_kind: CloudOpKind::Share.as_str().to_string(),
        target_id: Some(item_id.to_string()),
        idempotency_key: key.clone(),
        if_match_etag: None,
        state: "pending".to_string(),
        result_id: None,
        intent_json: Some(intent.intent_json()),
        attempts: 0,
        last_error: None,
    };
    if !store
        .record_cloud_write(&op, now)
        .map_err(|e| e.to_string())?
    {
        let existing = store
            .cloud_write_by_key(account, &key)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("invite ledger row for key {key} vanished"))?;
        if existing.state == "pending" || existing.last_error.as_deref() == Some(INVITE_NOT_STARTED)
        {
            store
                .set_cloud_write_state(&existing.op_id, "inflight", None, None, now)
                .map_err(|e| e.to_string())?;
            return issue_invite(
                store,
                &existing.op_id,
                &intent,
                &normalized_emails,
                sink,
                now,
            );
        }
        if existing.state == "failed" {
            return Err(existing
                .last_error
                .clone()
                .unwrap_or_else(|| INVITE_AMBIGUOUS.to_string()));
        }
        return recover_share_write_op(store, &existing, sink, secret, now);
    }
    store
        .set_cloud_write_state(&op_id, "inflight", None, None, now)
        .map_err(|e| e.to_string())?;
    issue_invite(store, &op_id, &intent, &normalized_emails, sink, now)
}

pub fn recover_share_write_op<S: OneDriveShareSink>(
    store: &Store,
    op: &CloudWriteOp,
    sink: &S,
    secret: &[u8],
    now: i64,
) -> Result<ShareOutcome, String> {
    let intent = match ShareIntent::from_op(op) {
        Ok(intent) => intent,
        Err(e) => {
            let redacted = redacted_share_error(&e);
            store
                .set_cloud_write_state(&op.op_id, "failed", None, Some(&redacted), now)
                .map_err(|store_err| store_err.to_string())?;
            return Ok(ShareOutcome::FailedClosed { reason: redacted });
        }
    };
    match intent.kind {
        ShareKind::Link => recover_share_link_write_op(store, op, sink, &intent, now),
        ShareKind::Invite => {
            recover_invite_write_op(store, account_from_op(op), op, sink, secret, &intent, now)
        }
    }
}

fn recover_share_link_write_op<S: OneDriveShareSink>(
    store: &Store,
    op: &CloudWriteOp,
    sink: &S,
    intent: &ShareIntent,
    now: i64,
) -> Result<ShareOutcome, String> {
    if op.state == "applied" {
        let permissions = sink.list_share_permissions(&intent.item_id)?;
        let matches = exact_link_matches(&permissions, intent);
        return match matches.as_slice() {
            [permission] => Ok(ShareOutcome::Link {
                web_url: share_permission_web_url(permission)?,
            }),
            [] => Err("applied share link permission was not found".into()),
            _ => Err("applied share link permission is ambiguous".into()),
        };
    }
    if op.state == "failed" {
        return Ok(ShareOutcome::FailedClosed {
            reason: op
                .last_error
                .clone()
                .unwrap_or_else(|| "share_failed_closed".to_string()),
        });
    }

    let permissions = sink.list_share_permissions(&intent.item_id)?;
    let matches = exact_link_matches(&permissions, intent);
    match matches.as_slice() {
        [permission] => apply_share_link_permission(store, &op.op_id, permission, now),
        _ => create_and_apply_share_link(store, &op.op_id, intent, sink, now),
    }
}

fn account_from_op(op: &CloudWriteOp) -> &str {
    op.account_id.as_str()
}

fn recover_invite_write_op<S: OneDriveShareSink>(
    store: &Store,
    account: &str,
    op: &CloudWriteOp,
    sink: &S,
    secret: &[u8],
    intent: &ShareIntent,
    now: i64,
) -> Result<ShareOutcome, String> {
    if op.state == "applied" {
        return Ok(ShareOutcome::Invite {
            permission_ids: op.result_id.iter().cloned().collect(),
            summary: "invite already applied".into(),
        });
    }
    if op.state == "failed" {
        return Ok(ShareOutcome::FailedClosed {
            reason: op
                .last_error
                .clone()
                .unwrap_or_else(|| INVITE_AMBIGUOUS.to_string()),
        });
    }
    if op.state == "pending" {
        return set_invite_failed(store, &op.op_id, INVITE_NOT_STARTED, now);
    }

    let permissions = sink.list_share_permissions(&intent.item_id)?;
    if let Some(permission_ids) =
        invite_recovered_permission_ids(&permissions, intent, secret, account)
    {
        return apply_invite_success(
            store,
            &op.op_id,
            permission_ids,
            intent.recipient_count,
            now,
        );
    }
    set_invite_failed(store, &op.op_id, INVITE_AMBIGUOUS, now)
}

/// Reconcile a non-`applied` ledger op to a terminal `applied` state **without a double
/// effect**, per its [`CloudOpKind`] recovery rule. Called both by boot recovery and by the
/// re-issue path of [`run_cloud_write`]. Returns the op's cloud id (`Create`) or `""`.
pub fn recover_cloud_write_op<S: OneDriveWriteSink + OneDriveShareSink>(
    store: &Store,
    op: &CloudWriteOp,
    sink: &S,
    secret: &[u8],
    now: i64,
) -> Result<WriteOutcome, String> {
    if op.state == "applied" {
        return Ok(WriteOutcome::Applied(
            op.result_id.clone().unwrap_or_default(),
        ));
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
            match sink.replace_if_match(
                &target,
                &bytes,
                op.if_match_etag.as_deref().unwrap_or(""),
            )? {
                ReplaceOutcome::Replaced(id) => WriteOutcome::Applied(id),
                ReplaceOutcome::Conflict => WriteOutcome::Conflict,
            }
        }
        CloudOpKind::Share => {
            recover_share_write_op(store, op, sink, secret, now)?;
            return Ok(WriteOutcome::Applied(String::new()));
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
pub fn recover_pending_cloud_writes<S: OneDriveWriteSink + OneDriveShareSink>(
    store: &Store,
    account: &str,
    sink: &S,
    secret: &[u8],
    now: i64,
) -> Result<usize, String> {
    let pending = store
        .pending_cloud_writes(account)
        .map_err(|e| e.to_string())?;
    let mut recovered = 0usize;
    for op in &pending {
        // One un-reconcilable op must never abort the whole batch: boot recovery runs *before*
        // the offline materialize, so a single `?` here would block every download behind it
        // (observed: a stale Upload whose local body was deleted poisoned every offline pass).
        // An Upload/Replace whose local source is gone can never succeed → mark it terminally
        // `failed` so it leaves the pending set; any other (e.g. transient network) error stays
        // pending and is retried on the next pass.
        match recover_cloud_write_op(store, op, sink, secret, now) {
            Ok(_) => recovered += 1,
            Err(e) => {
                let public_error = if CloudOpKind::parse(&op.op_kind) == Some(CloudOpKind::Share) {
                    redacted_share_error(&e)
                } else {
                    e.clone()
                };
                eprintln!(
                    "isyncyou: cloud-write recovery for {} skipped: {public_error}",
                    op.op_id,
                );
                if cloud_write_body_source_missing(op) {
                    let _ = store.set_cloud_write_state(&op.op_id, "failed", None, Some(&e), now);
                }
            }
        }
    }
    Ok(recovered)
}

/// True if `op` is a body op (Upload/Replace) whose local source file is gone, so it can never
/// be re-read and re-sent — a terminal failure rather than something to keep retrying (#656 F-A).
fn cloud_write_body_source_missing(op: &CloudWriteOp) -> bool {
    if !matches!(
        CloudOpKind::parse(&op.op_kind),
        Some(CloudOpKind::Upload) | Some(CloudOpKind::Replace)
    ) {
        return false;
    }
    match op
        .intent_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| {
            v.get("local_path")
                .and_then(|p| p.as_str())
                .map(PathBuf::from)
        }) {
        // a body op with no local_path can never re-read its body → treat as gone
        None => true,
        // the local source file was deleted
        Some(p) => !p.exists(),
    }
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
/// reconcile every pending OneDrive cloud-write (metadata/body ops plus #722 share/invite rows;
/// the mobile offline pass runs its own recovery inline). Returns the count reconciled.
pub fn recover_pending_cloud_writes_for(cfg: &Config, account: &str) -> Result<usize, String> {
    with_ledger(cfg, account, |store, sink, _secret| {
        recover_pending_cloud_writes(store, account, sink, _secret, now_secs())
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

/// Create or return a OneDrive sharing link through the crash-safe cloud-write ledger.
pub fn share_link_via_ledger(
    cfg: &Config,
    account: &str,
    item_id: &str,
    link_type: &str,
    scope: &str,
) -> Result<String, String> {
    with_ledger(cfg, account, |store, sink, secret| {
        match run_share_link(
            store,
            account,
            item_id,
            link_type,
            scope,
            sink,
            secret,
            now_secs(),
        )? {
            ShareOutcome::Link { web_url } => Ok(web_url),
            ShareOutcome::FailedClosed { reason } => Err(reason),
            ShareOutcome::Invite { .. } => Err("unexpected invite outcome for share link".into()),
        }
    })
}

pub fn invite_via_ledger(
    cfg: &Config,
    account: &str,
    item_id: &str,
    emails: &[String],
    role: &str,
) -> Result<String, String> {
    with_ledger(cfg, account, |store, sink, secret| {
        match run_invite(
            store,
            account,
            item_id,
            emails,
            role,
            sink,
            secret,
            now_secs(),
        )? {
            ShareOutcome::Invite { summary, .. } => Ok(summary),
            ShareOutcome::FailedClosed { reason } => Err(reason),
            ShareOutcome::Link { .. } => Err("unexpected link outcome for invite".into()),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::{Mutex, OnceLock};

    /// A fake cloud file: `(id, parent, name, bytes)`.
    type FakeFile = (String, String, String, Vec<u8>);

    /// A non-idempotent fake OneDrive: `create_folder`/`upload` always append (modelling the
    /// real duplication danger). Safety must come from the recovery logic, not this sink.
    #[derive(Default)]
    struct FakeCloud {
        folders: RefCell<Vec<(String, String, String)>>, // (id, parent, name)
        files: RefCell<Vec<FakeFile>>,
        etags: RefCell<std::collections::HashMap<String, String>>, // item_id -> current etag
        seq: RefCell<u32>,
        create_calls: RefCell<u32>,
        delete_calls: RefCell<u32>,
        move_calls: RefCell<u32>,
        upload_calls: RefCell<u32>,
        replace_calls: RefCell<u32>,
        create_link_calls: RefCell<u32>,
        invite_calls: RefCell<u32>,
        replace_attempts: RefCell<Vec<(String, String, Vec<u8>)>>, // (id, etag, bytes)
        permissions: RefCell<Vec<DrivePermission>>,
        create_link_errors: RefCell<Vec<String>>,
        invite_outcomes: RefCell<Vec<Result<InviteOutcome, String>>>,
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
        fn body_of(&self, id: &str) -> Option<Vec<u8>> {
            self.files
                .borrow()
                .iter()
                .find(|(file_id, _, _, _)| file_id == id)
                .map(|(_, _, _, bytes)| bytes.clone())
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
                return Err(format!(
                    "upload conflict: '{name}' exists under {parent_id}"
                ));
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
            self.replace_attempts.borrow_mut().push((
                item_id.to_string(),
                etag.to_string(),
                bytes.to_vec(),
            ));
            if self.etag_of(item_id) != etag {
                return Ok(ReplaceOutcome::Conflict); // 412: the cloud moved on
            }
            // Matched → replace the bytes and advance the etag.
            let new_etag = {
                let mut seq = self.seq.borrow_mut();
                *seq += 1;
                format!("etag-{item_id}-{seq}")
            };
            self.etags
                .borrow_mut()
                .insert(item_id.to_string(), new_etag);
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

    impl OneDriveShareSink for FakeCloud {
        fn create_share_link(
            &self,
            item_id: &str,
            link_type: &str,
            scope: &str,
        ) -> Result<DrivePermission, String> {
            *self.create_link_calls.borrow_mut() += 1;
            if let Some(error) = self.create_link_errors.borrow_mut().pop() {
                return Err(error);
            }
            let id = {
                let mut seq = self.seq.borrow_mut();
                *seq += 1;
                format!("perm-{seq}")
            };
            let permission = DrivePermission {
                id: id.clone(),
                roles: vec![if link_type == "edit" { "write" } else { "read" }.to_string()],
                link_web_url: Some(format!(
                    "https://1drv.ms/{item_id}/{link_type}/{scope}/{id}"
                )),
                link_type: Some(link_type.to_string()),
                link_scope: Some(scope.to_string()),
                granted_emails: Vec::new(),
                invitation_emails: Vec::new(),
                inherited: false,
            };
            self.permissions.borrow_mut().push(permission.clone());
            Ok(permission)
        }

        fn list_share_permissions(&self, _item_id: &str) -> Result<Vec<DrivePermission>, String> {
            Ok(self.permissions.borrow().clone())
        }

        fn invite_share(
            &self,
            _item_id: &str,
            emails: &[String],
            roles: &[&str],
            _require_sign_in: bool,
            _send_invitation: bool,
        ) -> Result<InviteOutcome, String> {
            *self.invite_calls.borrow_mut() += 1;
            {
                let mut outcomes = self.invite_outcomes.borrow_mut();
                if !outcomes.is_empty() {
                    return outcomes.remove(0);
                }
            }
            let roles = roles
                .iter()
                .map(|role| (*role).to_string())
                .collect::<Vec<_>>();
            let mut permission_ids = Vec::new();
            for email in emails {
                let id = {
                    let mut seq = self.seq.borrow_mut();
                    *seq += 1;
                    format!("invite-perm-{seq}")
                };
                self.permissions.borrow_mut().push(DrivePermission {
                    id: id.clone(),
                    roles: roles.clone(),
                    link_web_url: None,
                    link_type: None,
                    link_scope: None,
                    granted_emails: Vec::new(),
                    invitation_emails: vec![email.clone()],
                    inherited: false,
                });
                permission_ids.push(id);
            }
            Ok(InviteOutcome::Applied { permission_ids })
        }
    }

    const SECRET: &[u8] = b"test-secret";
    const ACCT: &str = "me";

    fn body_key_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static BODY_KEY_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        BODY_KEY_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// A temp file holding `content`, for the Upload/Replace body-source. Returned so the
    /// caller keeps it alive (drop = delete).
    fn body_file(content: &[u8]) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), content).unwrap();
        f
    }

    /// A temp body source sealed with the process body-envelope key. The caller must hold
    /// `body_key_test_guard()` while using this helper because the body-key registry is global.
    fn sealed_body_file(key_id: u32, key: [u8; 32], content: &[u8]) -> tempfile::NamedTempFile {
        assert!(!content.is_empty(), "sentinel content must not be empty");
        isyncyou_core::envelope::set_body_key(key_id, key);
        let f = tempfile::NamedTempFile::new().unwrap();
        isyncyou_core::envelope::write_body_atomic(f.path(), content).unwrap();
        let raw = std::fs::read(f.path()).unwrap();
        assert_eq!(
            isyncyou_core::envelope::blob_key_id(&raw),
            Some(key_id),
            "sealed body source must carry the expected key id"
        );
        assert!(
            !raw.windows(content.len()).any(|w| w == content),
            "sealed body source must not contain plaintext bytes"
        );
        assert_eq!(
            isyncyou_core::envelope::read_body(f.path()).unwrap(),
            content
        );
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

    fn link_permission(id: &str, link_type: &str, scope: &str, url: &str) -> DrivePermission {
        DrivePermission {
            id: id.to_string(),
            roles: vec![if link_type == "edit" { "write" } else { "read" }.to_string()],
            link_web_url: Some(url.to_string()),
            link_type: Some(link_type.to_string()),
            link_scope: Some(scope.to_string()),
            granted_emails: Vec::new(),
            invitation_emails: Vec::new(),
            inherited: false,
        }
    }

    fn inherited_link_permission(
        id: &str,
        link_type: &str,
        scope: &str,
        url: &str,
    ) -> DrivePermission {
        let mut permission = link_permission(id, link_type, scope, url);
        permission.inherited = true;
        permission
    }

    fn share_link_op(item_id: &str, link_type: &str, scope: &str, state: &str) -> CloudWriteOp {
        let intent = ShareIntent::link(item_id, link_type, scope).unwrap();
        let (op_id, key) = intent.keys(SECRET, ACCT);
        CloudWriteOp {
            op_id,
            account_id: ACCT.into(),
            service: SERVICE.into(),
            op_kind: CloudOpKind::Share.as_str().into(),
            target_id: Some(item_id.into()),
            idempotency_key: key,
            if_match_etag: None,
            state: state.into(),
            result_id: None,
            intent_json: Some(intent.intent_json()),
            attempts: 1,
            last_error: None,
        }
    }

    fn invite_emails() -> Vec<String> {
        vec![
            "Alpha@example.com".to_string(),
            "beta@example.com".to_string(),
        ]
    }

    fn invite_permission(id: &str, email: &str, role: &str) -> DrivePermission {
        DrivePermission {
            id: id.to_string(),
            roles: vec![role.to_string()],
            link_web_url: None,
            link_type: None,
            link_scope: None,
            granted_emails: vec![email.to_string()],
            invitation_emails: Vec::new(),
            inherited: false,
        }
    }

    fn invite_op(item_id: &str, emails: &[String], role: &str, state: &str) -> CloudWriteOp {
        let (intent, _) = ShareIntent::invite(ACCT, item_id, emails, role, SECRET).unwrap();
        let (op_id, key) = intent.keys(SECRET, ACCT);
        CloudWriteOp {
            op_id,
            account_id: ACCT.into(),
            service: SERVICE.into(),
            op_kind: CloudOpKind::Share.as_str().into(),
            target_id: Some(item_id.into()),
            idempotency_key: key,
            if_match_etag: None,
            state: state.into(),
            result_id: None,
            intent_json: Some(intent.intent_json()),
            attempts: 1,
            last_error: None,
        }
    }

    struct LedgerAssertingShareSink<'a> {
        store: &'a Store,
        account: &'a str,
        key: String,
        cloud: FakeCloud,
    }

    impl OneDriveShareSink for LedgerAssertingShareSink<'_> {
        fn create_share_link(
            &self,
            item_id: &str,
            link_type: &str,
            scope: &str,
        ) -> Result<DrivePermission, String> {
            let row = self
                .store
                .cloud_write_by_key(self.account, &self.key)
                .unwrap()
                .expect("share intent must exist before Graph createLink");
            assert_eq!(row.op_kind, "share");
            assert_eq!(row.state, "inflight");
            assert_eq!(row.target_id.as_deref(), Some(item_id));
            self.cloud.create_share_link(item_id, link_type, scope)
        }

        fn list_share_permissions(&self, item_id: &str) -> Result<Vec<DrivePermission>, String> {
            self.cloud.list_share_permissions(item_id)
        }

        fn invite_share(
            &self,
            item_id: &str,
            emails: &[String],
            roles: &[&str],
            require_sign_in: bool,
            send_invitation: bool,
        ) -> Result<InviteOutcome, String> {
            self.cloud
                .invite_share(item_id, emails, roles, require_sign_in, send_invitation)
        }
    }

    struct LedgerAssertingInviteSink<'a> {
        store: &'a Store,
        account: &'a str,
        key: String,
        raw_emails: Vec<String>,
        cloud: FakeCloud,
    }

    impl OneDriveShareSink for LedgerAssertingInviteSink<'_> {
        fn create_share_link(
            &self,
            item_id: &str,
            link_type: &str,
            scope: &str,
        ) -> Result<DrivePermission, String> {
            self.cloud.create_share_link(item_id, link_type, scope)
        }

        fn list_share_permissions(&self, item_id: &str) -> Result<Vec<DrivePermission>, String> {
            self.cloud.list_share_permissions(item_id)
        }

        fn invite_share(
            &self,
            item_id: &str,
            emails: &[String],
            roles: &[&str],
            require_sign_in: bool,
            send_invitation: bool,
        ) -> Result<InviteOutcome, String> {
            let row = self
                .store
                .cloud_write_by_key(self.account, &self.key)
                .unwrap()
                .expect("invite intent must exist before Graph invite");
            assert_eq!(row.op_kind, "share");
            assert_eq!(row.state, "inflight");
            assert_eq!(row.target_id.as_deref(), Some(item_id));
            let intent_json = row.intent_json.as_deref().expect("invite intent_json");
            assert!(intent_json.contains(r#""share_kind":"invite""#));
            assert!(intent_json.contains(r#""recipient_count":2"#));
            for raw in &self.raw_emails {
                assert!(
                    !intent_json
                        .to_ascii_lowercase()
                        .contains(&raw.to_ascii_lowercase()),
                    "raw invite email leaked into intent_json"
                );
            }
            assert!(!intent_json.contains('@'));
            self.cloud
                .invite_share(item_id, emails, roles, require_sign_in, send_invitation)
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
        let n = recover_pending_cloud_writes(&s, ACCT, &cloud, SECRET, 20).unwrap();
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
        recover_pending_cloud_writes(&s, ACCT, &cloud, SECRET, 20).unwrap();
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

    #[test]
    fn share_link_records_intent_before_graph_mutation() {
        let s = Store::open_in_memory().unwrap();
        let intent = ShareIntent::link("item-1", "view", "anonymous").unwrap();
        let (_op_id, key) = intent.keys(SECRET, ACCT);
        let sink = LedgerAssertingShareSink {
            store: &s,
            account: ACCT,
            key: key.clone(),
            cloud: FakeCloud::default(),
        };

        let out =
            run_share_link(&s, ACCT, "item-1", "view", "anonymous", &sink, SECRET, 10).unwrap();

        assert!(matches!(out, ShareOutcome::Link { .. }));
        assert_eq!(*sink.cloud.create_link_calls.borrow(), 1);
        let row = s.cloud_write_by_key(ACCT, &key).unwrap().unwrap();
        assert_eq!(row.state, "applied");
        assert_eq!(row.op_kind, "share");
        assert_eq!(row.result_id.as_deref(), Some("perm-1"));
        let intent_json = row.intent_json.unwrap();
        assert!(intent_json.contains(r#""share_kind":"link""#));
        assert!(!intent_json.contains("http"));
        assert!(!intent_json.contains("1drv.ms"));
    }

    #[test]
    fn share_link_rejects_invalid_inputs_before_ledger() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        for (item_id, link_type, scope) in [
            ("", "view", "anonymous"),
            ("item-1", "owner", "anonymous"),
            ("item-1", "view", "tenant-wide"),
        ] {
            let err = run_share_link(&s, ACCT, item_id, link_type, scope, &cloud, SECRET, 10)
                .expect_err("invalid share link input must fail");
            assert!(!err.is_empty());
        }
        assert!(
            s.cloud_write_ledger_summary(ACCT).unwrap().is_empty(),
            "invalid share link input must not create ledger rows"
        );
        assert_eq!(*cloud.create_link_calls.borrow(), 0);
    }

    #[test]
    fn share_link_crash_after_graph_success_recovers_without_duplicate() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        cloud.permissions.borrow_mut().push(link_permission(
            "perm-landed",
            "view",
            "anonymous",
            "https://1drv.ms/landed",
        ));
        let op = share_link_op("item-1", "view", "anonymous", "inflight");
        let key = op.idempotency_key.clone();
        s.record_cloud_write(&op, 10).unwrap();

        let recovered = recover_pending_cloud_writes(&s, ACCT, &cloud, SECRET, 20).unwrap();

        assert_eq!(recovered, 1);
        assert_eq!(
            *cloud.create_link_calls.borrow(),
            0,
            "recovery must adopt landed link, not call createLink"
        );
        let row = s.cloud_write_by_key(ACCT, &key).unwrap().unwrap();
        assert_eq!(row.state, "applied");
        assert_eq!(row.result_id.as_deref(), Some("perm-landed"));
    }

    #[test]
    fn share_link_recovery_creates_once_when_no_matching_link_exists() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        let op = share_link_op("item-1", "view", "anonymous", "inflight");
        let key = op.idempotency_key.clone();
        s.record_cloud_write(&op, 10).unwrap();

        recover_pending_cloud_writes(&s, ACCT, &cloud, SECRET, 20).unwrap();

        assert_eq!(*cloud.create_link_calls.borrow(), 1);
        assert_eq!(cloud.permissions.borrow().len(), 1);
        let row = s.cloud_write_by_key(ACCT, &key).unwrap().unwrap();
        assert_eq!(row.state, "applied");
        assert_eq!(row.result_id.as_deref(), Some("perm-1"));
    }

    #[test]
    fn share_link_recovery_create_link_when_permission_match_is_ambiguous() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        cloud.permissions.borrow_mut().extend([
            link_permission("perm-a", "view", "anonymous", "https://1drv.ms/a"),
            link_permission("perm-b", "view", "anonymous", "https://1drv.ms/b"),
            inherited_link_permission("perm-inh", "view", "anonymous", "https://1drv.ms/inh"),
        ]);
        let op = share_link_op("item-1", "view", "anonymous", "inflight");
        let key = op.idempotency_key.clone();
        s.record_cloud_write(&op, 10).unwrap();

        recover_pending_cloud_writes(&s, ACCT, &cloud, SECRET, 20).unwrap();

        assert_eq!(*cloud.create_link_calls.borrow(), 1);
        let row = s.cloud_write_by_key(ACCT, &key).unwrap().unwrap();
        assert_eq!(row.state, "applied");
        assert_eq!(row.result_id.as_deref(), Some("perm-1"));
    }

    #[test]
    fn share_link_retry_uses_existing_ledger_key() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();

        let first =
            run_share_link(&s, ACCT, "item-1", "view", "anonymous", &cloud, SECRET, 10).unwrap();
        let second =
            run_share_link(&s, ACCT, "item-1", "view", "anonymous", &cloud, SECRET, 20).unwrap();

        assert_eq!(first, second);
        assert_eq!(*cloud.create_link_calls.borrow(), 1);
        assert_eq!(cloud.permissions.borrow().len(), 1);
    }

    #[test]
    fn share_link_create_link_transient_error_remains_recoverable() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        cloud
            .create_link_errors
            .borrow_mut()
            .push("transient network failure".into());

        let err = run_share_link(&s, ACCT, "item-1", "view", "anonymous", &cloud, SECRET, 10)
            .unwrap_err();
        assert_eq!(err, "transient network failure");
        let key = ShareIntent::link("item-1", "view", "anonymous")
            .unwrap()
            .keys(SECRET, ACCT)
            .1;
        let row = s.cloud_write_by_key(ACCT, &key).unwrap().unwrap();
        assert_eq!(row.state, "inflight");
        assert_eq!(row.last_error.as_deref(), Some("transient network failure"));

        recover_pending_cloud_writes(&s, ACCT, &cloud, SECRET, 20).unwrap();
        assert_eq!(*cloud.create_link_calls.borrow(), 2);
        assert_eq!(
            s.cloud_write_by_key(ACCT, &key).unwrap().unwrap().state,
            "applied"
        );
    }

    #[test]
    fn share_link_accepts_organization_scope() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();

        run_share_link(
            &s,
            ACCT,
            "item-1",
            "edit",
            "organization",
            &cloud,
            SECRET,
            10,
        )
        .unwrap();

        let key = ShareIntent::link("item-1", "edit", "organization")
            .unwrap()
            .keys(SECRET, ACCT)
            .1;
        let row = s.cloud_write_by_key(ACCT, &key).unwrap().unwrap();
        assert_eq!(row.state, "applied");
        assert_eq!(row.op_kind, "share");
        assert!(row.idempotency_key.len() > 16);
        assert!(cloud.permissions.borrow()[0]
            .link_scope
            .as_deref()
            .is_some_and(|scope| scope == "organization"));
    }

    #[test]
    fn share_link_embed_unsupported_is_policy_final() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        cloud
            .create_link_errors
            .borrow_mut()
            .push("HTTP 400: embed links are only supported for OneDrive personal".into());

        let err = run_share_link(&s, ACCT, "item-1", "embed", "anonymous", &cloud, SECRET, 10)
            .unwrap_err();

        assert!(err.contains("embed links"));
        let key = ShareIntent::link("item-1", "embed", "anonymous")
            .unwrap()
            .keys(SECRET, ACCT)
            .1;
        let row = s.cloud_write_by_key(ACCT, &key).unwrap().unwrap();
        assert_eq!(row.state, "failed");
        assert_eq!(row.last_error.as_deref(), Some("share_policy_unsupported"));
        assert!(s.pending_cloud_writes(ACCT).unwrap().is_empty());
    }

    #[test]
    fn malformed_share_intent_recovery_fails_closed_without_loop() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        let op = CloudWriteOp {
            op_id: "malformed-share-op".into(),
            account_id: ACCT.into(),
            service: SERVICE.into(),
            op_kind: CloudOpKind::Share.as_str().into(),
            target_id: Some("item-1".into()),
            idempotency_key: "malformed-share-key".into(),
            if_match_etag: None,
            state: "inflight".into(),
            result_id: None,
            intent_json: Some(r#"{"share_kind":"link"}"#.into()),
            attempts: 1,
            last_error: None,
        };
        s.record_cloud_write(&op, 10).unwrap();

        let recovered = recover_pending_cloud_writes(&s, ACCT, &cloud, SECRET, 20).unwrap();

        assert_eq!(recovered, 1);
        assert_eq!(*cloud.create_link_calls.borrow(), 0);
        assert_eq!(*cloud.invite_calls.borrow(), 0);
        let row = s
            .cloud_write_by_key(ACCT, "malformed-share-key")
            .unwrap()
            .unwrap();
        assert_eq!(row.state, "failed");
        assert_eq!(
            row.last_error.as_deref(),
            Some("share link intent has no link_type")
        );
        assert!(s.pending_cloud_writes(ACCT).unwrap().is_empty());
    }

    #[test]
    fn share_redaction_covers_link_and_invite_last_error() {
        let raw = "Graph failed for person@example.com at https://1drv.ms/raw-secret";

        let link_store = Store::open_in_memory().unwrap();
        let link_cloud = FakeCloud::default();
        link_cloud.create_link_errors.borrow_mut().push(raw.into());
        let link_err = run_share_link(
            &link_store,
            ACCT,
            "item-1",
            "view",
            "anonymous",
            &link_cloud,
            SECRET,
            10,
        )
        .unwrap_err();
        assert!(link_err.contains("person@example.com"));
        let link_key = ShareIntent::link("item-1", "view", "anonymous")
            .unwrap()
            .keys(SECRET, ACCT)
            .1;
        let link_row = link_store
            .cloud_write_by_key(ACCT, &link_key)
            .unwrap()
            .unwrap();
        assert_eq!(link_row.state, "inflight");
        assert_eq!(link_row.last_error.as_deref(), Some("share_error"));

        let invite_store = Store::open_in_memory().unwrap();
        let invite_cloud = FakeCloud::default();
        invite_cloud
            .invite_outcomes
            .borrow_mut()
            .push(Err(raw.into()));
        let emails = invite_emails();
        let invite_err = run_invite(
            &invite_store,
            ACCT,
            "item-1",
            &emails,
            "read",
            &invite_cloud,
            SECRET,
            10,
        )
        .unwrap_err();
        assert!(invite_err.contains("person@example.com"));
        let (intent, _) = ShareIntent::invite(ACCT, "item-1", &emails, "read", SECRET).unwrap();
        let invite_key = intent.keys(SECRET, ACCT).1;
        let invite_row = invite_store
            .cloud_write_by_key(ACCT, &invite_key)
            .unwrap()
            .unwrap();
        assert_eq!(invite_row.state, "inflight");
        assert_eq!(invite_row.last_error.as_deref(), Some("share_error"));
        assert!(!invite_row.intent_json.unwrap().contains('@'));
    }

    #[test]
    fn invite_rejects_invalid_inputs_before_ledger() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        for (item_id, emails, role) in [
            ("item-1", Vec::<String>::new(), "read"),
            ("item-1", vec!["not-an-email".to_string()], "read"),
            (
                "item-1",
                vec!["bad\nperson@example.com".to_string()],
                "read",
            ),
            ("item-1", vec!["person@example.com".to_string()], "owner"),
            ("", vec!["person@example.com".to_string()], "read"),
        ] {
            let err = run_invite(&s, ACCT, item_id, &emails, role, &cloud, SECRET, 10)
                .expect_err("invalid invite input must fail");
            assert!(!err.is_empty());
        }
        assert!(
            s.cloud_write_ledger_summary(ACCT).unwrap().is_empty(),
            "invalid invite input must not create ledger rows"
        );
        assert_eq!(*cloud.invite_calls.borrow(), 0);
    }

    #[test]
    fn invite_records_intent_before_graph_mutation() {
        let s = Store::open_in_memory().unwrap();
        let emails = invite_emails();
        let (intent, _) = ShareIntent::invite(ACCT, "item-1", &emails, "read", SECRET).unwrap();
        let (_op_id, key) = intent.keys(SECRET, ACCT);
        let sink = LedgerAssertingInviteSink {
            store: &s,
            account: ACCT,
            key: key.clone(),
            raw_emails: emails.clone(),
            cloud: FakeCloud::default(),
        };

        let out = run_invite(&s, ACCT, "item-1", &emails, "read", &sink, SECRET, 10).unwrap();

        match out {
            ShareOutcome::Invite {
                permission_ids,
                summary,
            } => {
                assert_eq!(permission_ids.len(), 2);
                assert_eq!(summary, "invited 2 recipient(s)");
            }
            other => panic!("unexpected invite outcome: {other:?}"),
        }
        assert_eq!(*sink.cloud.invite_calls.borrow(), 1);
        let row = s.cloud_write_by_key(ACCT, &key).unwrap().unwrap();
        assert_eq!(row.state, "applied");
        assert_eq!(row.op_kind, "share");
        assert_eq!(
            row.result_id, None,
            "multi-invite ids are not packed into result_id"
        );
        let intent_json = row.intent_json.unwrap();
        assert!(intent_json.contains(r#""share_kind":"invite""#));
        assert!(intent_json.contains(r#""recipient_hashes""#));
        assert!(!intent_json.contains('@'));
        assert!(!intent_json.contains("Alpha@example.com"));
        assert!(!intent_json.contains("beta@example.com"));
    }

    #[test]
    fn invite_pending_not_started_boot_recovery_marks_retry_required() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        let emails = invite_emails();
        let op = invite_op("item-1", &emails, "read", "pending");
        let key = op.idempotency_key.clone();
        s.record_cloud_write(&op, 10).unwrap();

        let recovered = recover_pending_cloud_writes(&s, ACCT, &cloud, SECRET, 20).unwrap();

        assert_eq!(recovered, 1);
        assert_eq!(*cloud.invite_calls.borrow(), 0);
        let row = s.cloud_write_by_key(ACCT, &key).unwrap().unwrap();
        assert_eq!(row.state, "failed");
        assert_eq!(row.last_error.as_deref(), Some(INVITE_NOT_STARTED));
        assert!(s.pending_cloud_writes(ACCT).unwrap().is_empty());
    }

    #[test]
    fn invite_user_retry_can_resume_not_started_row() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        let emails = invite_emails();
        let op = invite_op("item-1", &emails, "read", "pending");
        let key = op.idempotency_key.clone();
        let old_op_id = op.op_id.clone();
        s.record_cloud_write(&op, 10).unwrap();
        recover_pending_cloud_writes(&s, ACCT, &cloud, SECRET, 20).unwrap();
        assert_eq!(*cloud.invite_calls.borrow(), 0);

        let out = run_invite(&s, ACCT, "item-1", &emails, "read", &cloud, SECRET, 30).unwrap();

        assert!(matches!(out, ShareOutcome::Invite { .. }));
        assert_eq!(*cloud.invite_calls.borrow(), 1);
        let row = s.cloud_write_by_key(ACCT, &key).unwrap().unwrap();
        assert_eq!(row.op_id, old_op_id);
        assert_eq!(row.state, "applied");
        assert_eq!(
            s.cloud_write_ledger_summary(ACCT).unwrap(),
            vec![("applied".into(), 1)]
        );
    }

    #[test]
    fn invite_inflight_with_all_permissions_recovers_applied_without_invite() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        let emails = invite_emails();
        cloud.permissions.borrow_mut().extend([
            invite_permission("perm-alpha", "alpha@example.com", "read"),
            invite_permission("perm-beta", "beta@example.com", "read"),
        ]);
        let op = invite_op("item-1", &emails, "read", "inflight");
        let key = op.idempotency_key.clone();
        s.record_cloud_write(&op, 10).unwrap();

        let recovered = recover_pending_cloud_writes(&s, ACCT, &cloud, SECRET, 20).unwrap();

        assert_eq!(recovered, 1);
        assert_eq!(
            *cloud.invite_calls.borrow(),
            0,
            "recovery must probe landed permissions, not resend invite"
        );
        let row = s.cloud_write_by_key(ACCT, &key).unwrap().unwrap();
        assert_eq!(row.state, "applied");
        assert_eq!(row.result_id, None);
    }

    #[test]
    fn invite_recovery_ambiguous_fails_closed_without_blocking_later_delete() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        let emails = invite_emails();
        cloud.permissions.borrow_mut().extend([
            invite_permission("perm-alpha-1", "alpha@example.com", "read"),
            invite_permission("perm-alpha-2", "alpha@example.com", "read"),
            invite_permission("perm-beta", "beta@example.com", "read"),
        ]);
        let invite = invite_op("item-1", &emails, "read", "inflight");
        let invite_key = invite.idempotency_key.clone();
        s.record_cloud_write(&invite, 10).unwrap();

        let delete = CloudWrite {
            kind: CloudOpKind::Delete,
            target_id: "later-delete".into(),
            name: String::new(),
            new_parent_id: None,
            if_match: None,
            local_path: None,
            content_tag: None,
        };
        let (delete_id, delete_key) = delete.keys(SECRET, ACCT);
        s.record_cloud_write(
            &CloudWriteOp {
                op_id: delete_id,
                account_id: ACCT.into(),
                service: SERVICE.into(),
                op_kind: "delete".into(),
                target_id: Some("later-delete".into()),
                idempotency_key: delete_key.clone(),
                if_match_etag: None,
                state: "pending".into(),
                result_id: None,
                intent_json: Some(delete.intent_json()),
                attempts: 1,
                last_error: None,
            },
            11,
        )
        .unwrap();

        let recovered = recover_pending_cloud_writes(&s, ACCT, &cloud, SECRET, 20).unwrap();

        assert_eq!(recovered, 2);
        assert_eq!(*cloud.invite_calls.borrow(), 0);
        assert_eq!(*cloud.delete_calls.borrow(), 1);
        let invite_row = s.cloud_write_by_key(ACCT, &invite_key).unwrap().unwrap();
        assert_eq!(invite_row.state, "failed");
        assert_eq!(invite_row.last_error.as_deref(), Some(INVITE_AMBIGUOUS));
        assert_eq!(
            s.cloud_write_by_key(ACCT, &delete_key)
                .unwrap()
                .unwrap()
                .state,
            "applied"
        );
        assert!(s.pending_cloud_writes(ACCT).unwrap().is_empty());
    }

    #[test]
    fn invite_graph_207_partial_success_is_failed_closed_and_not_auto_retried() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        cloud
            .invite_outcomes
            .borrow_mut()
            .push(Ok(InviteOutcome::Partial {
                successful_permission_ids: vec!["perm-ok".into()],
                failed_recipient_count: 1,
                redacted_reason: "partial_success".into(),
            }));
        let emails = invite_emails();
        let (intent, _) = ShareIntent::invite(ACCT, "item-1", &emails, "read", SECRET).unwrap();
        let (_op_id, key) = intent.keys(SECRET, ACCT);

        let err = run_invite(&s, ACCT, "item-1", &emails, "read", &cloud, SECRET, 10)
            .expect_err("Graph 207 partial must fail closed");

        assert_eq!(err, INVITE_PARTIAL_SUCCESS);
        assert_eq!(*cloud.invite_calls.borrow(), 1);
        let row = s.cloud_write_by_key(ACCT, &key).unwrap().unwrap();
        assert_eq!(row.state, "failed");
        assert_eq!(row.result_id, None);
        assert_eq!(row.last_error.as_deref(), Some(INVITE_PARTIAL_SUCCESS));
        let intent_json = row.intent_json.as_deref().unwrap();
        assert!(!intent_json.contains('@'));
        assert!(!row.last_error.as_deref().unwrap().contains('@'));

        let recovered = recover_pending_cloud_writes(&s, ACCT, &cloud, SECRET, 20).unwrap();
        assert_eq!(recovered, 0);
        assert_eq!(
            *cloud.invite_calls.borrow(),
            1,
            "failed partial invite must not be auto-retried"
        );
    }

    #[test]
    fn failed_ambiguous_invite_blocks_silent_identical_retry() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        let emails = invite_emails();
        cloud.permissions.borrow_mut().extend([
            invite_permission("perm-alpha-1", "alpha@example.com", "read"),
            invite_permission("perm-alpha-2", "alpha@example.com", "read"),
            invite_permission("perm-beta", "beta@example.com", "read"),
        ]);
        let op = invite_op("item-1", &emails, "read", "inflight");
        let key = op.idempotency_key.clone();
        s.record_cloud_write(&op, 10).unwrap();
        recover_pending_cloud_writes(&s, ACCT, &cloud, SECRET, 20).unwrap();
        assert_eq!(*cloud.invite_calls.borrow(), 0);

        let err = run_invite(&s, ACCT, "item-1", &emails, "read", &cloud, SECRET, 30)
            .expect_err("ambiguous failed invite must require explicit conflict handling");

        assert_eq!(err, INVITE_AMBIGUOUS);
        assert_eq!(*cloud.invite_calls.borrow(), 0);
        let row = s.cloud_write_by_key(ACCT, &key).unwrap().unwrap();
        assert_eq!(row.state, "failed");
        assert_eq!(row.last_error.as_deref(), Some(INVITE_AMBIGUOUS));
        assert_eq!(
            s.cloud_write_ledger_summary(ACCT).unwrap(),
            vec![("failed".into(), 1)]
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
        let n = recover_pending_cloud_writes(&s, ACCT, &cloud, SECRET, 20).unwrap();
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
    fn upload_recovery_replays_from_sealed_body_source() {
        let _guard = body_key_test_guard();
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        let sentinel = b"isy-om720-upload-sealed-source-sentinel";
        let f = sealed_body_file(720_001, [1u8; 32], sentinel);
        let w = upload("parent", "sealed-upload.txt", f.path());
        let (op_id, key) = w.keys(SECRET, ACCT);
        let op = CloudWriteOp {
            op_id,
            account_id: ACCT.into(),
            service: SERVICE.into(),
            op_kind: "upload".into(),
            target_id: Some("parent".into()),
            idempotency_key: key.clone(),
            if_match_etag: None,
            state: "pending".into(),
            result_id: None,
            intent_json: Some(w.intent_json()),
            attempts: 1,
            last_error: None,
        };
        s.record_cloud_write(&op, 10).unwrap();

        let recovered = recover_pending_cloud_writes(&s, ACCT, &cloud, SECRET, 20).unwrap();

        assert_eq!(recovered, 1);
        assert_eq!(
            *cloud.upload_calls.borrow(),
            1,
            "sealed upload recovery must issue exactly one upload"
        );
        assert_eq!(cloud.file_count(), 1);
        assert_eq!(
            cloud.body_of("file-1").as_deref(),
            Some(sentinel.as_slice())
        );
        let row = s.cloud_write_by_key(ACCT, &key).unwrap().unwrap();
        assert_eq!(row.state, "applied");
        assert_eq!(row.result_id.as_deref(), Some("file-1"));
    }

    #[test]
    fn recovery_skips_a_missing_local_body_op_without_aborting_the_batch() {
        // #656 F-A: an Upload whose local source file is gone can never be re-sent. It must be
        // marked terminally `failed` (leaving the pending set) and MUST NOT abort recovery of the
        // later ops — otherwise one stale write blocks every offline materialize behind it.
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        let gone = std::path::PathBuf::from("/nonexistent/isyncyou-om656-f-a-missing.txt");
        assert!(!gone.exists());

        // op1 (earlier): an Upload whose local body is gone → unrecoverable.
        let w1 = upload("parent", "gone.txt", &gone);
        let (op1_id, key1) = w1.keys(SECRET, ACCT);
        let op1 = CloudWriteOp {
            op_id: op1_id,
            account_id: ACCT.into(),
            service: SERVICE.into(),
            op_kind: "upload".into(),
            target_id: Some("parent".into()),
            idempotency_key: key1.clone(),
            if_match_etag: None,
            state: "pending".into(),
            result_id: None,
            intent_json: Some(w1.intent_json()),
            attempts: 1,
            last_error: None,
        };
        s.record_cloud_write(&op1, 10).unwrap();

        // op2 (later): a Delete → recoverable. Without per-op tolerance, op1's error would abort
        // the loop and op2 would never be reconciled.
        let w2 = CloudWrite {
            kind: CloudOpKind::Delete,
            target_id: "item-2".into(),
            name: String::new(),
            new_parent_id: None,
            if_match: None,
            local_path: None,
            content_tag: None,
        };
        let (op2_id, key2) = w2.keys(SECRET, ACCT);
        let op2 = CloudWriteOp {
            op_id: op2_id,
            account_id: ACCT.into(),
            service: SERVICE.into(),
            op_kind: "delete".into(),
            target_id: Some("item-2".into()),
            idempotency_key: key2.clone(),
            if_match_etag: None,
            state: "pending".into(),
            result_id: None,
            intent_json: Some(w2.intent_json()),
            attempts: 1,
            last_error: None,
        };
        s.record_cloud_write(&op2, 11).unwrap();

        let recovered = recover_pending_cloud_writes(&s, ACCT, &cloud, SECRET, 20).unwrap();
        assert_eq!(
            recovered, 1,
            "the recoverable delete is reconciled despite the failing upload"
        );
        assert_eq!(
            *cloud.delete_calls.borrow(),
            1,
            "the op after the un-reconcilable one still ran"
        );
        // The missing-body op is terminally failed → out of the pending set, no longer blocking.
        assert_eq!(
            s.cloud_write_by_key(ACCT, &key1).unwrap().unwrap().state,
            "failed"
        );
        assert!(
            s.pending_cloud_writes(ACCT).unwrap().is_empty(),
            "no ops remain pending after recovery"
        );
    }

    #[test]
    fn missing_sealed_body_source_fails_terminally_and_recovery_continues() {
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        let missing_upload = std::path::PathBuf::from("/nonexistent/om720-missing-upload.bin");
        let missing_replace = std::path::PathBuf::from("/nonexistent/om720-missing-replace.bin");
        assert!(!missing_upload.exists());
        assert!(!missing_replace.exists());

        let w_upload = upload("parent", "missing-upload.txt", &missing_upload);
        let (upload_id, upload_key) = w_upload.keys(SECRET, ACCT);
        s.record_cloud_write(
            &CloudWriteOp {
                op_id: upload_id,
                account_id: ACCT.into(),
                service: SERVICE.into(),
                op_kind: "upload".into(),
                target_id: Some("parent".into()),
                idempotency_key: upload_key.clone(),
                if_match_etag: None,
                state: "pending".into(),
                result_id: None,
                intent_json: Some(w_upload.intent_json()),
                attempts: 1,
                last_error: None,
            },
            10,
        )
        .unwrap();

        let w_replace = CloudWrite {
            kind: CloudOpKind::Replace,
            target_id: "file-replace".into(),
            name: String::new(),
            new_parent_id: None,
            if_match: Some("etag-old".into()),
            local_path: Some(missing_replace),
            content_tag: Some("tag-missing".into()),
        };
        let (replace_id, replace_key) = w_replace.keys(SECRET, ACCT);
        s.record_cloud_write(
            &CloudWriteOp {
                op_id: replace_id,
                account_id: ACCT.into(),
                service: SERVICE.into(),
                op_kind: "replace".into(),
                target_id: Some("file-replace".into()),
                idempotency_key: replace_key.clone(),
                if_match_etag: Some("etag-old".into()),
                state: "pending".into(),
                result_id: None,
                intent_json: Some(w_replace.intent_json()),
                attempts: 1,
                last_error: None,
            },
            11,
        )
        .unwrap();

        let w_delete = CloudWrite {
            kind: CloudOpKind::Delete,
            target_id: "later-delete".into(),
            name: String::new(),
            new_parent_id: None,
            if_match: None,
            local_path: None,
            content_tag: None,
        };
        let (delete_id, delete_key) = w_delete.keys(SECRET, ACCT);
        s.record_cloud_write(
            &CloudWriteOp {
                op_id: delete_id,
                account_id: ACCT.into(),
                service: SERVICE.into(),
                op_kind: "delete".into(),
                target_id: Some("later-delete".into()),
                idempotency_key: delete_key.clone(),
                if_match_etag: None,
                state: "pending".into(),
                result_id: None,
                intent_json: Some(w_delete.intent_json()),
                attempts: 1,
                last_error: None,
            },
            12,
        )
        .unwrap();

        let recovered = recover_pending_cloud_writes(&s, ACCT, &cloud, SECRET, 20).unwrap();

        assert_eq!(
            recovered, 1,
            "later recoverable op must still run after missing body sources"
        );
        assert_eq!(*cloud.delete_calls.borrow(), 1);
        assert_eq!(
            s.cloud_write_by_key(ACCT, &upload_key)
                .unwrap()
                .unwrap()
                .state,
            "failed"
        );
        assert_eq!(
            s.cloud_write_by_key(ACCT, &replace_key)
                .unwrap()
                .unwrap()
                .state,
            "failed"
        );
        assert_eq!(
            s.cloud_write_by_key(ACCT, &delete_key)
                .unwrap()
                .unwrap()
                .state,
            "applied"
        );
        assert!(
            s.pending_cloud_writes(ACCT).unwrap().is_empty(),
            "missing upload and replace sources must not leave pending blockers"
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
        let n = recover_pending_cloud_writes(&s, ACCT, &cloud, SECRET, 20).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn replace_recovery_replays_from_sealed_body_source() {
        let _guard = body_key_test_guard();
        let s = Store::open_in_memory().unwrap();
        let cloud = FakeCloud::default();
        let fid = cloud.upload("parent", "replace.txt", b"v1").unwrap();
        let etag = cloud.etag_of(&fid);
        let replacement = b"isy-om720-replace-sealed-source-v2";
        let f = sealed_body_file(720_002, [2u8; 32], replacement);
        let w = CloudWrite {
            kind: CloudOpKind::Replace,
            target_id: fid.clone(),
            name: String::new(),
            new_parent_id: None,
            if_match: Some(etag.clone()),
            local_path: Some(f.path().to_path_buf()),
            content_tag: Some("tag-sealed-v2".into()),
        };
        let (op_id, key) = w.keys(SECRET, ACCT);
        s.record_cloud_write(
            &CloudWriteOp {
                op_id,
                account_id: ACCT.into(),
                service: SERVICE.into(),
                op_kind: "replace".into(),
                target_id: Some(fid.clone()),
                idempotency_key: key.clone(),
                if_match_etag: Some(etag.clone()),
                state: "pending".into(),
                result_id: None,
                intent_json: Some(w.intent_json()),
                attempts: 1,
                last_error: None,
            },
            10,
        )
        .unwrap();

        let recovered = recover_pending_cloud_writes(&s, ACCT, &cloud, SECRET, 20).unwrap();

        assert_eq!(recovered, 1);
        assert_eq!(
            *cloud.replace_calls.borrow(),
            1,
            "sealed replace recovery must issue exactly one replace"
        );
        assert_eq!(cloud.body_of(&fid).as_deref(), Some(replacement.as_slice()));
        assert_eq!(
            *cloud.replace_attempts.borrow(),
            vec![(fid.clone(), etag, replacement.to_vec())],
            "recovery must use the recorded If-Match etag path with decrypted bytes"
        );
        let row = s.cloud_write_by_key(ACCT, &key).unwrap().unwrap();
        assert_eq!(row.state, "applied");
        assert_eq!(row.result_id.as_deref(), Some(fid.as_str()));
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
