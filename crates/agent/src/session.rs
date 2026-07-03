//! Cross-device, conflict-safe, encrypted agent session (REQ-AGENT-006).
//!
//! A session is a set of **per-turn ULID files** under a transport
//! (`/Apps/iSyncYou/agent/<session>/<ulid>.json` on OneDrive). Each turn is encrypted
//! with the pairing secret ([`crate::session_crypto`]). The transport is abstracted by
//! [`SessionTransport`] so the model (append/load/sort/lease/fork/offline-sync) is
//! tested over an in-memory fake; [`OneDriveTransport`] (feature `onedrive`) is the real
//! one. An **active-turn lease** prevents forks; **fork detection** is the fallback.

use crate::session_crypto::{self, SealedTurn};
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
pub fn new_ulid() -> Result<String, AgentError> {
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
    Ok(s)
}

/// Increment a 26-char Crockford ULID by 1 (with carry). Used for ULID **monotonicity**:
/// when two turns are created in the same millisecond, the next is derived from the last
/// so ordering still equals creation order.
fn increment_ulid(s: &str) -> Result<String, AgentError> {
    let mut chars: Vec<u8> = s.bytes().collect();
    for i in (0..chars.len()).rev() {
        let v = ALPHABET
            .iter()
            .position(|&a| a == chars[i])
            .ok_or_else(|| AgentError::Provider("bad ulid char".into()))?;
        if v == 31 {
            chars[i] = ALPHABET[0]; // carry
        } else {
            chars[i] = ALPHABET[v + 1];
            return String::from_utf8(chars).map_err(|e| AgentError::Provider(e.to_string()));
        }
    }
    Err(AgentError::Provider("ulid overflow".into()))
}

// ----- transport -----

/// Storage for per-turn files + the active-turn lease. Account/session scoping is by
/// `session_id`. Implemented over OneDrive (feature `onedrive`) and an in-memory fake.
pub trait SessionTransport {
    fn put(&self, session_id: &str, ulid: &str, bytes: &[u8]) -> Result<(), AgentError>;
    fn get(&self, session_id: &str, ulid: &str) -> Result<Vec<u8>, AgentError>;
    /// ULIDs present for the session.
    fn list(&self, session_id: &str) -> Result<Vec<String>, AgentError>;
    /// Try to acquire the single active-turn lease; `true` if acquired.
    fn acquire_lease(&self, session_id: &str, holder: &str) -> Result<bool, AgentError>;
    fn release_lease(&self, session_id: &str, holder: &str) -> Result<(), AgentError>;
    fn current_lease(&self, session_id: &str) -> Result<Option<String>, AgentError>;
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
    pub session_id: String,
    pairing_secret: Vec<u8>,
    transport: T,
    /// Sealed turns not yet confirmed on the transport (offline cache).
    pending: std::cell::RefCell<Vec<SealedTurn>>,
    head: std::cell::RefCell<Option<String>>,
}

impl<T: SessionTransport> Session<T> {
    pub fn new(session_id: impl Into<String>, pairing_secret: Vec<u8>, transport: T) -> Self {
        Self {
            session_id: session_id.into(),
            pairing_secret,
            transport,
            pending: std::cell::RefCell::new(Vec::new()),
            head: std::cell::RefCell::new(None),
        }
    }

