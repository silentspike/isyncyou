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
    ConfirmationOperation::parse(op).is_some()
}

/// Canonical operation names for the native prompt and the mobile authorization
/// catalogue. This enum is the single Rust source of truth; Kotlin only maps the
/// returned wire names to fixed Android string resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmationOperation {
    Delete,
    Share,
    ExternalShare,
    Backup,
    RestoreCloud,
    LiveWrite,
    Upload,
    Replace,
    MoveOutOfProtected,
    ModeSwitchOfflineLarge,
    ConflictKeepMine,
    Bulk,
    UserPresence,
}

impl ConfirmationOperation {
    pub const ALL: &'static [Self] = &[
        Self::Delete,
        Self::Share,
        Self::ExternalShare,
        Self::Backup,
        Self::RestoreCloud,
        Self::LiveWrite,
        Self::Upload,
        Self::Replace,
        Self::MoveOutOfProtected,
        Self::ModeSwitchOfflineLarge,
        Self::ConflictKeepMine,
        Self::Bulk,
        Self::UserPresence,
    ];

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "delete" => Self::Delete,
            "share" => Self::Share,
            "external-share" => Self::ExternalShare,
            "backup" => Self::Backup,
            "restore-cloud" => Self::RestoreCloud,
            "live-write" => Self::LiveWrite,
            "upload" => Self::Upload,
            "replace" => Self::Replace,
            "move-out-of-protected" => Self::MoveOutOfProtected,
            "mode-switch-offline-large" => Self::ModeSwitchOfflineLarge,
            "conflict-keep-mine" => Self::ConflictKeepMine,
            "bulk" => Self::Bulk,
            "user-presence" => Self::UserPresence,
            _ => return None,
        })
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Delete => "delete",
            Self::Share => "share",
            Self::ExternalShare => "external-share",
            Self::Backup => "backup",
            Self::RestoreCloud => "restore-cloud",
            Self::LiveWrite => "live-write",
            Self::Upload => "upload",
            Self::Replace => "replace",
            Self::MoveOutOfProtected => "move-out-of-protected",
            Self::ModeSwitchOfflineLarge => "mode-switch-offline-large",
            Self::ConflictKeepMine => "conflict-keep-mine",
            Self::Bulk => "bulk",
            Self::UserPresence => "user-presence",
        }
    }

    fn allows_service(self, service: ConfirmationService) -> bool {
        match self {
            Self::Delete => matches!(
                service,
                ConfirmationService::Calendar
                    | ConfirmationService::Contacts
                    | ConfirmationService::Todo
                    | ConfirmationService::Onenote
                    | ConfirmationService::Onedrive
            ),
            Self::Share | Self::ExternalShare => service == ConfirmationService::Onedrive,
            Self::Backup => matches!(
                service,
                ConfirmationService::Backup | ConfirmationService::Agent
            ),
            Self::RestoreCloud | Self::LiveWrite => matches!(
                service,
                ConfirmationService::Mail
                    | ConfirmationService::Calendar
                    | ConfirmationService::Contacts
                    | ConfirmationService::Todo
                    | ConfirmationService::Onenote
            ),
            Self::Upload
            | Self::Replace
            | Self::MoveOutOfProtected
            | Self::ModeSwitchOfflineLarge
            | Self::ConflictKeepMine => service == ConfirmationService::Onedrive,
            Self::Bulk => matches!(
                service,
                ConfirmationService::Onedrive | ConfirmationService::Todo
            ),
            Self::UserPresence => service == ConfirmationService::Agent,
        }
    }
}

/// Canonical service names accepted by pending-action registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmationService {
    Mail,
    Calendar,
    Contacts,
    Todo,
    Onenote,
    Onedrive,
    Backup,
    Agent,
}

