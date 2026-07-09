//! Cross-device, conflict-safe, encrypted agent session (REQ-AGENT-006).
//!
//! A session is a set of **per-turn ULID files** under a transport
//! (`/Apps/iSyncYou/agent/<session>/<ulid>.json` on OneDrive). Each turn is encrypted
//! with the pairing secret ([`crate::session_crypto`]). The transport is abstracted by
//! [`SessionTransport`] so the model (append/load/sort/lease/fork/offline-sync) is
//! tested over an in-memory fake; [`OneDriveTransport`] (feature `onedrive`) is the real
//! one. An **active-turn lease** prevents forks; **fork detection** is the fallback.

use crate::session_crypto::{self, SealedTurn, SessionCryptoConfig, SessionKey};
use crate::session_ids::{DeviceId, SessionId, TurnId};
use crate::AgentError;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// One conversation turn (the plaintext that gets sealed per file).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub ulid: String,
    pub role: String,
    pub content: String,
    /// The head this turn was authored against (for fork detection).
    pub observed_head: Option<String>,
    pub parent_turn_ids: Vec<String>,
    pub ts_ms: u64,
}

// ----- ULID (timestamp-ordered, Crockford base32) -----

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

fn encode_time(ms: u64) -> [u8; 10] {
    let mut out = [0u8; 10];
    let mut ts = ms;
    for i in (0..10).rev() {
        out[i] = ALPHABET[(ts & 0x1f) as usize];
        ts >>= 5;
    }
    out
}

fn encode_rand(r: &[u8; 10]) -> [u8; 16] {
    let mut bits: u128 = 0;
    for &b in r.iter() {
        bits = (bits << 8) | b as u128; // 80 bits of randomness
    }
    let mut out = [0u8; 16];
    for i in (0..16).rev() {
        out[i] = ALPHABET[(bits & 0x1f) as usize];
        bits >>= 5;
    }
    out
}

/// A fresh ULID: 48-bit ms timestamp + 80-bit randomness, 26 Crockford chars. Sorts
/// lexicographically in creation order.
fn new_turn_id() -> Result<TurnId, AgentError> {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| AgentError::Provider(e.to_string()))?
        .as_millis() as u64;
    let mut r = [0u8; 10];
    SystemRandom::new()
        .fill(&mut r)
        .map_err(|_| AgentError::Provider("ulid rng".into()))?;
    let t = encode_time(ms);
    let rr = encode_rand(&r);
    let mut s = String::with_capacity(26);
    // SAFETY: ALPHABET is ASCII, so both arrays are valid UTF-8.
    s.push_str(std::str::from_utf8(&t).expect("ascii"));
    s.push_str(std::str::from_utf8(&rr).expect("ascii"));
    TurnId::new(s)
}

pub fn new_ulid() -> Result<String, AgentError> {
    Ok(new_turn_id()?.into_string())
}

/// Increment a 26-char Crockford ULID by 1 (with carry). Used for ULID **monotonicity**:
/// when two turns are created in the same millisecond, the next is derived from the last
/// so ordering still equals creation order.
fn increment_turn_id(id: &TurnId) -> Result<TurnId, AgentError> {
    let mut chars: Vec<u8> = id.as_str().bytes().collect();
    for i in (0..chars.len()).rev() {
        let v = ALPHABET
            .iter()
            .position(|&a| a == chars[i])
            .ok_or_else(|| AgentError::Provider("bad ulid char".into()))?;
        if v == 31 {
            chars[i] = ALPHABET[0]; // carry
        } else {
            chars[i] = ALPHABET[v + 1];
            let next = String::from_utf8(chars).map_err(|e| AgentError::Provider(e.to_string()))?;
            return TurnId::new(next);
        }
    }
    Err(AgentError::Provider("ulid overflow".into()))
}

#[cfg(any(test, feature = "onedrive"))]
const ONEDRIVE_AGENT_PREFIX: &str = "Apps/iSyncYou/agent";

