//! Per-action confirmation tokens for destructive mobile ops (#onedrive-mobile 0.6).
//!
//! Threat model (Bedrohung 2 — prompt-injection / a compromised WebView): on mobile
//! the WebView (and any in-app agent) already holds every capability token, because
//! they ride in `app.js`. So `cap_ok` alone cannot stop a manipulated UI from calling
//! a destructive route. This module adds a SECOND gate the UI cannot satisfy on its
//! own:
//!
//! 1. A destructive op with no token registers a [`PendingActionRegistry::register`]
//!    entry bound to a SHA-256 of exactly that action (op+account+service+item).
//! 2. The native side shows a `BiometricPrompt` and, on success, calls
//!    [`PendingActionRegistry::confirm_biometric`] over a JNI path the WebView can't
//!    reach — recording that a human was present.
//! 3. The op is re-issued with the pending id; [`PendingActionRegistry::consume`]
//!    authorizes it exactly once, and only for the action the hash pins. A token minted
//!    for "delete X" can never authorize "delete Y" (the hash won't match), it expires,
//!    and a replay fails (single-use).
//!
//! The registry is a pure, self-contained state machine (no I/O, clock injected) so the
//! security properties are exhaustively unit-tested.

use ring::digest::{digest, SHA256};
use ring::rand::{SecureRandom, SystemRandom};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// How long a registered pending action stays confirmable (mint → biometric →
/// re-issue). Long enough for a human biometric interaction, short enough to bound
/// the replay window if an id ever leaks.
pub const DEFAULT_TTL_MS: u64 = 120_000;

/// Milliseconds since the Unix epoch — the runtime clock for the registry.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The destructive / external op classes that require a biometric per-action
/// confirmation on mobile (risk-based gate catalogue, #onedrive-mobile 0.6):
/// delete, external share, upload/replace, backup, queued cloud restore, Agent
/// live-write, move OUT of a protected scope, a mode-switch that would pull a
/// large folder offline, a conflict resolve that deletes the cloud copy
/// (keep-mine), and bulk operations.
///
/// Read-only ops (search/read/list/export) are never gated. Free-up / download-now
/// are NOT gated (local-only, reversible: free-up just drops a re-downloadable copy)
/// — only the cloud-deleting keep-mine resolve is (#659).
pub fn requires_confirmation(op: &str) -> bool {
    matches!(
        op,
        "delete"
            | "share"
            | "external-share"
            | "backup"
            | "restore-cloud"
            | "live-write"
            | "upload"
            | "replace"
            | "move-out-of-protected"
            | "mode-switch-offline-large"
            | "conflict-keep-mine"
            | "bulk"
    )
}

/// A destructive action awaiting (or holding) a biometric confirmation.
struct Pending {
    action_hash: [u8; 32],
    expires_at_ms: u64,
    biometric_confirmed: bool,
}

/// Why consuming a per-action token failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumeError {
    /// Unknown id — never registered, already consumed (single-use), or expired+swept.
    NotFound,
    /// The TTL elapsed.
    Expired,
    /// No biometric confirmation has been recorded for this pending yet.
    NotConfirmed,
    /// The presented action does not hash to the one the token was minted for.
    /// The action-hash is immutable: a token binds exactly one action.
    HashMismatch,
}

/// SHA-256 over the length-prefixed action tuple. Length-prefixing removes field-
/// boundary ambiguity (so `("ab","c")` and `("a","bc")` hash differently).
pub fn action_hash(op: &str, account: &str, service: &str, item: &str) -> [u8; 32] {
    let mut buf = Vec::new();
    for f in [op, account, service, item] {
        buf.extend_from_slice(&(f.len() as u64).to_le_bytes());
        buf.extend_from_slice(f.as_bytes());
    }
    let d = digest(&SHA256, &buf);
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}

/// Constant-time byte-equality (XOR-accumulate, no short-circuit) so the hash check
/// can't be probed byte-by-byte via timing.
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

