//! Human confirmation of destructive actions (REQ-AGENT-003 / REQ-AGENT-004).
//!
//! The model/agent never holds a capability token. When it proposes a destructive
//! action the server registers a [`PendingAction`] and gets back a **one-time
//! confirmation token**. The UI shows the preview and, on the user's confirm, posts the
//! token back; [`PendingRegistry::confirm`] verifies it in constant time, enforces a TTL,
//! and is **single-use** (a replay fails). The token is bound to exactly one pending
//! action — confirming returns that action and nothing else.

use crate::tool::ToolAction;
use crate::AgentError;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine;
use ring::digest;
use ring::rand::{SecureRandom, SystemRandom};
use std::collections::HashMap;
use std::sync::Mutex;

const ACTION_HASH_DOMAIN: &str = "isyncyou-agent-confirm-v1";

/// A destructive action awaiting human confirmation. `id` + the (separately returned)
/// one-time token are what the UI confirms with; `preview` is the human-readable diff.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingAction {
    pub id: String,
    pub action: ToolAction,
    pub preview: String,
    pub action_hash: String,
    pub risk: String,
    pub expires_at_ms: u64,
}

/// Non-secret binding fields for a pending destructive action. Mobile uses this
/// to mint a native biometric-token challenge before the Agent confirmation token
/// is consumed. `item` is intentionally bound to the pending id + action hash,
/// not raw payload fields, so the biometric token cannot be reused across two
/// pending actions with the same cloud item but different mutation payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingActionBinding {
    pub op: String,
    pub account: String,
    pub service: String,
    pub item: String,
    pub expires_at_ms: u64,
}

struct Pending {
    action: ToolAction,
    token: String,
    action_hash: String,
    expires_at_ms: u64,
    owner: PendingOwnerBinding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingOwnerBinding {
    pub account: String,
    pub session_id: String,
    pub request_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone)]
pub struct PersistedPendingAction {
    pub id: String,
    pub action: ToolAction,
    pub preview: String,
    pub token_hash: [u8; 32],
    pub action_hash: String,
    pub risk: String,
    pub expires_at_ms: u64,
    pub owner: PendingOwnerBinding,
}

pub trait PendingPersistence: Send + Sync {
    fn insert(&self, pending: PersistedPendingAction) -> Result<(), ConfirmError>;
    fn confirm(
        &self,
        pending_id: &str,
        token_hash: &[u8; 32],
        action_hash: &str,
        now_ms: u64,
    ) -> Result<ToolAction, ConfirmError>;
    fn binding(
        &self,
        pending_id: &str,
        action_hash: &str,
        now_ms: u64,
    ) -> Result<PendingActionBinding, ConfirmError>;
    fn cancel(&self, pending_id: &str, action_hash: &str, now_ms: u64) -> Result<(), ConfirmError>;
    fn has_pending_for_turn(&self, turn_id: &str, now_ms: u64) -> Result<bool, ConfirmError>;
}

/// Why a confirmation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmError {
    /// Unknown id — never registered, already consumed (single-use), or expired+swept.
    NotFound,
    /// The TTL elapsed.
    Expired,
    /// The token did not match this pending action.
    BadToken,
    /// The caller's action hash does not match the registered action binding.
    ActionMismatch,
    /// The durable confirmation store could not complete the transition.
    Unavailable,
}

/// Registry of pending destructive actions, keyed by pending id.
pub struct PendingRegistry {
    inner: Mutex<HashMap<String, Pending>>,
    persistence: Option<std::sync::Arc<dyn PendingPersistence>>,
}

impl Default for PendingRegistry {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            persistence: None,
        }
    }
}

fn random_b64(n: usize) -> Result<String, AgentError> {
    let mut buf = vec![0u8; n];
    SystemRandom::new()
        .fill(&mut buf)
        .map_err(|_| AgentError::Provider("rng".into()))?;
    Ok(B64URL.encode(buf))
}

/// Constant-time byte-equality (textbook XOR-accumulate). Length is allowed to leak —
/// the tokens are fixed-length random — but the byte comparison does not short-circuit.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn confirmation_token_hash(token: &str) -> [u8; 32] {
    let mut context = digest::Context::new(&digest::SHA256);
    context.update(b"isyncyou-confirmation-token-v1\0");
    context.update(token.as_bytes());
    context.finish().as_ref().try_into().expect("sha256 length")
}