#[cfg(any(test, feature = "onedrive"))]
fn onedrive_session_dir(session_id: &SessionId) -> String {
    format!("{ONEDRIVE_AGENT_PREFIX}/{}", session_id.as_str())
}

#[cfg(any(test, feature = "onedrive"))]
fn onedrive_turn_file(session_id: &SessionId, turn_id: &TurnId) -> String {
    format!(
        "{}/{}.json",
        onedrive_session_dir(session_id),
        turn_id.as_str()
    )
}

#[cfg(any(test, feature = "onedrive"))]
fn onedrive_lease_file(session_id: &SessionId) -> String {
    format!("{}/.lease", onedrive_session_dir(session_id))
}

// ----- transport -----

/// Storage for per-turn files + the active-turn lease. Account/session scoping is by
/// `session_id`. Implemented over OneDrive (feature `onedrive`) and an in-memory fake.
pub trait SessionTransport {
    fn put(&self, session_id: &SessionId, turn_id: &TurnId, bytes: &[u8])
        -> Result<(), AgentError>;
    fn get(&self, session_id: &SessionId, turn_id: &TurnId) -> Result<Vec<u8>, AgentError>;
    /// ULIDs present for the session.
    fn list(&self, session_id: &SessionId) -> Result<Vec<TurnId>, AgentError>;
    /// Try to acquire the single active-turn lease; `true` if acquired.
    fn acquire_lease(&self, session_id: &SessionId, holder: &DeviceId) -> Result<bool, AgentError>;
    fn release_lease(&self, session_id: &SessionId, holder: &DeviceId) -> Result<(), AgentError>;
    fn current_lease(&self, session_id: &SessionId) -> Result<Option<DeviceId>, AgentError>;
}

// ----- session -----

/// Detect a fork: two or more turns authored against the same `observed_head`.
/// Returns the conflicting ULIDs (empty = no fork). The active-turn lease is the
/// primary anti-fork mechanism; this is the fallback when one is forced.
pub fn detect_fork(turns: &[Turn]) -> Vec<String> {
    use std::collections::HashMap;
    let mut by_head: HashMap<&Option<String>, Vec<&str>> = HashMap::new();
    for t in turns {
        by_head.entry(&t.observed_head).or_default().push(&t.ulid);
    }
    let mut conflicts: Vec<String> = by_head
        .into_iter()
        .filter(|(head, kids)| head.is_some() && kids.len() > 1)
        .flat_map(|(_, kids)| kids.into_iter().map(|s| s.to_string()))
        .collect();
    conflicts.sort();
    conflicts
}

/// An encrypted, conflict-safe session over a [`SessionTransport`].
pub struct Session<T: SessionTransport> {
    pub session_id: SessionId,
    crypto_config: SessionCryptoConfig,
    session_key: SessionKey,
    transport: T,
    /// Sealed turns not yet confirmed on the transport (offline cache).
    pending: std::cell::RefCell<Vec<SealedTurn>>,
    head: std::cell::RefCell<Option<TurnId>>,
}

impl<T: SessionTransport> Session<T> {
    pub fn new(
        session_id: impl AsRef<str>,
        pairing_secret: Vec<u8>,
        transport: T,
    ) -> Result<Self, AgentError> {
        let crypto_config = SessionCryptoConfig::generate_default()?;
        Self::new_with_crypto_config(session_id, pairing_secret, transport, crypto_config)
    }

    pub fn new_with_crypto_config(
        session_id: impl AsRef<str>,
        pairing_secret: Vec<u8>,
        transport: T,
        crypto_config: SessionCryptoConfig,
    ) -> Result<Self, AgentError> {
        let session_key = SessionKey::derive(&pairing_secret, &crypto_config)?;
        Ok(Self {
            session_id: SessionId::new(session_id.as_ref())?,
            crypto_config,
            session_key,
            transport,
            pending: std::cell::RefCell::new(Vec::new()),
            head: std::cell::RefCell::new(None),
        })
    }

