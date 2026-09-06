//! Human confirmation of destructive actions (REQ-AGENT-003 / REQ-AGENT-004).
//!
//! The model/agent never holds a capability token. When it proposes a destructive
//! action the server registers a [`PendingAction`] and gets back a **one-time
//! confirmation token**. The UI shows the preview and, on the user's confirm, posts the
//! token back; [`PendingRegistry::confirm`] verifies it in constant time, enforces a TTL,
//! and is **single-use** (a replay fails). The token is bound to exactly one pending
//! action — confirming returns that action and nothing else.

use crate::tool::{ToolAction, ToolPolicy};
use crate::AgentError;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine;
use ring::digest;
use ring::rand::{SecureRandom, SystemRandom};
use std::collections::HashMap;
use std::sync::Mutex;

const ACTION_HASH_DOMAIN: &str = "isyncyou-agent-confirm-v1";
const MAX_PENDING_TTL_MS: u64 = 120_000;
const MAX_PENDING_PREVIEW_BYTES: usize = 512;
const MAX_PENDING_OWNER_BYTES: usize = 128;

/// A destructive action awaiting human confirmation. `id` + the (separately returned)
/// one-time token are what the UI confirms with; `preview` is the human-readable diff.
#[derive(Clone, PartialEq)]
pub struct PendingAction {
    pub id: String,
    pub action: ToolAction,
    pub preview: String,
    pub action_hash: String,
    pub risk: String,
    pub expires_at_ms: u64,
}

impl std::fmt::Debug for PendingAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingAction")
            .field("id_present", &!self.id.is_empty())
            .field("op", &self.action.op())
            .field("preview_present", &!self.preview.is_empty())
            .field("action_hash_present", &!self.action_hash.is_empty())
            .field("risk", &self.risk)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

/// Non-secret binding fields for a pending destructive action. Mobile uses this
/// to mint a native biometric-token challenge before the Agent confirmation token
/// is consumed. `item` is intentionally bound to the pending id + action hash,
/// not raw payload fields, so the biometric token cannot be reused across two
/// pending actions with the same cloud item but different mutation payloads.
#[derive(Clone, PartialEq, Eq)]
pub struct PendingActionBinding {
    pub op: String,
    pub account: String,
    pub service: String,
    pub item: String,
    pub expires_at_ms: u64,
}

impl std::fmt::Debug for PendingActionBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingActionBinding")
            .field("op_present", &!self.op.is_empty())
            .field("account_present", &!self.account.is_empty())
            .field("service_present", &!self.service.is_empty())
            .field("item_present", &!self.item.is_empty())
            .finish()
    }
}

struct Pending {
    action: ToolAction,
    token: String,
    preview: String,
    action_hash: String,
    risk: String,
    expires_at_ms: u64,
    owner: PendingOwnerBinding,
}

#[derive(Clone, PartialEq, Eq)]
pub struct PendingOwnerBinding {
    pub account: String,
    pub session_id: String,
    pub request_id: String,
    pub turn_id: String,
}

impl std::fmt::Debug for PendingOwnerBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingOwnerBinding")
            .field("account_present", &!self.account.is_empty())
            .field("session_id_present", &!self.session_id.is_empty())
            .field("request_id_present", &!self.request_id.is_empty())
            .field("turn_id_present", &!self.turn_id.is_empty())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PendingOwnerProof {
    pub session_id: String,
    pub turn_request_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosedConfirmationCode {
    Invalid,
    Expired,
    Cancelled,
    Replayed,
}

#[derive(Clone, PartialEq)]
pub enum PendingConfirmOutcome {
    Confirmed(Box<ToolAction>),
    Rejected(ClosedConfirmationCode),
    RetainedRetryable,
    ConsumedOrCommitUnknown,
}

impl std::fmt::Debug for PendingConfirmOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Confirmed(_) => f.write_str("Confirmed(<redacted>)"),
            Self::Rejected(code) => f.debug_tuple("Rejected").field(code).finish(),
            Self::RetainedRetryable => f.write_str("RetainedRetryable"),
            Self::ConsumedOrCommitUnknown => f.write_str("ConsumedOrCommitUnknown"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingBindingOutcome<T> {
    Ready(T),
    Rejected(ClosedConfirmationCode),
    Unavailable,
}

impl std::fmt::Debug for PendingOwnerProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingOwnerProof")
            .field("session_id_present", &!self.session_id.is_empty())
            .field("turn_request_id_present", &!self.turn_request_id.is_empty())
            .field("turn_id_present", &!self.turn_id.is_empty())
            .finish()
    }
}