pub fn action_hash(action: &ToolAction, expires_at_ms: u64) -> Result<String, AgentError> {
    let payload = serde_json::json!({
        "domain": ACTION_HASH_DOMAIN,
        "v": 1,
        "action": action,
        "binding": {
            "account": action.account(),
            "service": action.service().unwrap_or(""),
            "item": action.item_or_target().unwrap_or(""),
            "expires_at_ms": expires_at_ms,
        },
    });
    let bytes = serde_json::to_vec(&payload).map_err(|e| AgentError::Provider(e.to_string()))?;
    Ok(hex(digest::digest(&digest::SHA256, &bytes).as_ref()))
}

fn biometric_binding_item(pending_id: &str, action_hash: &str) -> String {
    format!(
        "pending:{}:{}:action_hash:{}:{}",
        pending_id.len(),
        pending_id,
        action_hash.len(),
        action_hash
    )
}

impl PendingRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_persistence(persistence: std::sync::Arc<dyn PendingPersistence>) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            persistence: Some(persistence),
        }
    }

    /// Register a destructive action and return its [`PendingAction`] plus the one-time
    /// confirmation token (give the token to the UI; never to the model).
    pub fn register(
        &self,
        action: ToolAction,
        preview: impl Into<String>,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<(PendingAction, String), AgentError> {
        self.register_bound(
            action,
            preview,
            now_ms,
            ttl_ms,
            PendingOwnerBinding {
                account: String::new(),
                session_id: String::new(),
                request_id: String::new(),
                turn_id: String::new(),
            },
        )
    }

    pub fn register_bound(
        &self,
        action: ToolAction,
        preview: impl Into<String>,
        now_ms: u64,
        ttl_ms: u64,
        owner: PendingOwnerBinding,
    ) -> Result<(PendingAction, String), AgentError> {
        let id = random_b64(16)?;
        let token = random_b64(32)?;
        let expires_at_ms = now_ms.saturating_add(ttl_ms);
        let action_hash = action_hash(&action, expires_at_ms)?;
        let risk = "destructive".to_string();
        let preview = preview.into();
        if let Some(persistence) = &self.persistence {
            persistence
                .insert(PersistedPendingAction {
                    id: id.clone(),
                    action: action.clone(),
                    preview: preview.clone(),
                    token_hash: confirmation_token_hash(&token),
                    action_hash: action_hash.clone(),
                    risk: risk.clone(),
                    expires_at_ms,
                    owner,
                })
                .map_err(|_| AgentError::Provider("confirmation_unavailable".into()))?;
        } else {
            self.inner.lock().unwrap().insert(
                id.clone(),
                Pending {
                    action: action.clone(),
                    token: token.clone(),
                    action_hash: action_hash.clone(),
                    expires_at_ms,
                    owner,
                },
            );
        }
        Ok((
            PendingAction {
                id,
                action,
                preview,
                action_hash,
                risk,
                expires_at_ms,
            },
            token,
        ))
    }

    /// Confirm a pending action. Constant-time token check, TTL-enforced, single-use:
    /// on success the action is removed and returned; a replay returns `NotFound`.
    pub fn confirm(
        &self,
        pending_id: &str,
        token: &str,
        action_hash: &str,
        now_ms: u64,
    ) -> Result<ToolAction, ConfirmError> {
        if let Some(persistence) = &self.persistence {
            return persistence.confirm(
                pending_id,
                &confirmation_token_hash(token),
                action_hash,
                now_ms,
            );
        }
        let mut map = self.inner.lock().unwrap();
        let pending = map.get(pending_id).ok_or(ConfirmError::NotFound)?;
        if now_ms > pending.expires_at_ms {
            map.remove(pending_id);
            return Err(ConfirmError::Expired);
        }
        if !ct_eq(action_hash.as_bytes(), pending.action_hash.as_bytes()) {
            return Err(ConfirmError::ActionMismatch);
        }
        if !ct_eq(token.as_bytes(), pending.token.as_bytes()) {
            return Err(ConfirmError::BadToken); // not consumed — the legit user can retry
        }
        // Single-use: consume on success.
        Ok(map.remove(pending_id).expect("present").action)
    }

    /// Return a non-secret action binding without checking or consuming the
    /// one-time Agent confirmation token. This is for mobile's native biometric
    /// gate, which must run before [`Self::confirm`] can safely consume the token.
    pub fn binding(
        &self,
        pending_id: &str,
        action_hash: &str,
        now_ms: u64,
    ) -> Result<PendingActionBinding, ConfirmError> {
        if let Some(persistence) = &self.persistence {
            return persistence.binding(pending_id, action_hash, now_ms);
        }
        let mut map = self.inner.lock().unwrap();
        let pending = map.get(pending_id).ok_or(ConfirmError::NotFound)?;
        if now_ms > pending.expires_at_ms {
            map.remove(pending_id);
            return Err(ConfirmError::Expired);
        }
        if !ct_eq(action_hash.as_bytes(), pending.action_hash.as_bytes()) {
            return Err(ConfirmError::ActionMismatch);
        }
        Ok(PendingActionBinding {
            op: pending.action.op().to_string(),
            account: pending.action.account().to_string(),
            service: pending.action.service().unwrap_or("agent").to_string(),
            item: biometric_binding_item(pending_id, action_hash),
            expires_at_ms: pending.expires_at_ms,
        })
    }

    /// Cancel exactly the pending action bound to the supplied public action hash.
    /// Cancellation reduces authority and therefore never consumes a confirmation token.
    pub fn cancel(
        &self,
        pending_id: &str,
        action_hash: &str,
        now_ms: u64,
    ) -> Result<(), ConfirmError> {
        if let Some(persistence) = &self.persistence {
            return persistence.cancel(pending_id, action_hash, now_ms);
        }
        let mut map = self.inner.lock().unwrap();
        let pending = map.get(pending_id).ok_or(ConfirmError::NotFound)?;
        if now_ms > pending.expires_at_ms {
            map.remove(pending_id);
            return Err(ConfirmError::Expired);
        }
        if !ct_eq(action_hash.as_bytes(), pending.action_hash.as_bytes()) {
            return Err(ConfirmError::ActionMismatch);
        }
        map.remove(pending_id);
        Ok(())
    }

    pub fn has_pending_for_turn(&self, turn_id: &str, now_ms: u64) -> Result<bool, ConfirmError> {
        if let Some(persistence) = &self.persistence {
            return persistence.has_pending_for_turn(turn_id, now_ms);
        }
        let mut map = self.inner.lock().unwrap();
        map.retain(|_, pending| now_ms <= pending.expires_at_ms);
        Ok(map.values().any(|pending| pending.owner.turn_id == turn_id))
    }

    /// Number of outstanding pending actions (for tests/metrics).
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn backup() -> ToolAction {
        crate::tool::parse_action(&json!({"op":"backup","account":"me","services":["mail"]}))
            .unwrap()
    }

    #[test]
    fn confirmation_token_is_single_use_and_action_bound() {
        let reg = PendingRegistry::new();
        let (pending, token) = reg
            .register(backup(), "back up mail", 1_000, 60_000)
            .unwrap();
        assert_eq!(pending.risk, "destructive");
        assert_eq!(pending.expires_at_ms, 61_000);
        assert_eq!(pending.action_hash.len(), 64);
        let action = reg
            .confirm(&pending.id, &token, &pending.action_hash, 2_000)
            .unwrap();
        assert_eq!(action.op(), "backup");
        // replay → consumed
        assert_eq!(
            reg.confirm(&pending.id, &token, &pending.action_hash, 2_001),
            Err(ConfirmError::NotFound)
        );
        assert!(reg.is_empty());
    }

    #[test]
    fn wrong_token_is_rejected_and_does_not_consume() {
        let reg = PendingRegistry::new();
        let (pending, token) = reg.register(backup(), "p", 0, 60_000).unwrap();
        assert_eq!(
            reg.confirm(&pending.id, "not-the-token", &pending.action_hash, 1),
            Err(ConfirmError::BadToken)
        );
        // still confirmable with the real token afterwards
        assert!(reg
            .confirm(&pending.id, &token, &pending.action_hash, 2)
            .is_ok());
    }

    #[test]
    fn confirm_rejects_action_hash_mismatch_without_consuming() {
        let reg = PendingRegistry::new();
        let (pending, token) = reg.register(backup(), "p", 0, 60_000).unwrap();
        let bad_hash = action_hash(&backup(), pending.expires_at_ms + 1).unwrap();
        assert_ne!(pending.action_hash, bad_hash);
        assert_eq!(
            reg.confirm(&pending.id, &token, &bad_hash, 1),
            Err(ConfirmError::ActionMismatch)
        );
        assert!(reg
            .confirm(&pending.id, &token, &pending.action_hash, 2)
            .is_ok());
    }

    #[test]
    fn agent_pending_binding_peek_does_not_consume_confirmation() {
        let reg = PendingRegistry::new();
        let (pending, token) = reg.register(backup(), "p", 0, 60_000).unwrap();

        let binding = reg
            .binding(&pending.id, &pending.action_hash, 1)
            .expect("binding peek");

        assert_eq!(binding.op, "backup");
        assert_eq!(binding.account, "me");
        assert_eq!(binding.service, "agent");
        assert!(binding.item.contains(&pending.id));
        assert!(binding.item.contains(&pending.action_hash));
        assert_eq!(binding.expires_at_ms, pending.expires_at_ms);
        assert_eq!(reg.len(), 1, "peek must not consume the pending action");
        assert!(reg
            .confirm(&pending.id, &token, &pending.action_hash, 2)
            .is_ok());
    }

    #[test]
    fn agent_pending_binding_rejects_action_hash_mismatch() {
        let reg = PendingRegistry::new();
        let (pending, token) = reg.register(backup(), "p", 0, 60_000).unwrap();
        let bad_hash = action_hash(&backup(), pending.expires_at_ms + 1).unwrap();

        assert_eq!(
            reg.binding(&pending.id, &bad_hash, 1),
            Err(ConfirmError::ActionMismatch)
        );
        assert_eq!(reg.len(), 1, "bad binding peek must not consume");
        assert!(reg
            .confirm(&pending.id, &token, &pending.action_hash, 2)
            .is_ok());
    }

    #[test]
    fn confirm_rejects_token_from_another_pending() {
        let reg = PendingRegistry::new();
        let (p1, _t1) = reg.register(backup(), "p1", 0, 60_000).unwrap();
        let (_p2, t2) = reg.register(backup(), "p2", 0, 60_000).unwrap();
        // t2 cannot confirm p1
        assert_eq!(
            reg.confirm(&p1.id, &t2, &p1.action_hash, 1),
            Err(ConfirmError::BadToken)
        );
    }

    #[test]
    fn expired_confirmation_token_is_rejected_and_swept() {
        let reg = PendingRegistry::new();
        let (pending, token) = reg.register(backup(), "p", 1_000, 5_000).unwrap();
        assert_eq!(
            reg.confirm(&pending.id, &token, &pending.action_hash, 10_000),
            Err(ConfirmError::Expired)
        );
        assert!(reg.is_empty()); // swept
    }

    #[test]
    fn unknown_id_is_not_found() {
        let reg = PendingRegistry::new();
        assert_eq!(
            reg.confirm("nope", "x", "hash", 0),
            Err(ConfirmError::NotFound)
        );
    }

    #[test]
    fn action_hash_changes_when_binding_fields_change() {
        let restore = crate::tool::parse_action(
            &json!({"op":"restore-cloud","account":"me","service":"mail","id":"m1"}),
        )
        .unwrap();
        let different_item = crate::tool::parse_action(
            &json!({"op":"restore-cloud","account":"me","service":"mail","id":"m2"}),
        )
        .unwrap();
        assert_ne!(
            action_hash(&restore, 60_000).unwrap(),
            action_hash(&different_item, 60_000).unwrap()
        );
        assert_ne!(
            action_hash(&restore, 60_000).unwrap(),
            action_hash(&restore, 60_001).unwrap()
        );
    }

    #[test]
    fn pending_cancel_makes_confirm_binding_and_replay_fail() {
        let reg = PendingRegistry::new();
        let (pending, token) = reg
            .register_bound(
                backup(),
                "p",
                1_000,
                60_000,
                PendingOwnerBinding {
                    account: "me".into(),
                    session_id: "session".into(),
                    request_id: "request".into(),
                    turn_id: "turn".into(),
                },
            )
            .unwrap();
        assert!(reg.has_pending_for_turn("turn", 2_000).unwrap());
        reg.cancel(&pending.id, &pending.action_hash, 2_000)
            .unwrap();
        assert!(!reg.has_pending_for_turn("turn", 2_001).unwrap());
        assert_eq!(
            reg.binding(&pending.id, &pending.action_hash, 2_001),
            Err(ConfirmError::NotFound)
        );
        assert_eq!(
            reg.confirm(&pending.id, &token, &pending.action_hash, 2_001),
            Err(ConfirmError::NotFound)
        );
    }
}