    /// Append a turn: seal it, write it to the transport (or keep it pending if offline),
    /// and advance the local head.
    pub fn append(&self, role: &str, content: &str) -> Result<Turn, AgentError> {
        let observed_head = self.head.borrow().clone();
        // Monotonic ULID: strictly increasing even within one millisecond, so load order
        // equals append order.
        let mut turn_id = new_turn_id()?;
        if let Some(last) = observed_head.as_ref() {
            if turn_id <= *last {
                turn_id = increment_turn_id(last)?;
            }
        }
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| AgentError::Provider(e.to_string()))?
            .as_millis() as u64;
        let turn = Turn {
            ulid: turn_id.to_string(),
            role: role.to_string(),
            content: content.to_string(),
            observed_head: observed_head.as_ref().map(ToString::to_string),
            parent_turn_ids: observed_head.into_iter().map(|id| id.to_string()).collect(),
            ts_ms,
        };
        let plaintext =
            serde_json::to_vec(&turn).map_err(|e| AgentError::Provider(e.to_string()))?;
        let sealed = session_crypto::seal(
            &self.session_key,
            &self.crypto_config,
            &self.session_id,
            &turn_id,
            &plaintext,
        )?;
        let bytes = serde_json::to_vec(&sealed).map_err(|e| AgentError::Provider(e.to_string()))?;
        if self
            .transport
            .put(&self.session_id, &turn_id, &bytes)
            .is_err()
        {
            // Offline: keep locally and sync later (idempotent by ULID).
            self.pending.borrow_mut().push(sealed);
        }
        *self.head.borrow_mut() = Some(turn_id);
        Ok(turn)
    }

    /// Upload any pending (offline-written) turns that the transport does not yet have.
    /// Idempotent: ULIDs already present are skipped. Returns how many were uploaded.
    pub fn sync(&self) -> Result<usize, AgentError> {
        let present: std::collections::HashSet<TurnId> =
            self.transport.list(&self.session_id)?.into_iter().collect();
        let mut uploaded = 0;
        let mut still_pending = Vec::new();
        for sealed in self.pending.borrow().iter() {
            let turn_id = TurnId::new(&sealed.ulid)?;
            if present.contains(&turn_id) {
                continue; // already there — no duplicate
            }
            let bytes =
                serde_json::to_vec(sealed).map_err(|e| AgentError::Provider(e.to_string()))?;
            match self.transport.put(&self.session_id, &turn_id, &bytes) {
                Ok(()) => uploaded += 1,
                Err(_) => still_pending.push(sealed.clone()),
            }
        }
        *self.pending.borrow_mut() = still_pending;
        Ok(uploaded)
    }

    /// Load the whole conversation, decrypted and sorted by ULID (= creation order).
    /// Merges transport files with any still-pending local turns.
    pub fn load(&self) -> Result<Vec<Turn>, AgentError> {
        use std::collections::BTreeMap;
        let mut sealed_by_ulid: BTreeMap<TurnId, SealedTurn> = BTreeMap::new();
        for turn_id in self.transport.list(&self.session_id)? {
            let bytes = self.transport.get(&self.session_id, &turn_id)?;
            let sealed: SealedTurn =
                serde_json::from_slice(&bytes).map_err(|e| AgentError::Provider(e.to_string()))?;
            if sealed.ulid != turn_id.as_str() {
                return Err(AgentError::Provider(format!(
                    "turn id mismatch: file {} envelope {}",
                    turn_id, sealed.ulid
                )));
            }
            sealed_by_ulid.insert(turn_id, sealed);
        }
        for sealed in self.pending.borrow().iter() {
            let turn_id = TurnId::new(&sealed.ulid)?;
            sealed_by_ulid
                .entry(turn_id)
                .or_insert_with(|| sealed.clone());
        }
        let mut turns = Vec::with_capacity(sealed_by_ulid.len());
        for (_ulid, sealed) in sealed_by_ulid {
            let plaintext = session_crypto::open(&self.session_key, &self.crypto_config, &sealed)?;
            let turn: Turn = serde_json::from_slice(&plaintext)
                .map_err(|e| AgentError::Provider(e.to_string()))?;
            turns.push(turn);
        }
        Ok(turns) // BTreeMap iterates keys (ULIDs) in sorted order
    }

    /// Acquire the active-turn lease (anti-fork) for `holder`.
    pub fn begin_turn(&self, holder: &DeviceId) -> Result<bool, AgentError> {
        self.transport.acquire_lease(&self.session_id, holder)
    }

    /// Release the active-turn lease.
    pub fn end_turn(&self, holder: &DeviceId) -> Result<(), AgentError> {
        self.transport.release_lease(&self.session_id, holder)
    }
}