/// A 192-bit unguessable id, lower-case hex. `None` if the OS RNG fails.
fn random_hex(n_bytes: usize) -> Option<String> {
    let mut buf = vec![0u8; n_bytes];
    SystemRandom::new().fill(&mut buf).ok()?;
    let mut s = String::with_capacity(n_bytes * 2);
    for b in buf {
        s.push_str(&format!("{b:02x}"));
    }
    Some(s)
}

/// Registry of destructive actions awaiting (and after) biometric confirmation,
/// keyed by an unguessable pending id. The id itself is the one-time token: it is
/// single-use and cryptographically bound (via the action hash) to one action.
#[derive(Default)]
pub struct PendingActionRegistry {
    inner: Mutex<HashMap<String, Pending>>,
}

impl PendingActionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a destructive action and return its unguessable pending id (the
    /// one-time token). Not yet biometric-confirmed. `None` if the RNG fails.
    pub fn register(
        &self,
        op: &str,
        account: &str,
        service: &str,
        item: &str,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Option<String> {
        let id = random_hex(24)?;
        self.inner.lock().unwrap().insert(
            id.clone(),
            Pending {
                action_hash: action_hash(op, account, service, item),
                expires_at_ms: now_ms.saturating_add(ttl_ms),
                biometric_confirmed: false,
            },
        );
        Some(id)
    }

    /// Record a successful native biometric for `id`. Called ONLY from the native
    /// JNI path (never reachable from the WebView). Returns `false` for an unknown id
    /// or one whose TTL already elapsed (which is swept).
    pub fn confirm_biometric(&self, id: &str, now_ms: u64) -> bool {
        let mut map = self.inner.lock().unwrap();
        match map.get_mut(id) {
            Some(p) if now_ms <= p.expires_at_ms => {
                p.biometric_confirmed = true;
                true
            }
            Some(_) => {
                map.remove(id);
                false
            }
            None => false,
        }
    }

    /// Consume the token for exactly this action. Enforces, in order: the id exists,
    /// the TTL has not elapsed, a biometric was recorded, and the presented action
    /// hashes to the one the token was minted for. Single-use: on success (and on
    /// expiry) the entry is removed, so a replay returns [`ConsumeError::NotFound`].
    ///
    /// A wrong hash or a not-yet-confirmed token does NOT consume the entry, so a
    /// legitimate retry still works and an attacker probing a leaked id can never burn
    /// the real user's pending confirmation.
    pub fn consume(
        &self,
        id: &str,
        op: &str,
        account: &str,
        service: &str,
        item: &str,
        now_ms: u64,
    ) -> Result<(), ConsumeError> {
        let mut map = self.inner.lock().unwrap();
        let pending = map.get(id).ok_or(ConsumeError::NotFound)?;
        if now_ms > pending.expires_at_ms {
            map.remove(id);
            return Err(ConsumeError::Expired);
        }
        if !pending.biometric_confirmed {
            return Err(ConsumeError::NotConfirmed);
        }
        let want = action_hash(op, account, service, item);
        if !ct_eq(&want, &pending.action_hash) {
            return Err(ConsumeError::HashMismatch);
        }
        map.remove(id); // single-use
        Ok(())
    }

    /// Outstanding pending count (tests/metrics).
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

    // --- Gate catalogue (AC4) --------------------------------------------------
    #[test]
    fn gate_catalogue_covers_full_node_mobile_agent_ops() {
        for op in [
            "delete",
            "share",
            "external-share",
            "backup",
            "restore-cloud",
            "live-write",
            "upload",
            "replace",
            "move-out-of-protected",
            "mode-switch-offline-large",
            "conflict-keep-mine",
            "bulk",
        ] {
            assert!(requires_confirmation(op), "{op} must be gated");
        }
        // Read-only ops are never gated. free-up / download-now are local-only,
        // reversible -> never gated (#659).
        for op in [
            "read",
            "list",
            "search",
            "export",
            "move",
            "free-up",
            "download-now",
        ] {
            assert!(!requires_confirmation(op), "{op} must NOT be gated");
        }
    }

    // --- Single-use + expiry (AC1) --------------------------------------------
    #[test]
    fn confirmed_token_authorizes_once_then_replay_fails() {
        let reg = PendingActionRegistry::new();
        let id = reg
            .register("delete", "me", "calendar", "ev1", 1_000, 60_000)
            .unwrap();
        assert!(reg.confirm_biometric(&id, 2_000));
        assert_eq!(
            reg.consume(&id, "delete", "me", "calendar", "ev1", 3_000),
            Ok(())
        );
        // replay → consumed / gone
        assert_eq!(
            reg.consume(&id, "delete", "me", "calendar", "ev1", 3_001),
            Err(ConsumeError::NotFound)
        );
        assert!(reg.is_empty());
    }

    #[test]
    fn token_expires_after_ttl() {
        let reg = PendingActionRegistry::new();
        let id = reg
            .register("delete", "me", "contacts", "c1", 1_000, 5_000)
            .unwrap();
        assert!(reg.confirm_biometric(&id, 2_000));
        // now past expiry
        assert_eq!(
            reg.consume(&id, "delete", "me", "contacts", "c1", 10_000),
            Err(ConsumeError::Expired)
        );
        assert!(reg.is_empty()); // swept
    }

    #[test]
    fn biometric_confirm_past_ttl_is_rejected_and_swept() {
        let reg = PendingActionRegistry::new();
        let id = reg
            .register("delete", "me", "todo", "t1", 1_000, 5_000)
            .unwrap();
        assert!(!reg.confirm_biometric(&id, 10_000));
        assert!(reg.is_empty());
    }

    // --- Action-hash immutability (AC2) ---------------------------------------
    #[test]
    fn token_binds_exactly_one_action_hash_is_immutable() {
        let reg = PendingActionRegistry::new();
        let id = reg
            .register("delete", "me", "calendar", "ev1", 0, 60_000)
            .unwrap();
        assert!(reg.confirm_biometric(&id, 1));
        // a different item → hash mismatch, NOT consumed
        assert_eq!(
            reg.consume(&id, "delete", "me", "calendar", "ev2", 2),
            Err(ConsumeError::HashMismatch)
        );
        // a different op → hash mismatch
        assert_eq!(
            reg.consume(&id, "share", "me", "calendar", "ev1", 2),
            Err(ConsumeError::HashMismatch)
        );
        // a different service → hash mismatch
        assert_eq!(
            reg.consume(&id, "delete", "me", "contacts", "ev1", 2),
            Err(ConsumeError::HashMismatch)
        );
        // the exact action still works (mismatch did not burn the token)
        assert_eq!(
            reg.consume(&id, "delete", "me", "calendar", "ev1", 2),
            Ok(())
        );
    }

    // --- Biometric requirement ------------------------------------------------
    #[test]
    fn consume_without_biometric_is_rejected_but_retained() {
        let reg = PendingActionRegistry::new();
        let id = reg
            .register("delete", "me", "onenote", "p1", 0, 60_000)
            .unwrap();
        // no confirm_biometric yet
        assert_eq!(
            reg.consume(&id, "delete", "me", "onenote", "p1", 1),
            Err(ConsumeError::NotConfirmed)
        );
        // still there — a later biometric + consume succeeds
        assert!(reg.confirm_biometric(&id, 2));
        assert_eq!(reg.consume(&id, "delete", "me", "onenote", "p1", 3), Ok(()));
    }

    #[test]
    fn unknown_id_is_not_found() {
        let reg = PendingActionRegistry::new();
        assert_eq!(
            reg.consume("nope", "delete", "me", "calendar", "ev1", 0),
            Err(ConsumeError::NotFound)
        );
        assert!(!reg.confirm_biometric("nope", 0));
    }

    #[test]
    fn each_register_yields_a_distinct_unguessable_id() {
        let reg = PendingActionRegistry::new();
        let a = reg
            .register("delete", "me", "calendar", "ev1", 0, 60_000)
            .unwrap();
        let b = reg
            .register("delete", "me", "calendar", "ev1", 0, 60_000)
            .unwrap();
        assert_ne!(a, b, "ids must be random, not derived from the action");
        assert_eq!(a.len(), 48, "24 random bytes → 48 hex chars");
    }
}