impl ConfirmationService {
    pub const ALL: &'static [Self] = &[
        Self::Mail,
        Self::Calendar,
        Self::Contacts,
        Self::Todo,
        Self::Onenote,
        Self::Onedrive,
        Self::Backup,
        Self::Agent,
    ];

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "mail" => Self::Mail,
            "calendar" => Self::Calendar,
            "contacts" => Self::Contacts,
            "todo" => Self::Todo,
            "onenote" => Self::Onenote,
            "onedrive" => Self::Onedrive,
            "backup" => Self::Backup,
            "agent" => Self::Agent,
            _ => return None,
        })
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mail => "mail",
            Self::Calendar => "calendar",
            Self::Contacts => "contacts",
            Self::Todo => "todo",
            Self::Onenote => "onenote",
            Self::Onedrive => "onedrive",
            Self::Backup => "backup",
            Self::Agent => "agent",
        }
    }
}

/// Safe, bounded metadata returned to the trusted Android side for prompt text.
/// It intentionally contains no account, item, recipient, body, or share URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingActionDescriptor {
    pub op: ConfirmationOperation,
    pub service: ConfirmationService,
}

impl PendingActionDescriptor {
    pub fn parse(op: &str, service: &str) -> Result<Self, RegistrationError> {
        let op = ConfirmationOperation::parse(op).ok_or(RegistrationError::UnknownOperation)?;
        let service =
            ConfirmationService::parse(service).ok_or(RegistrationError::UnknownService)?;
        if !op.allows_service(service) {
            return Err(RegistrationError::InvalidServiceForOperation);
        }
        Ok(Self { op, service })
    }
}

/// Registration failures are deliberately typed so callers cannot accidentally
/// turn an invalid operation/service pair into a valid generic prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationError {
    UnknownOperation,
    UnknownService,
    InvalidServiceForOperation,
    RandomnessUnavailable,
}

/// Non-consuming descriptor lookup failures. Expired entries are removed when
/// observed so a stale handle cannot be reused after a later restart of the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescribeError {
    NotFound,
    Expired,
}