    /// Append a turn: seal it, write it to the transport (or keep it pending if offline),
    /// and advance the local head.
    pub fn append(&self, role: &str, content: &str) -> Result<Turn, AgentError> {
        let observed_head = self.head.borrow().clone();
        // Monotonic ULID: strictly increasing even within one millisecond, so load order
        // equals append order.
        let mut ulid = new_ulid()?;
        if let Some(last) = observed_head.as_ref() {
            if ulid <= *last {
                ulid = increment_ulid(last)?;
            }
        }
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| AgentError::Provider(e.to_string()))?
            .as_millis() as u64;
        let turn = Turn {
            ulid: ulid.clone(),
            role: role.to_string(),
            content: content.to_string(),
            observed_head: observed_head.clone(),
            parent_turn_ids: observed_head.into_iter().collect(),
            ts_ms,
        };
        let plaintext =
            serde_json::to_vec(&turn).map_err(|e| AgentError::Provider(e.to_string()))?;
        let sealed =
            session_crypto::seal(&self.pairing_secret, &self.session_id, &ulid, &plaintext)?;
        let bytes = serde_json::to_vec(&sealed).map_err(|e| AgentError::Provider(e.to_string()))?;
        if self.transport.put(&self.session_id, &ulid, &bytes).is_err() {
            // Offline: keep locally and sync later (idempotent by ULID).
            self.pending.borrow_mut().push(sealed);
        }
        *self.head.borrow_mut() = Some(ulid);
        Ok(turn)
    }

    /// Upload any pending (offline-written) turns that the transport does not yet have.
    /// Idempotent: ULIDs already present are skipped. Returns how many were uploaded.
    pub fn sync(&self) -> Result<usize, AgentError> {
        let present: std::collections::HashSet<String> =
            self.transport.list(&self.session_id)?.into_iter().collect();
        let mut uploaded = 0;
        let mut still_pending = Vec::new();
        for sealed in self.pending.borrow().iter() {
            if present.contains(&sealed.ulid) {
                continue; // already there — no duplicate
            }
            let bytes =
                serde_json::to_vec(sealed).map_err(|e| AgentError::Provider(e.to_string()))?;
            match self.transport.put(&self.session_id, &sealed.ulid, &bytes) {
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
        let mut sealed_by_ulid: BTreeMap<String, SealedTurn> = BTreeMap::new();
        for ulid in self.transport.list(&self.session_id)? {
            let bytes = self.transport.get(&self.session_id, &ulid)?;
            let sealed: SealedTurn =
                serde_json::from_slice(&bytes).map_err(|e| AgentError::Provider(e.to_string()))?;
            sealed_by_ulid.insert(ulid, sealed);
        }
        for sealed in self.pending.borrow().iter() {
            sealed_by_ulid
                .entry(sealed.ulid.clone())
                .or_insert_with(|| sealed.clone());
        }
        let mut turns = Vec::with_capacity(sealed_by_ulid.len());
        for (_ulid, sealed) in sealed_by_ulid {
            let plaintext = session_crypto::open(&self.pairing_secret, &sealed)?;
            let turn: Turn = serde_json::from_slice(&plaintext)
                .map_err(|e| AgentError::Provider(e.to_string()))?;
            turns.push(turn);
        }
        Ok(turns) // BTreeMap iterates keys (ULIDs) in sorted order
    }

    /// Acquire the active-turn lease (anti-fork) for `holder`.
    pub fn begin_turn(&self, holder: &str) -> Result<bool, AgentError> {
        self.transport.acquire_lease(&self.session_id, holder)
    }

    /// Release the active-turn lease.
    pub fn end_turn(&self, holder: &str) -> Result<(), AgentError> {
        self.transport.release_lease(&self.session_id, holder)
    }
}

// ----- in-memory transport (tests + offline simulation) -----

/// In-memory [`SessionTransport`] with an `offline` switch, for tests.
#[derive(Default)]
pub struct InMemoryTransport {
    files: std::cell::RefCell<std::collections::HashMap<(String, String), Vec<u8>>>,
    lease: std::cell::RefCell<std::collections::HashMap<String, String>>,
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
    pub fn raw(&self, session_id: &str, ulid: &str) -> Option<Vec<u8>> {
        self.files
            .borrow()
            .get(&(session_id.to_string(), ulid.to_string()))
            .cloned()
    }
}

impl SessionTransport for InMemoryTransport {
    fn put(&self, session_id: &str, ulid: &str, bytes: &[u8]) -> Result<(), AgentError> {
        self.guard()?;
        self.files
            .borrow_mut()
            .insert((session_id.to_string(), ulid.to_string()), bytes.to_vec());
        Ok(())
    }
    fn get(&self, session_id: &str, ulid: &str) -> Result<Vec<u8>, AgentError> {
        self.guard()?;
        self.files
            .borrow()
            .get(&(session_id.to_string(), ulid.to_string()))
            .cloned()
            .ok_or_else(|| AgentError::Provider(format!("no turn {session_id}/{ulid}")))
    }
    fn list(&self, session_id: &str) -> Result<Vec<String>, AgentError> {
        self.guard()?;
        Ok(self
            .files
            .borrow()
            .keys()
            .filter(|(s, _)| s == session_id)
            .map(|(_, u)| u.clone())
            .collect())
    }
    fn acquire_lease(&self, session_id: &str, holder: &str) -> Result<bool, AgentError> {
        self.guard()?;
        let mut lease = self.lease.borrow_mut();
        match lease.get(session_id) {
            Some(h) if h != holder => Ok(false),
            _ => {
                lease.insert(session_id.to_string(), holder.to_string());
                Ok(true)
            }
        }
    }
    fn release_lease(&self, session_id: &str, holder: &str) -> Result<(), AgentError> {
        let mut lease = self.lease.borrow_mut();
        if lease.get(session_id).map(|h| h == holder).unwrap_or(false) {
            lease.remove(session_id);
        }
        Ok(())
    }
    fn current_lease(&self, session_id: &str) -> Result<Option<String>, AgentError> {
        Ok(self.lease.borrow().get(session_id).cloned())
    }
}

// ----- OneDrive transport (real, feature-gated) -----

#[cfg(feature = "onedrive")]
mod onedrive {
    use super::SessionTransport;
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
        fn dir(session_id: &str) -> String {
            format!("Apps/iSyncYou/agent/{session_id}")
        }
        fn file(session_id: &str, ulid: &str) -> String {
            format!("Apps/iSyncYou/agent/{session_id}/{ulid}.json")
        }
    }

    impl SessionTransport for OneDriveTransport {
        fn put(&self, session_id: &str, ulid: &str, bytes: &[u8]) -> Result<(), AgentError> {
            self.client
                .simple_upload(&Self::file(session_id, ulid), bytes)
                .map(|_| ())
                .map_err(|e| AgentError::Transport(e.to_string()))
        }
        fn get(&self, session_id: &str, ulid: &str) -> Result<Vec<u8>, AgentError> {
            let url = format!("/me/drive/root:/{}:/content", Self::file(session_id, ulid));
            self.client
                .get_bytes(&url)
                .map_err(|e| AgentError::Transport(e.to_string()))
        }
        fn list(&self, session_id: &str) -> Result<Vec<String>, AgentError> {
            let url = format!(
                "/me/drive/root:/{}:/children?$select=name",
                Self::dir(session_id)
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
                            ulids.push(ulid.to_string());
                        }
                    }
                }
            }
            Ok(ulids)
        }
        fn acquire_lease(&self, session_id: &str, holder: &str) -> Result<bool, AgentError> {
            // Best-effort lease via a marker file. (A stronger ETag/If-Match lease is a
            // follow-up; the per-turn ULID files keep storage conflict-free regardless.)
            let file = format!("Apps/iSyncYou/agent/{session_id}/.lease");
            self.client
                .simple_upload(&file, holder.as_bytes())
                .map(|_| true)
                .map_err(|e| AgentError::Transport(e.to_string()))
        }
        fn release_lease(&self, _session_id: &str, _holder: &str) -> Result<(), AgentError> {
            Ok(())
        }
        fn current_lease(&self, session_id: &str) -> Result<Option<String>, AgentError> {
            let url = format!("/me/drive/root:/Apps/iSyncYou/agent/{session_id}/.lease:/content");
            match self.client.get_bytes(&url) {
                Ok(b) => Ok(Some(String::from_utf8_lossy(&b).into_owned())),
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
        let s = Session::new("sess1", KEY.to_vec(), InMemoryTransport::new());
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
        let s = Session::new("sess1", KEY.to_vec(), InMemoryTransport::new());
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
        // Insert out of order; load must sort by ULID.
        for ulid in ["02BBB", "00AAA", "01ABC"] {
            let turn = Turn {
                ulid: ulid.into(),
                role: "user".into(),
                content: format!("c-{ulid}"),
                observed_head: None,
                parent_turn_ids: vec![],
                ts_ms: 0,
            };
            let pt = serde_json::to_vec(&turn).unwrap();
            let sealed = session_crypto::seal(KEY, "sess1", ulid, &pt).unwrap();
            t.put("sess1", ulid, &serde_json::to_vec(&sealed).unwrap())
                .unwrap();
        }
        let s = Session::new("sess1", KEY.to_vec(), t);
        let ulids: Vec<String> = s.load().unwrap().into_iter().map(|t| t.ulid).collect();
        assert_eq!(ulids, vec!["00AAA", "01ABC", "02BBB"]);
    }

    #[test]
    fn offline_writes_then_sync_is_idempotent() {
        // RcTransport lets the test toggle offline on the same transport the session uses.
        let t = std::rc::Rc::new(InMemoryTransport::new());
        let s = Session::new("sess1", KEY.to_vec(), RcTransport(t.clone()));
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
        let s = Session::new("sess1", KEY.to_vec(), InMemoryTransport::new());
        assert!(s.begin_turn("deviceA").unwrap());
        assert!(!s.begin_turn("deviceB").unwrap()); // A holds it
        s.end_turn("deviceA").unwrap();
        assert!(s.begin_turn("deviceB").unwrap()); // now free
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
        let s = Session::new("sess1", KEY.to_vec(), RcTransport(t.clone()));
        let turn = s.append("user", "VERY-SECRET-MAIL-CONTENT").unwrap();
        let raw = t.raw("sess1", &turn.ulid).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("VERY-SECRET-MAIL-CONTENT"));
    }

    /// Test shim so a test can hold the transport AND give one to the session.
    struct RcTransport(std::rc::Rc<InMemoryTransport>);
    impl SessionTransport for RcTransport {
        fn put(&self, s: &str, u: &str, b: &[u8]) -> Result<(), AgentError> {
            self.0.put(s, u, b)
        }
        fn get(&self, s: &str, u: &str) -> Result<Vec<u8>, AgentError> {
            self.0.get(s, u)
        }
        fn list(&self, s: &str) -> Result<Vec<String>, AgentError> {
            self.0.list(s)
        }
        fn acquire_lease(&self, s: &str, h: &str) -> Result<bool, AgentError> {
            self.0.acquire_lease(s, h)
        }
        fn release_lease(&self, s: &str, h: &str) -> Result<(), AgentError> {
            self.0.release_lease(s, h)
        }
        fn current_lease(&self, s: &str) -> Result<Option<String>, AgentError> {
            self.0.current_lease(s)
        }
    }
}