// ----- in-memory transport (tests + offline simulation) -----

/// In-memory [`SessionTransport`] with an `offline` switch, for tests.
#[derive(Default)]
pub struct InMemoryTransport {
    files: std::cell::RefCell<std::collections::HashMap<(SessionId, TurnId), Vec<u8>>>,
    lease: std::cell::RefCell<std::collections::HashMap<SessionId, DeviceId>>,
    offline: std::cell::Cell<bool>,
}

impl InMemoryTransport {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set_offline(&self, off: bool) {
        self.offline.set(off);
    }
    fn guard(&self) -> Result<(), AgentError> {
        if self.offline.get() {
            Err(AgentError::Transport("offline".into()))
        } else {
            Ok(())
        }
    }
    /// Raw stored bytes for a ULID (test inspection).
    pub fn raw(&self, session_id: &SessionId, turn_id: &TurnId) -> Option<Vec<u8>> {
        self.files
            .borrow()
            .get(&(session_id.clone(), turn_id.clone()))
            .cloned()
    }
}

impl SessionTransport for InMemoryTransport {
    fn put(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
        bytes: &[u8],
    ) -> Result<(), AgentError> {
        self.guard()?;
        self.files
            .borrow_mut()
            .insert((session_id.clone(), turn_id.clone()), bytes.to_vec());
        Ok(())
    }
    fn get(&self, session_id: &SessionId, turn_id: &TurnId) -> Result<Vec<u8>, AgentError> {
        self.guard()?;
        self.files
            .borrow()
            .get(&(session_id.clone(), turn_id.clone()))
            .cloned()
            .ok_or_else(|| AgentError::Provider(format!("no turn {session_id}/{turn_id}")))
    }
    fn list(&self, session_id: &SessionId) -> Result<Vec<TurnId>, AgentError> {
        self.guard()?;
        Ok(self
            .files
            .borrow()
            .keys()
            .filter(|(s, _)| s == session_id)
            .map(|(_, u)| u.clone())
            .collect())
    }
    fn acquire_lease(&self, session_id: &SessionId, holder: &DeviceId) -> Result<bool, AgentError> {
        self.guard()?;
        let mut lease = self.lease.borrow_mut();
        match lease.get(session_id) {
            Some(h) if h != holder => Ok(false),
            _ => {
                lease.insert(session_id.clone(), holder.clone());
                Ok(true)
            }
        }
    }
    fn release_lease(&self, session_id: &SessionId, holder: &DeviceId) -> Result<(), AgentError> {
        let mut lease = self.lease.borrow_mut();
        if lease.get(session_id).map(|h| h == holder).unwrap_or(false) {
            lease.remove(session_id);
        }
        Ok(())
    }
    fn current_lease(&self, session_id: &SessionId) -> Result<Option<DeviceId>, AgentError> {
        Ok(self.lease.borrow().get(session_id).cloned())
    }
}

// ----- OneDrive transport (real, feature-gated) -----

#[cfg(feature = "onedrive")]
mod onedrive {
    use super::{
        onedrive_lease_file, onedrive_session_dir, onedrive_turn_file, DeviceId, SessionId,
        SessionTransport, TurnId,
    };
    use crate::AgentError;
    use isyncyou_graph::http::GraphClient;

    /// Real OneDrive transport. The caller supplies a pre-resolved `Files.ReadWrite`
    /// token (e.g. via `engine::resolve_cached_sync_token`), so this needs no engine dep.
    pub struct OneDriveTransport {
        client: GraphClient,
    }