/// A destructive action awaiting (or holding) a biometric confirmation.
struct Pending {
    action_hash: [u8; 32],
    expires_at_ms: u64,
    biometric_confirmed: bool,
    descriptor: PendingActionDescriptor,
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
    ) -> Result<String, RegistrationError> {
        let descriptor = PendingActionDescriptor::parse(op, service)?;
        let id = random_hex(24).ok_or(RegistrationError::RandomnessUnavailable)?;
        self.inner.lock().unwrap().insert(
            id.clone(),
            Pending {
                action_hash: action_hash(op, account, service, item),
                expires_at_ms: now_ms.saturating_add(ttl_ms),
                biometric_confirmed: false,
                descriptor,
            },
        );
        Ok(id)
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

    /// Return only the fixed operation/service enums for native prompt rendering.
    /// This does not confirm or consume the action and never returns its hash or
    /// destructive payload.
    pub fn describe(
        &self,
        id: &str,
        now_ms: u64,
    ) -> Result<PendingActionDescriptor, DescribeError> {
        let mut map = self.inner.lock().unwrap();
        let pending = map.get(id).ok_or(DescribeError::NotFound)?;
        if now_ms > pending.expires_at_ms {
            map.remove(id);
            return Err(DescribeError::Expired);
        }
        Ok(pending.descriptor)
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

    #[test]
    fn descriptor_table_covers_every_confirmation_operation() {
        assert_eq!(ConfirmationOperation::ALL.len(), 13);
        for op in ConfirmationOperation::ALL {
            assert!(requires_confirmation(op.as_str()));
        }
        assert_eq!(ConfirmationService::ALL.len(), 8);
    }

    #[test]
    fn pending_registration_rejects_unknown_and_invalid_descriptors() {
        let reg = PendingActionRegistry::new();
        assert_eq!(
            reg.register("unknown", "me", "onedrive", "item", 0, DEFAULT_TTL_MS),
            Err(RegistrationError::UnknownOperation)
        );
        assert_eq!(
            reg.register("delete", "me", "unknown", "item", 0, DEFAULT_TTL_MS),
            Err(RegistrationError::UnknownService)
        );
        assert_eq!(
            reg.register("backup", "me", "onedrive", "item", 0, DEFAULT_TTL_MS),
            Err(RegistrationError::InvalidServiceForOperation)
        );
        assert_eq!(reg.len(), 0, "invalid descriptors must not insert a row");
    }

    #[test]
    fn descriptor_matrix_accepts_only_allowlisted_operation_service_pairs() {
        let valid = [
            ("delete", "calendar"),
            ("delete", "contacts"),
            ("delete", "todo"),
            ("delete", "onenote"),
            ("delete", "onedrive"),
            ("share", "onedrive"),
            ("external-share", "onedrive"),
            ("backup", "backup"),
            ("backup", "agent"),
            ("restore-cloud", "mail"),
            ("restore-cloud", "calendar"),
            ("restore-cloud", "contacts"),
            ("restore-cloud", "todo"),
            ("restore-cloud", "onenote"),
            ("live-write", "mail"),
            ("live-write", "calendar"),
            ("live-write", "contacts"),
            ("live-write", "todo"),
            ("live-write", "onenote"),
            ("upload", "onedrive"),
            ("replace", "onedrive"),
            ("move-out-of-protected", "onedrive"),
            ("mode-switch-offline-large", "onedrive"),
            ("conflict-keep-mine", "onedrive"),
            ("bulk", "onedrive"),
            ("bulk", "todo"),
            ("user-presence", "agent"),
        ];
        for op in ConfirmationOperation::ALL {
            for service in ConfirmationService::ALL {
                let expected = valid.contains(&(op.as_str(), service.as_str()));
                assert_eq!(
                    PendingActionDescriptor::parse(op.as_str(), service.as_str()).is_ok(),
                    expected,
                    "descriptor matrix mismatch for {}:{}",
                    op.as_str(),
                    service.as_str()
                );
            }
        }
    }

    #[test]
    fn descriptor_lookup_is_non_consuming_and_payload_free() {
        let reg = PendingActionRegistry::new();
        let id = reg
            .register(
                "backup",
                "me",
                "agent",
                "opaque-item",
                1_000,
                DEFAULT_TTL_MS,
            )
            .unwrap();
        assert_eq!(
            reg.describe(&id, 2_000),
            Ok(PendingActionDescriptor {
                op: ConfirmationOperation::Backup,
                service: ConfirmationService::Agent,
            })
        );
        assert_eq!(reg.len(), 1);
        assert!(reg.confirm_biometric(&id, 2_001));
        assert_eq!(
            reg.consume(&id, "backup", "me", "agent", "opaque-item", 2_002),
            Ok(())
        );
    }

    #[test]
    fn descriptor_lookup_expires_and_sweeps_stale_action() {
        let reg = PendingActionRegistry::new();
        let id = reg
            .register("delete", "me", "calendar", "event", 1_000, 5_000)
            .unwrap();
        assert_eq!(reg.describe(&id, 6_001), Err(DescribeError::Expired));
        assert_eq!(reg.describe(&id, 6_002), Err(DescribeError::NotFound));
        assert!(reg.is_empty());
    }

    #[test]
    fn descriptor_display_normalization_does_not_change_action_hash_tuple() {
        let reg = PendingActionRegistry::new();
        let id = reg
            .register("backup", "me", "agent", "me", 0, DEFAULT_TTL_MS)
            .unwrap();
        assert!(reg.confirm_biometric(&id, 1));
        assert_eq!(
            reg.consume(&id, "backup", "me", "backup", "me", 2),
            Err(ConsumeError::HashMismatch),
            "agent/backup display normalization must not rewrite the hashed service"
        );
        assert_eq!(reg.consume(&id, "backup", "me", "agent", "me", 3), Ok(()));
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