#[derive(Clone)]
pub struct PersistedPendingAction {
    pub id: String,
    pub action: ToolAction,
    pub preview: String,
    pub token_hash: [u8; 32],
    pub action_hash: String,
    pub risk: String,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub owner: PendingOwnerBinding,
}

impl std::fmt::Debug for PersistedPendingAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersistedPendingAction")
            .field("id_present", &!self.id.is_empty())
            .field("op", &self.action.op())
            .field("preview_present", &!self.preview.is_empty())
            .field("token_hash_present", &true)
            .field("action_hash_present", &!self.action_hash.is_empty())
            .field("risk", &self.risk)
            .field("created_at_ms", &self.created_at_ms)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("owner", &self.owner)
            .finish()
    }
}

pub trait PendingPersistence: Send + Sync {
    fn insert(&self, pending: PersistedPendingAction) -> Result<(), ConfirmError>;
    fn confirm(
        &self,
        pending_id: &str,
        token_hash: &[u8; 32],
        action_hash: &str,
        owner: &PendingOwnerProof,
        now_ms: u64,
    ) -> PendingConfirmOutcome;
    fn binding(
        &self,
        pending_id: &str,
        action_hash: &str,
        owner: &PendingOwnerProof,
        now_ms: u64,
    ) -> PendingBindingOutcome<PendingActionBinding>;
    fn cancel(
        &self,
        pending_id: &str,
        action_hash: &str,
        now_ms: u64,
    ) -> Result<PendingOwnerBinding, ConfirmError>;
    fn reissue_for_owner(
        &self,
        owner: &PendingOwnerBinding,
        token_hash: &[u8; 32],
        now_ms: u64,
    ) -> Result<Option<PendingAction>, ConfirmError>;
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
    /// The caller does not own the originating session/request/turn.
    OwnerMismatch,
    /// The stored action is not confirmable under the current exhaustive policy.
    PolicyMismatch,
    /// Registration input was malformed or exceeded its bounded representation.
    InvalidRegistration,
    /// The durable confirmation store has reached its bounded capacity.
    Capacity,
    /// Another pending confirmation already owns the same durable binding.
    Conflict,
    /// Expired confirmation cleanup failed before registration.
    MaintenanceUnavailable,
    /// The bounded confirmation payload could not be sealed.
    SealUnavailable,
    /// Durable quota state could not be read or validated.
    QuotaUnavailable,
    /// The durable confirmation database operation failed.
    DatabaseUnavailable,
    /// The durable confirmation transaction could not be committed.
    CommitUnavailable,
    /// The durable confirmation store could not complete the transition.
    Unavailable,
}

fn pending_registration_error(error: ConfirmError) -> &'static str {
    match error {
        ConfirmError::OwnerMismatch => "pending_owner_mismatch",
        ConfirmError::PolicyMismatch => "pending_policy_mismatch",
        ConfirmError::InvalidRegistration => "pending_registration_invalid",
        ConfirmError::Capacity => "pending_capacity_unavailable",
        ConfirmError::Conflict => "pending_registration_conflict",
        ConfirmError::MaintenanceUnavailable => "pending_maintenance_unavailable",
        ConfirmError::SealUnavailable => "pending_seal_unavailable",
        ConfirmError::QuotaUnavailable => "pending_quota_unavailable",
        ConfirmError::DatabaseUnavailable => "pending_database_unavailable",
        ConfirmError::CommitUnavailable => "pending_commit_unavailable",
        _ => "confirmation_unavailable",
    }
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