    impl OneDriveTransport {
        pub fn new(token: impl Into<String>) -> Self {
            Self {
                client: GraphClient::new(token),
            }
        }
    }

    impl SessionTransport for OneDriveTransport {
        fn put(
            &self,
            session_id: &SessionId,
            turn_id: &TurnId,
            bytes: &[u8],
        ) -> Result<(), AgentError> {
            self.client
                .simple_upload(&onedrive_turn_file(session_id, turn_id), bytes)
                .map(|_| ())
                .map_err(|e| AgentError::Transport(e.to_string()))
        }
        fn get(&self, session_id: &SessionId, turn_id: &TurnId) -> Result<Vec<u8>, AgentError> {
            let url = format!(
                "/me/drive/root:/{}:/content",
                onedrive_turn_file(session_id, turn_id)
            );
            self.client
                .get_bytes(&url)
                .map_err(|e| AgentError::Transport(e.to_string()))
        }
        fn list(&self, session_id: &SessionId) -> Result<Vec<TurnId>, AgentError> {
            let url = format!(
                "/me/drive/root:/{}:/children?$select=name",
                onedrive_session_dir(session_id)
            );
            let v = self
                .client
                .get_json(&url)
                .map_err(|e| AgentError::Transport(e.to_string()))?;
            let mut ulids = Vec::new();
            if let Some(items) = v.get("value").and_then(|x| x.as_array()) {
                for it in items {
                    if let Some(name) = it.get("name").and_then(|n| n.as_str()) {
                        if let Some(ulid) = name.strip_suffix(".json") {
                            ulids.push(TurnId::new(ulid)?);
                        }
                    }
                }
            }
            Ok(ulids)
        }
        fn acquire_lease(
            &self,
            session_id: &SessionId,
            holder: &DeviceId,
        ) -> Result<bool, AgentError> {
            // Best-effort lease via a marker file. (A stronger ETag/If-Match lease is a
            // follow-up; the per-turn ULID files keep storage conflict-free regardless.)
            self.client
                .simple_upload(&onedrive_lease_file(session_id), holder.as_str().as_bytes())
                .map(|_| true)
                .map_err(|e| AgentError::Transport(e.to_string()))
        }
        fn release_lease(
            &self,
            _session_id: &SessionId,
            _holder: &DeviceId,
        ) -> Result<(), AgentError> {
            Ok(())
        }
        fn current_lease(&self, session_id: &SessionId) -> Result<Option<DeviceId>, AgentError> {
            let url = format!(
                "/me/drive/root:/{}:/content",
                onedrive_lease_file(session_id)
            );
            match self.client.get_bytes(&url) {
                Ok(b) => Ok(Some(DeviceId::new(String::from_utf8_lossy(&b))?)),
                Err(_) => Ok(None),
            }
        }
    }
}

