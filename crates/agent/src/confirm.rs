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
use ring::rand::{SecureRandom, SystemRandom};
use std::collections::HashMap;
use std::sync::Mutex;

/// A destructive action awaiting human confirmation. `id` + the (separately returned)
/// one-time token are what the UI confirms with; `preview` is the human-readable diff.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingAction {
    pub id: String,
    pub action: ToolAction,
    pub preview: String,
}

struct Pending {
    action: ToolAction,
    token: String,
    expires_at_ms: u64,
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
}

/// Registry of pending destructive actions, keyed by pending id.
#[derive(Default)]
pub struct PendingRegistry {
    inner: Mutex<HashMap<String, Pending>>,
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

impl PendingRegistry {
    pub fn new() -> Self {
        Self::default()
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
        let id = random_b64(16)?;
        let token = random_b64(32)?;
        self.inner.lock().unwrap().insert(
            id.clone(),
            Pending {
                action: action.clone(),
                token: token.clone(),
                expires_at_ms: now_ms.saturating_add(ttl_ms),
            },
        );
        Ok((
            PendingAction {
                id,
                action,
                preview: preview.into(),
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
        now_ms: u64,
    ) -> Result<ToolAction, ConfirmError> {
        let mut map = self.inner.lock().unwrap();
        let pending = map.get(pending_id).ok_or(ConfirmError::NotFound)?;
        if now_ms > pending.expires_at_ms {
            map.remove(pending_id);
            return Err(ConfirmError::Expired);
        }
        if !ct_eq(token.as_bytes(), pending.token.as_bytes()) {
            return Err(ConfirmError::BadToken); // not consumed — the legit user can retry
        }
        // Single-use: consume on success.
        Ok(map.remove(pending_id).expect("present").action)
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
    fn confirm_with_right_token_returns_action_and_is_single_use() {
        let reg = PendingRegistry::new();
        let (pending, token) = reg
            .register(backup(), "back up mail", 1_000, 60_000)
            .unwrap();
        let action = reg.confirm(&pending.id, &token, 2_000).unwrap();
        assert_eq!(action.op(), "backup");
        // replay → consumed
        assert_eq!(
            reg.confirm(&pending.id, &token, 2_001),
            Err(ConfirmError::NotFound)
        );
        assert!(reg.is_empty());
    }

    #[test]
    fn wrong_token_is_rejected_and_does_not_consume() {
        let reg = PendingRegistry::new();
        let (pending, token) = reg.register(backup(), "p", 0, 60_000).unwrap();
        assert_eq!(
            reg.confirm(&pending.id, "not-the-token", 1),
            Err(ConfirmError::BadToken)
        );
        // still confirmable with the real token afterwards
        assert!(reg.confirm(&pending.id, &token, 2).is_ok());
    }

    #[test]
    fn a_token_for_another_pending_is_rejected() {
        let reg = PendingRegistry::new();
        let (p1, _t1) = reg.register(backup(), "p1", 0, 60_000).unwrap();
        let (_p2, t2) = reg.register(backup(), "p2", 0, 60_000).unwrap();
        // t2 cannot confirm p1
        assert_eq!(reg.confirm(&p1.id, &t2, 1), Err(ConfirmError::BadToken));
    }

    #[test]
    fn expired_token_is_rejected() {
        let reg = PendingRegistry::new();
        let (pending, token) = reg.register(backup(), "p", 1_000, 5_000).unwrap();
        assert_eq!(
            reg.confirm(&pending.id, &token, 10_000),
            Err(ConfirmError::Expired)
        );
        assert!(reg.is_empty()); // swept
    }

    #[test]
    fn unknown_id_is_not_found() {
        let reg = PendingRegistry::new();
        assert_eq!(reg.confirm("nope", "x", 0), Err(ConfirmError::NotFound));
    }
}