fn valid_owner_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PENDING_OWNER_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_owner(owner: &PendingOwnerBinding) -> bool {
    !owner.account.is_empty()
        && owner.account.len() <= MAX_PENDING_OWNER_BYTES
        && !owner.account.chars().any(char::is_control)
        && valid_owner_component(&owner.session_id)
        && valid_owner_component(&owner.request_id)
        && valid_owner_component(&owner.turn_id)
}

fn owner_proof_matches(owner: &PendingOwnerBinding, proof: &PendingOwnerProof) -> bool {
    owner.session_id == proof.session_id
        && owner.request_id == proof.turn_request_id
        && owner.turn_id == proof.turn_id
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

    pub fn is_persistent(&self) -> bool {
        self.persistence.is_some()
    }

    /// Register a destructive action and return its [`PendingAction`] plus the one-time
    /// confirmation token (give the token to the UI; never to the model).
    #[cfg(test)]
    pub fn register(
        &self,
        action: ToolAction,
        preview: impl Into<String>,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<(PendingAction, String), AgentError> {
        let account = action.account().to_string();
        self.register_bound(
            action,
            preview,
            now_ms,
            ttl_ms,
            PendingOwnerBinding {
                account,
                session_id: "test-session".into(),
                request_id: "00000000-0000-4000-8000-000000000000".into(),
                turn_id: "test-turn".into(),
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
        let preview = preview.into();
        if action.policy() != ToolPolicy::ConfirmedEffectNeverRepeat {
            return Err(AgentError::Provider("not_confirmable".into()));
        }
        if !valid_owner(&owner)
            || ttl_ms == 0
            || ttl_ms > MAX_PENDING_TTL_MS
            || preview.is_empty()
            || preview.len() > MAX_PENDING_PREVIEW_BYTES
        {
            return Err(AgentError::Provider(
                "pending_registration_unavailable".into(),
            ));
        }
        let expires_at_ms = now_ms
            .checked_add(ttl_ms)
            .ok_or_else(|| AgentError::Provider("pending_registration_unavailable".into()))?;
        let id = random_b64(16)?;
        let token = random_b64(32)?;
        let action_hash = action_hash(&action, expires_at_ms)?;
        let risk = "destructive".to_string();
        if let Some(persistence) = &self.persistence {
            persistence
                .insert(PersistedPendingAction {
                    id: id.clone(),
                    action: action.clone(),
                    preview: preview.clone(),
                    token_hash: confirmation_token_hash(&token),
                    action_hash: action_hash.clone(),
                    risk: risk.clone(),
                    created_at_ms: now_ms,
                    expires_at_ms,
                    owner,
                })
                .map_err(|error| AgentError::Provider(pending_registration_error(error).into()))?;
        } else {
            self.inner.lock().unwrap().insert(
                id.clone(),
                Pending {
                    action: action.clone(),
                    token: token.clone(),
                    preview: preview.clone(),
                    action_hash: action_hash.clone(),
                    risk: risk.clone(),
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
        owner: &PendingOwnerProof,
        now_ms: u64,
    ) -> PendingConfirmOutcome {
        if let Some(persistence) = &self.persistence {
            return persistence.confirm(
                pending_id,
                &confirmation_token_hash(token),
                action_hash,
                owner,
                now_ms,
            );
        }
        let Ok(mut map) = self.inner.lock() else {
            return PendingConfirmOutcome::RetainedRetryable;
        };
        let Some(pending) = map.get(pending_id) else {
            return PendingConfirmOutcome::Rejected(ClosedConfirmationCode::Replayed);
        };
        if !owner_proof_matches(&pending.owner, owner) {
            return PendingConfirmOutcome::Rejected(ClosedConfirmationCode::Invalid);
        }
        if now_ms >= pending.expires_at_ms {
            map.remove(pending_id);
            return PendingConfirmOutcome::Rejected(ClosedConfirmationCode::Expired);
        }
        if !ct_eq(action_hash.as_bytes(), pending.action_hash.as_bytes()) {
            return PendingConfirmOutcome::Rejected(ClosedConfirmationCode::Invalid);
        }
        let Ok(recomputed_hash) =
            crate::confirm::action_hash(&pending.action, pending.expires_at_ms)
        else {
            return PendingConfirmOutcome::RetainedRetryable;
        };
        if !ct_eq(recomputed_hash.as_bytes(), pending.action_hash.as_bytes())
            || pending.action.policy() != ToolPolicy::ConfirmedEffectNeverRepeat
            || pending.action.account() != pending.owner.account
        {
            return PendingConfirmOutcome::Rejected(ClosedConfirmationCode::Invalid);
        }
        if !ct_eq(token.as_bytes(), pending.token.as_bytes()) {
            return PendingConfirmOutcome::Rejected(ClosedConfirmationCode::Invalid);
        }
        // Single-use: consume on success.
        PendingConfirmOutcome::Confirmed(Box::new(map.remove(pending_id).expect("present").action))
    }

    /// Return a non-secret action binding without checking or consuming the
    /// one-time Agent confirmation token. This is for mobile's native biometric
    /// gate, which must run before [`Self::confirm`] can safely consume the token.
    pub fn binding(
        &self,
        pending_id: &str,
        action_hash: &str,
        owner: &PendingOwnerProof,
        now_ms: u64,
    ) -> PendingBindingOutcome<PendingActionBinding> {
        if let Some(persistence) = &self.persistence {
            return persistence.binding(pending_id, action_hash, owner, now_ms);
        }
        let Ok(mut map) = self.inner.lock() else {
            return PendingBindingOutcome::Unavailable;
        };
        let Some(pending) = map.get(pending_id) else {
            return PendingBindingOutcome::Rejected(ClosedConfirmationCode::Replayed);
        };
        if !owner_proof_matches(&pending.owner, owner) {
            return PendingBindingOutcome::Rejected(ClosedConfirmationCode::Invalid);
        }
        if now_ms >= pending.expires_at_ms {
            map.remove(pending_id);
            return PendingBindingOutcome::Rejected(ClosedConfirmationCode::Expired);
        }
        if !ct_eq(action_hash.as_bytes(), pending.action_hash.as_bytes()) {
            return PendingBindingOutcome::Rejected(ClosedConfirmationCode::Invalid);
        }
        let Ok(recomputed_hash) =
            crate::confirm::action_hash(&pending.action, pending.expires_at_ms)
        else {
            return PendingBindingOutcome::Unavailable;
        };
        if !ct_eq(recomputed_hash.as_bytes(), pending.action_hash.as_bytes())
            || pending.action.policy() != ToolPolicy::ConfirmedEffectNeverRepeat
            || pending.action.account() != pending.owner.account
        {
            return PendingBindingOutcome::Rejected(ClosedConfirmationCode::Invalid);
        }
        PendingBindingOutcome::Ready(PendingActionBinding {
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
    ) -> Result<PendingOwnerBinding, ConfirmError> {
        if let Some(persistence) = &self.persistence {
            return persistence.cancel(pending_id, action_hash, now_ms);
        }
        let mut map = self.inner.lock().unwrap();
        let pending = map.get(pending_id).ok_or(ConfirmError::NotFound)?;
        if now_ms >= pending.expires_at_ms {
            map.remove(pending_id);
            return Err(ConfirmError::Expired);
        }
        if !ct_eq(action_hash.as_bytes(), pending.action_hash.as_bytes()) {
            return Err(ConfirmError::ActionMismatch);
        }
        Ok(map.remove(pending_id).expect("present").owner)
    }

    /// Rotate the one-time confirmation token for the exact durable pending action
    /// owned by a replayed turn. The raw token is never persisted; a lost stream can
    /// therefore recover authority only by invalidating the old hash and returning a
    /// freshly generated token to the same session/request binding.
    pub fn reissue_for_owner(
        &self,
        owner: &PendingOwnerBinding,
        now_ms: u64,
    ) -> Result<Option<(PendingAction, String)>, ConfirmError> {
        let token = random_b64(32).map_err(|_| ConfirmError::Unavailable)?;
        let token_hash = confirmation_token_hash(&token);
        if let Some(persistence) = &self.persistence {
            return persistence
                .reissue_for_owner(owner, &token_hash, now_ms)
                .map(|pending| pending.map(|pending| (pending, token)));
        }

        let mut map = self.inner.lock().map_err(|_| ConfirmError::Unavailable)?;
        map.retain(|_, pending| now_ms < pending.expires_at_ms);
        let matching = map
            .iter()
            .filter(|(_, pending)| &pending.owner == owner)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let [pending_id] = matching.as_slice() else {
            return if matching.is_empty() {
                Ok(None)
            } else {
                Err(ConfirmError::Unavailable)
            };
        };
        let pending = map.get_mut(pending_id).ok_or(ConfirmError::Unavailable)?;
        pending.token.clone_from(&token);
        Ok(Some((
            PendingAction {
                id: pending_id.clone(),
                action: pending.action.clone(),
                preview: pending.preview.clone(),
                action_hash: pending.action_hash.clone(),
                risk: pending.risk.clone(),
                expires_at_ms: pending.expires_at_ms,
            },
            token,
        )))
    }

    pub fn has_pending_for_turn(&self, turn_id: &str, now_ms: u64) -> Result<bool, ConfirmError> {
        if let Some(persistence) = &self.persistence {
            return persistence.has_pending_for_turn(turn_id, now_ms);
        }
        let mut map = self.inner.lock().unwrap();
        map.retain(|_, pending| now_ms < pending.expires_at_ms);
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

    #[test]
    fn pending_confirmation_outcome_and_binding_debug_redact_action_and_authority() {
        let action = crate::tool::parse_action(&json!({
            "op":"live-write", "account":"private-account", "service":"mail",
            "target":"private-item", "change":{"verb":"set_read","is_read":true}
        }))
        .unwrap();
        let outcome = PendingConfirmOutcome::Confirmed(Box::new(action));
        let binding = PendingBindingOutcome::Ready(PendingActionBinding {
            op: "private-op".into(),
            account: "private-account".into(),
            service: "private-service".into(),
            item: "private-pending-private-hash".into(),
            expires_at_ms: 123,
        });
        let diagnostic = format!("{outcome:?} {binding:?}");
        for forbidden in ["private-", "set_read", "is_read"] {
            assert!(!diagnostic.contains(forbidden));
        }
        assert_eq!(format!("{outcome:?}"), "Confirmed(<redacted>)");
        assert!(diagnostic.contains("item_present: true"));
    }

    #[test]
    fn pending_registration_failure_codes_are_closed_and_stable() {
        assert_eq!(
            pending_registration_error(ConfirmError::OwnerMismatch),
            "pending_owner_mismatch"
        );
        assert_eq!(
            pending_registration_error(ConfirmError::PolicyMismatch),
            "pending_policy_mismatch"
        );
        assert_eq!(
            pending_registration_error(ConfirmError::InvalidRegistration),
            "pending_registration_invalid"
        );
        assert_eq!(
            pending_registration_error(ConfirmError::Capacity),
            "pending_capacity_unavailable"
        );
        assert_eq!(
            pending_registration_error(ConfirmError::Conflict),
            "pending_registration_conflict"
        );
        assert_eq!(
            pending_registration_error(ConfirmError::MaintenanceUnavailable),
            "pending_maintenance_unavailable"
        );
        assert_eq!(
            pending_registration_error(ConfirmError::SealUnavailable),
            "pending_seal_unavailable"
        );
        assert_eq!(
            pending_registration_error(ConfirmError::QuotaUnavailable),
            "pending_quota_unavailable"
        );
        assert_eq!(
            pending_registration_error(ConfirmError::DatabaseUnavailable),
            "pending_database_unavailable"
        );
        assert_eq!(
            pending_registration_error(ConfirmError::CommitUnavailable),
            "pending_commit_unavailable"
        );
        assert_eq!(
            pending_registration_error(ConfirmError::Unavailable),
            "confirmation_unavailable"
        );
    }

    fn backup() -> ToolAction {
        crate::tool::parse_action(&json!({"op":"backup","account":"me","services":["mail"]}))
            .unwrap()
    }

    fn test_proof() -> PendingOwnerProof {
        PendingOwnerProof {
            session_id: "test-session".into(),
            turn_request_id: "00000000-0000-4000-8000-000000000000".into(),
            turn_id: "test-turn".into(),
        }
    }

    fn proof_for(owner: &PendingOwnerBinding) -> PendingOwnerProof {
        PendingOwnerProof {
            session_id: owner.session_id.clone(),
            turn_request_id: owner.request_id.clone(),
            turn_id: owner.turn_id.clone(),
        }
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
        let PendingConfirmOutcome::Confirmed(action) = reg.confirm(
            &pending.id,
            &token,
            &pending.action_hash,
            &test_proof(),
            2_000,
        ) else {
            panic!("valid authority must confirm");
        };
        assert_eq!(action.op(), "backup");
        // replay → consumed
        assert_eq!(
            reg.confirm(
                &pending.id,
                &token,
                &pending.action_hash,
                &test_proof(),
                2_001
            ),
            PendingConfirmOutcome::Rejected(ClosedConfirmationCode::Replayed)
        );
        assert!(reg.is_empty());
    }

    #[test]
    fn wrong_token_is_rejected_and_does_not_consume() {
        let reg = PendingRegistry::new();
        let (pending, token) = reg.register(backup(), "p", 0, 60_000).unwrap();
        assert_eq!(
            reg.confirm(
                &pending.id,
                "not-the-token",
                &pending.action_hash,
                &test_proof(),
                1
            ),
            PendingConfirmOutcome::Rejected(ClosedConfirmationCode::Invalid)
        );
        // still confirmable with the real token afterwards
        assert!(matches!(
            reg.confirm(&pending.id, &token, &pending.action_hash, &test_proof(), 2),
            PendingConfirmOutcome::Confirmed(_)
        ));
    }

    #[test]
    fn confirm_rejects_action_hash_mismatch_without_consuming() {
        let reg = PendingRegistry::new();
        let (pending, token) = reg.register(backup(), "p", 0, 60_000).unwrap();
        let bad_hash = action_hash(&backup(), pending.expires_at_ms + 1).unwrap();
        assert_ne!(pending.action_hash, bad_hash);
        assert_eq!(
            reg.confirm(&pending.id, &token, &bad_hash, &test_proof(), 1),
            PendingConfirmOutcome::Rejected(ClosedConfirmationCode::Invalid)
        );
        assert!(matches!(
            reg.confirm(&pending.id, &token, &pending.action_hash, &test_proof(), 2),
            PendingConfirmOutcome::Confirmed(_)
        ));
    }

    #[test]
    fn agent_pending_binding_peek_does_not_consume_confirmation() {
        let reg = PendingRegistry::new();
        let (pending, token) = reg.register(backup(), "p", 0, 60_000).unwrap();

        let PendingBindingOutcome::Ready(binding) =
            reg.binding(&pending.id, &pending.action_hash, &test_proof(), 1)
        else {
            panic!("binding peek must succeed");
        };

        assert_eq!(binding.op, "backup");
        assert_eq!(binding.account, "me");
        assert_eq!(binding.service, "agent");
        assert!(binding.item.contains(&pending.id));
        assert!(binding.item.contains(&pending.action_hash));
        assert_eq!(binding.expires_at_ms, pending.expires_at_ms);
        assert_eq!(reg.len(), 1, "peek must not consume the pending action");
        assert!(matches!(
            reg.confirm(&pending.id, &token, &pending.action_hash, &test_proof(), 2),
            PendingConfirmOutcome::Confirmed(_)
        ));
    }

    #[test]
    fn agent_pending_binding_rejects_action_hash_mismatch() {
        let reg = PendingRegistry::new();
        let (pending, token) = reg.register(backup(), "p", 0, 60_000).unwrap();
        let bad_hash = action_hash(&backup(), pending.expires_at_ms + 1).unwrap();

        assert_eq!(
            reg.binding(&pending.id, &bad_hash, &test_proof(), 1),
            PendingBindingOutcome::Rejected(ClosedConfirmationCode::Invalid)
        );
        assert_eq!(reg.len(), 1, "bad binding peek must not consume");
        assert!(matches!(
            reg.confirm(&pending.id, &token, &pending.action_hash, &test_proof(), 2),
            PendingConfirmOutcome::Confirmed(_)
        ));
    }

    #[test]
    fn confirm_rejects_token_from_another_pending() {
        let reg = PendingRegistry::new();
        let (p1, _t1) = reg.register(backup(), "p1", 0, 60_000).unwrap();
        let (_p2, t2) = reg.register(backup(), "p2", 0, 60_000).unwrap();
        // t2 cannot confirm p1
        assert_eq!(
            reg.confirm(&p1.id, &t2, &p1.action_hash, &test_proof(), 1),
            PendingConfirmOutcome::Rejected(ClosedConfirmationCode::Invalid)
        );
    }

    #[test]
    fn expired_confirmation_token_is_rejected_and_swept() {
        let reg = PendingRegistry::new();
        let (pending, token) = reg.register(backup(), "p", 1_000, 5_000).unwrap();
        assert_eq!(
            reg.confirm(
                &pending.id,
                &token,
                &pending.action_hash,
                &test_proof(),
                10_000
            ),
            PendingConfirmOutcome::Rejected(ClosedConfirmationCode::Expired)
        );
        assert!(reg.is_empty()); // swept
    }

    #[test]
    fn unknown_id_is_not_found() {
        let reg = PendingRegistry::new();
        assert_eq!(
            reg.confirm("nope", "x", "hash", &test_proof(), 0),
            PendingConfirmOutcome::Rejected(ClosedConfirmationCode::Replayed)
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
        let owner = reg
            .cancel(&pending.id, &pending.action_hash, 2_000)
            .unwrap();
        assert_eq!(owner.session_id, "session");
        assert_eq!(owner.request_id, "request");
        assert_eq!(owner.turn_id, "turn");
        assert!(!reg.has_pending_for_turn("turn", 2_001).unwrap());
        assert_eq!(
            reg.binding(
                &pending.id,
                &pending.action_hash,
                &PendingOwnerProof {
                    session_id: "session".into(),
                    turn_request_id: "request".into(),
                    turn_id: "turn".into(),
                },
                2_001,
            ),
            PendingBindingOutcome::Rejected(ClosedConfirmationCode::Replayed)
        );
        assert_eq!(
            reg.confirm(
                &pending.id,
                &token,
                &pending.action_hash,
                &PendingOwnerProof {
                    session_id: "session".into(),
                    turn_request_id: "request".into(),
                    turn_id: "turn".into(),
                },
                2_001,
            ),
            PendingConfirmOutcome::Rejected(ClosedConfirmationCode::Replayed)
        );
    }

    #[test]
    fn pending_replay_reissues_for_exact_owner_and_invalidates_old_token() {
        let reg = PendingRegistry::new();
        let owner = PendingOwnerBinding {
            account: "me".into(),
            session_id: "session".into(),
            request_id: "request".into(),
            turn_id: "turn".into(),
        };
        let (pending, old_token) = reg
            .register_bound(backup(), "p", 1_000, 60_000, owner.clone())
            .unwrap();

        let (reissued, new_token) = reg
            .reissue_for_owner(&owner, 2_000)
            .unwrap()
            .expect("durable pending replay");
        assert_eq!(reissued.id, pending.id);
        assert_eq!(reissued.action_hash, pending.action_hash);
        assert_ne!(new_token, old_token);
        assert_eq!(
            reg.confirm(
                &pending.id,
                &old_token,
                &pending.action_hash,
                &proof_for(&owner),
                2_001,
            ),
            PendingConfirmOutcome::Rejected(ClosedConfirmationCode::Invalid)
        );
        assert!(matches!(
            reg.confirm(
                &pending.id,
                &new_token,
                &pending.action_hash,
                &proof_for(&owner),
                2_002,
            ),
            PendingConfirmOutcome::Confirmed(_)
        ));
    }

    #[test]
    fn pending_replay_rejects_wrong_owner_and_exact_expiry() {
        let reg = PendingRegistry::new();
        let owner = PendingOwnerBinding {
            account: "me".into(),
            session_id: "session".into(),
            request_id: "request".into(),
            turn_id: "turn".into(),
        };
        reg.register_bound(backup(), "p", 1_000, 60_000, owner.clone())
            .unwrap();
        let mut wrong_owner = owner.clone();
        wrong_owner.request_id = "other-request".into();
        assert!(reg
            .reissue_for_owner(&wrong_owner, 2_000)
            .unwrap()
            .is_none());
        assert!(reg.reissue_for_owner(&owner, 61_000).unwrap().is_none());
        assert!(!reg.has_pending_for_turn("turn", 61_000).unwrap());
    }

    #[test]
    fn pending_registration_rejects_read_class_and_ttl_overflow() {
        let reg = PendingRegistry::new();
        let read = crate::tool::parse_action(
            &json!({"op":"read","account":"me","service":"mail","id":"item"}),
        )
        .unwrap();
        let owner = PendingOwnerBinding {
            account: "me".into(),
            session_id: "session".into(),
            request_id: "00000000-0000-4000-8000-000000000000".into(),
            turn_id: "turn".into(),
        };
        assert!(reg
            .register_bound(read, "read", 1_000, 60_000, owner.clone())
            .unwrap_err()
            .to_string()
            .contains("not_confirmable"));
        assert!(reg
            .register_bound(backup(), "backup", 1_000, 120_001, owner.clone())
            .unwrap_err()
            .to_string()
            .contains("pending_registration_unavailable"));
        assert!(reg
            .register_bound(backup(), "backup", u64::MAX - 1, 2, owner)
            .unwrap_err()
            .to_string()
            .contains("pending_registration_unavailable"));
        assert!(reg.is_empty());
    }

    #[test]
    fn pending_registration_rejects_incomplete_owner_before_rng_or_persistence() {
        let reg = PendingRegistry::new();
        let valid = PendingOwnerBinding {
            account: "me".into(),
            session_id: "session".into(),
            request_id: "00000000-0000-4000-8000-000000000000".into(),
            turn_id: "turn".into(),
        };
        for field in ["account", "session", "request", "turn"] {
            let mut owner = valid.clone();
            match field {
                "account" => owner.account.clear(),
                "session" => owner.session_id.clear(),
                "request" => owner.request_id.clear(),
                "turn" => owner.turn_id.clear(),
                _ => unreachable!(),
            }
            assert!(reg
                .register_bound(backup(), "backup", 1_000, 60_000, owner)
                .is_err());
        }
        assert!(reg.is_empty());
    }
}