#[cfg(feature = "onedrive")]
pub use onedrive::OneDriveTransport;

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"a-high-entropy-pairing-secret-32b";
    const TURN_A: &str = "0000000000000000000000000A";
    const TURN_B: &str = "0000000000000000000000000B";
    const TURN_C: &str = "0000000000000000000000000C";

    fn sid(value: &str) -> SessionId {
        SessionId::new(value).unwrap()
    }

    fn tid(value: &str) -> TurnId {
        TurnId::new(value).unwrap()
    }

    fn did(value: &str) -> DeviceId {
        DeviceId::new(value).unwrap()
    }

    fn crypto_config() -> SessionCryptoConfig {
        SessionCryptoConfig::new(session_crypto::KdfProfile::production(*b"0123456789ABCDEF"))
            .unwrap()
    }

    fn crypto_key(config: &SessionCryptoConfig) -> SessionKey {
        SessionKey::derive(KEY, config).unwrap()
    }

    #[test]
    fn ulid_is_26_chars_and_time_ordered() {
        assert_eq!(new_ulid().unwrap().len(), 26);
        // Encoded timestamps preserve order lexicographically.
        let a = String::from_utf8(encode_time(1_000).to_vec()).unwrap();
        let b = String::from_utf8(encode_time(2_000).to_vec()).unwrap();
        assert!(a < b);
    }

    #[test]
    fn append_then_load_roundtrips() {
        let s = Session::new("sess1", KEY.to_vec(), InMemoryTransport::new()).unwrap();
        s.append("user", "find the spotify invoice").unwrap();
        s.append("assistant", "it is item-42").unwrap();
        let turns = s.load().unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[1].content, "it is item-42");
        // second turn observed the first as head
        assert_eq!(
            turns[1].observed_head.as_deref(),
            Some(turns[0].ulid.as_str())
        );
    }

    #[test]
    fn appends_are_strictly_monotonic_even_within_one_ms() {
        // 50 rapid appends almost certainly share a millisecond; monotonicity must hold.
        let s = Session::new("sess1", KEY.to_vec(), InMemoryTransport::new()).unwrap();
        let mut prev = String::new();
        for i in 0..50 {
            let t = s.append("user", &format!("turn {i}")).unwrap();
            assert!(
                t.ulid > prev,
                "ulid must strictly increase: {} !> {}",
                t.ulid,
                prev
            );
            prev = t.ulid;
        }
        let contents: Vec<String> = s.load().unwrap().into_iter().map(|t| t.content).collect();
        let expected: Vec<String> = (0..50).map(|i| format!("turn {i}")).collect();
        assert_eq!(contents, expected); // load order == append order
    }

    #[test]
    fn load_returns_turns_sorted_by_ulid() {
        let t = InMemoryTransport::new();
        let config = crypto_config();
        let key = crypto_key(&config);
        // Insert out of order; load must sort by ULID.
        for ulid in [TURN_C, TURN_A, TURN_B] {
            let turn = Turn {
                ulid: ulid.into(),
                role: "user".into(),
                content: format!("c-{ulid}"),
                observed_head: None,
                parent_turn_ids: vec![],
                ts_ms: 0,
            };
            let pt = serde_json::to_vec(&turn).unwrap();
            let sealed =
                session_crypto::seal(&key, &config, &sid("sess1"), &tid(ulid), &pt).unwrap();
            t.put(
                &sid("sess1"),
                &tid(ulid),
                &serde_json::to_vec(&sealed).unwrap(),
            )
            .unwrap();
        }
        let s = Session::new_with_crypto_config("sess1", KEY.to_vec(), t, config).unwrap();
        let ulids: Vec<String> = s.load().unwrap().into_iter().map(|t| t.ulid).collect();
        assert_eq!(ulids, vec![TURN_A, TURN_B, TURN_C]);
    }

    #[test]
    fn offline_writes_then_sync_is_idempotent() {
        // RcTransport lets the test toggle offline on the same transport the session uses.
        let t = std::rc::Rc::new(InMemoryTransport::new());
        let s = Session::new("sess1", KEY.to_vec(), RcTransport(t.clone())).unwrap();
        t.set_offline(true);
        s.append("user", "a").unwrap(); // queue locally
        s.append("assistant", "b").unwrap();
        t.set_offline(false);
        assert_eq!(s.sync().unwrap(), 2); // two uploaded
        assert_eq!(s.sync().unwrap(), 0); // idempotent — no duplicates
        assert_eq!(s.load().unwrap().len(), 2);
    }

    #[test]
    fn lease_prevents_concurrent_turns() {
        let s = Session::new("sess1", KEY.to_vec(), InMemoryTransport::new()).unwrap();
        assert!(s.begin_turn(&did("deviceA")).unwrap());
        assert!(!s.begin_turn(&did("deviceB")).unwrap()); // A holds it
        s.end_turn(&did("deviceA")).unwrap();
        assert!(s.begin_turn(&did("deviceB")).unwrap()); // now free
    }

    #[test]
    fn fork_detection_flags_two_children_of_one_head() {
        let mk = |ulid: &str, head: Option<&str>| Turn {
            ulid: ulid.into(),
            role: "user".into(),
            content: "x".into(),
            observed_head: head.map(|h| h.into()),
            parent_turn_ids: head.into_iter().map(|h| h.to_string()).collect(),
            ts_ms: 0,
        };
        let linear = vec![mk("01", None), mk("02", Some("01"))];
        assert!(detect_fork(&linear).is_empty());
        let forked = vec![mk("01", None), mk("02", Some("01")), mk("03", Some("01"))];
        assert_eq!(
            detect_fork(&forked),
            vec!["02".to_string(), "03".to_string()]
        );
    }

    #[test]
    fn only_ciphertext_is_stored_no_plaintext() {
        let t = std::rc::Rc::new(InMemoryTransport::new());
        let s = Session::new("sess1", KEY.to_vec(), RcTransport(t.clone())).unwrap();
        let turn = s.append("user", "VERY-SECRET-MAIL-CONTENT").unwrap();
        let raw = t.raw(&sid("sess1"), &tid(&turn.ulid)).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("VERY-SECRET-MAIL-CONTENT"));
    }

    #[test]
    fn session_rejects_unsafe_session_ids_before_path_use() {
        assert!(Session::new("../sess", KEY.to_vec(), InMemoryTransport::new()).is_err());
        assert!(Session::new("sess:one", KEY.to_vec(), InMemoryTransport::new()).is_err());
        assert!(Session::new("sess/one", KEY.to_vec(), InMemoryTransport::new()).is_err());
    }

    #[test]
    fn onedrive_session_paths_are_under_agent_prefix() {
        let session = sid("sess1");
        let turn = tid(TURN_A);
        assert_eq!(onedrive_session_dir(&session), "Apps/iSyncYou/agent/sess1");
        assert_eq!(
            onedrive_turn_file(&session, &turn),
            format!("Apps/iSyncYou/agent/sess1/{TURN_A}.json")
        );
        assert_eq!(
            onedrive_lease_file(&session),
            "Apps/iSyncYou/agent/sess1/.lease"
        );
        assert!(SessionId::new("../evil").is_err());
        assert!(TurnId::new("../evil").is_err());
    }

    #[test]
    fn load_rejects_turn_file_envelope_id_mismatch() {
        let transport = InMemoryTransport::new();
        let config = crypto_config();
        let key = crypto_key(&config);
        let turn = Turn {
            ulid: TURN_B.into(),
            role: "user".into(),
            content: "mismatch".into(),
            observed_head: None,
            parent_turn_ids: vec![],
            ts_ms: 0,
        };
        let plaintext = serde_json::to_vec(&turn).unwrap();
        let sealed =
            session_crypto::seal(&key, &config, &sid("sess1"), &tid(TURN_B), &plaintext).unwrap();
        transport
            .put(
                &sid("sess1"),
                &tid(TURN_A),
                &serde_json::to_vec(&sealed).unwrap(),
            )
            .unwrap();
        let session =
            Session::new_with_crypto_config("sess1", KEY.to_vec(), transport, config).unwrap();
        let err = session.load().unwrap_err().to_string();
        assert!(err.contains("turn id mismatch"), "{err}");
    }

    /// Test shim so a test can hold the transport AND give one to the session.
    struct RcTransport(std::rc::Rc<InMemoryTransport>);
    impl SessionTransport for RcTransport {
        fn put(&self, s: &SessionId, u: &TurnId, b: &[u8]) -> Result<(), AgentError> {
            self.0.put(s, u, b)
        }
        fn get(&self, s: &SessionId, u: &TurnId) -> Result<Vec<u8>, AgentError> {
            self.0.get(s, u)
        }
        fn list(&self, s: &SessionId) -> Result<Vec<TurnId>, AgentError> {
            self.0.list(s)
        }
        fn acquire_lease(&self, s: &SessionId, h: &DeviceId) -> Result<bool, AgentError> {
            self.0.acquire_lease(s, h)
        }
        fn release_lease(&self, s: &SessionId, h: &DeviceId) -> Result<(), AgentError> {
            self.0.release_lease(s, h)
        }
        fn current_lease(&self, s: &SessionId) -> Result<Option<DeviceId>, AgentError> {
            self.0.current_lease(s)
        }
    }
}
