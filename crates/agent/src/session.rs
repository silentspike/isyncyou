//! Cross-device, conflict-safe, encrypted agent session (REQ-AGENT-006).
//!
//! A session is a set of **per-turn ULID files** under a transport
//! (`/Apps/iSyncYou/agent/<session>/<ulid>.json` on OneDrive). Each turn is encrypted
//! with the pairing secret ([`crate::session_crypto`]). The transport is abstracted by
//! [`SessionTransport`] so the model (append/load/sort/lease/fork/offline-sync) is
//! tested over an in-memory fake; [`OneDriveTransport`] (feature `onedrive`) is the real
//! one. An **active-turn lease** prevents forks; **fork detection** is the fallback.

use crate::session_crypto::{self, SealedTurn, SessionCryptoConfig, SessionKey};
use crate::session_ids::{DeviceId, LeaseId, SessionId, TurnId};
use crate::AgentError;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const ACTIVE_TURN_LEASE_TTL_MS: u64 = 120_000;

/// One conversation turn (the plaintext that gets sealed per file).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub ulid: String,
    pub role: String,
    pub content: String,
    /// The head this turn was authored against (for fork detection).
    pub observed_head: Option<String>,
    pub parent_turn_ids: Vec<String>,
    pub lease_state: TurnLeaseState,
    pub ts_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TurnLeaseState {
    Active { device_id: String, lease_id: String },
    OfflineUnleased { device_id: String },
}

impl TurnLeaseState {
    fn active(device_id: &DeviceId, lease_id: &LeaseId) -> Self {
        Self::Active {
            device_id: device_id.to_string(),
            lease_id: lease_id.to_string(),
        }
    }

    fn offline_unleased(device_id: &DeviceId) -> Self {
        Self::OfflineUnleased {
            device_id: device_id.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseRecord {
    pub session_id: SessionId,
    pub holder_device_id: DeviceId,
    pub lease_id: LeaseId,
    pub observed_head: Option<TurnId>,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub cloud_item_id: Option<String>,
    pub cloud_etag: Option<String>,
}

pub struct ActiveTurn<'a, T: SessionTransport, C: LocalSessionCache = MemorySessionCache> {
    session: &'a Session<T, C>,
    lease: LeaseRecord,
    next_observed_head: Option<TurnId>,
    finished: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedSession {
    pub turns: Vec<Turn>,
    pub heads: Vec<TurnId>,
    pub fork: Option<SessionFork>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFork {
    pub heads: Vec<TurnId>,
    pub conflicting_turns: Vec<TurnId>,
    pub missing_parent_refs: Vec<TurnId>,
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

fn now_ms() -> Result<u64, AgentError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| AgentError::Provider(e.to_string()))?
        .as_millis() as u64)
}

fn new_lease_record(
    session_id: &SessionId,
    holder: &DeviceId,
    observed_head: Option<TurnId>,
    now_ms: u64,
    ttl_ms: u64,
) -> Result<LeaseRecord, AgentError> {
    Ok(LeaseRecord {
        session_id: session_id.clone(),
        holder_device_id: holder.clone(),
        lease_id: LeaseId::new(new_ulid()?)?,
        observed_head,
        created_at_ms: now_ms,
        expires_at_ms: now_ms.saturating_add(ttl_ms),
        cloud_item_id: None,
        cloud_etag: None,
    })
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
    format!(
        "{}/.active_turn_lease.json",
        onedrive_session_dir(session_id)
    )
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
    /// Try to acquire the single active-turn lease; `None` means another valid lease is busy.
    fn acquire_lease(
        &self,
        session_id: &SessionId,
        holder: &DeviceId,
        observed_head: Option<TurnId>,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<Option<LeaseRecord>, AgentError>;
    fn renew_lease(
        &self,
        lease: &LeaseRecord,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<Option<LeaseRecord>, AgentError>;
    fn release_lease(&self, lease: &LeaseRecord) -> Result<bool, AgentError>;
    fn current_lease(&self, session_id: &SessionId) -> Result<Option<LeaseRecord>, AgentError>;
}

pub trait LocalSessionCache {
    fn put_pending(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
        bytes: &[u8],
    ) -> Result<(), AgentError>;
    fn list_pending(&self, session_id: &SessionId) -> Result<Vec<TurnId>, AgentError>;
    fn get_pending(&self, session_id: &SessionId, turn_id: &TurnId) -> Result<Vec<u8>, AgentError>;
    fn remove_pending(&self, session_id: &SessionId, turn_id: &TurnId) -> Result<(), AgentError>;
}

#[derive(Default)]
pub struct MemorySessionCache {
    pending: std::cell::RefCell<std::collections::HashMap<(SessionId, TurnId), Vec<u8>>>,
}

impl MemorySessionCache {
    pub fn new() -> Self {
        Self::default()
    }
}

impl LocalSessionCache for MemorySessionCache {
    fn put_pending(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
        bytes: &[u8],
    ) -> Result<(), AgentError> {
        self.pending
            .borrow_mut()
            .insert((session_id.clone(), turn_id.clone()), bytes.to_vec());
        Ok(())
    }

    fn list_pending(&self, session_id: &SessionId) -> Result<Vec<TurnId>, AgentError> {
        Ok(self
            .pending
            .borrow()
            .keys()
            .filter(|(s, _)| s == session_id)
            .map(|(_, t)| t.clone())
            .collect())
    }

    fn get_pending(&self, session_id: &SessionId, turn_id: &TurnId) -> Result<Vec<u8>, AgentError> {
        self.pending
            .borrow()
            .get(&(session_id.clone(), turn_id.clone()))
            .cloned()
            .ok_or_else(|| AgentError::Provider(format!("no pending turn {session_id}/{turn_id}")))
    }

    fn remove_pending(&self, session_id: &SessionId, turn_id: &TurnId) -> Result<(), AgentError> {
        self.pending
            .borrow_mut()
            .remove(&(session_id.clone(), turn_id.clone()));
        Ok(())
    }
}

pub struct FileSessionCache {
    root: PathBuf,
}

impl FileSessionCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn pending_dir(&self, session_id: &SessionId) -> PathBuf {
        self.root.join(session_id.as_str()).join("pending")
    }

    fn pending_path(&self, session_id: &SessionId, turn_id: &TurnId) -> PathBuf {
        self.pending_dir(session_id)
            .join(format!("{}.json", turn_id.as_str()))
    }

    fn ensure_safe_parent(&self, session_id: &SessionId) -> Result<PathBuf, AgentError> {
        reject_symlink(&self.root)?;
        let session_dir = self.root.join(session_id.as_str());
        reject_symlink(&session_dir)?;
        let pending_dir = session_dir.join("pending");
        reject_symlink(&pending_dir)?;
        std::fs::create_dir_all(&pending_dir).map_err(|e| AgentError::Provider(e.to_string()))?;
        Ok(pending_dir)
    }
}

impl LocalSessionCache for FileSessionCache {
    fn put_pending(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
        bytes: &[u8],
    ) -> Result<(), AgentError> {
        let pending_dir = self.ensure_safe_parent(session_id)?;
        let final_path = self.pending_path(session_id, turn_id);
        let tmp_path = pending_dir.join(format!("{}.{}.tmp", turn_id.as_str(), new_ulid()?));
        std::fs::write(&tmp_path, bytes).map_err(|e| AgentError::Provider(e.to_string()))?;
        std::fs::rename(&tmp_path, &final_path).map_err(|e| AgentError::Provider(e.to_string()))?;
        Ok(())
    }

    fn list_pending(&self, session_id: &SessionId) -> Result<Vec<TurnId>, AgentError> {
        let pending_dir = self.pending_dir(session_id);
        reject_symlink(&pending_dir)?;
        if !pending_dir.exists() {
            return Ok(Vec::new());
        }
        let mut ids = Vec::new();
        for entry in
            std::fs::read_dir(&pending_dir).map_err(|e| AgentError::Provider(e.to_string()))?
        {
            let entry = entry.map_err(|e| AgentError::Provider(e.to_string()))?;
            if entry
                .file_type()
                .map_err(|e| AgentError::Provider(e.to_string()))?
                .is_file()
            {
                if let Some(name) = entry.file_name().to_str() {
                    if let Some(id) = name.strip_suffix(".json") {
                        ids.push(TurnId::new(id)?);
                    }
                }
            }
        }
        Ok(ids)
    }

    fn get_pending(&self, session_id: &SessionId, turn_id: &TurnId) -> Result<Vec<u8>, AgentError> {
        let path = self.pending_path(session_id, turn_id);
        reject_symlink(&path)?;
        std::fs::read(path).map_err(|e| AgentError::Provider(e.to_string()))
    }

    fn remove_pending(&self, session_id: &SessionId, turn_id: &TurnId) -> Result<(), AgentError> {
        let path = self.pending_path(session_id, turn_id);
        reject_symlink(&path)?;
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(AgentError::Provider(e.to_string())),
        }
    }
}

fn reject_symlink(path: &Path) -> Result<(), AgentError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(AgentError::Provider(format!(
            "session cache path is symlink: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(AgentError::Provider(e.to_string())),
    }
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

fn analyze_loaded_turns(turns: &[Turn]) -> Result<(Vec<TurnId>, Option<SessionFork>), AgentError> {
    use std::collections::{BTreeSet, HashMap, HashSet};

    let turn_ids: HashSet<String> = turns.iter().map(|turn| turn.ulid.clone()).collect();
    let mut referenced: HashSet<String> = HashSet::new();
    let mut missing_parent_refs: BTreeSet<TurnId> = BTreeSet::new();
    let mut missing_parent_children: BTreeSet<TurnId> = BTreeSet::new();
    for turn in turns {
        for parent in &turn.parent_turn_ids {
            referenced.insert(parent.clone());
            if !turn_ids.contains(parent) {
                missing_parent_refs.insert(TurnId::new(parent)?);
                missing_parent_children.insert(TurnId::new(&turn.ulid)?);
            }
        }
    }

    let mut heads: Vec<TurnId> = turns
        .iter()
        .filter(|turn| !referenced.contains(&turn.ulid))
        .map(|turn| TurnId::new(&turn.ulid))
        .collect::<Result<_, _>>()?;
    heads.sort();

    let mut by_observed_head: HashMap<Option<String>, Vec<TurnId>> = HashMap::new();
    for turn in turns {
        by_observed_head
            .entry(turn.observed_head.clone())
            .or_default()
            .push(TurnId::new(&turn.ulid)?);
    }
    let mut conflicting_turns: BTreeSet<TurnId> = BTreeSet::new();
    for (observed_head, children) in by_observed_head {
        if observed_head.is_some() && children.len() > 1 {
            conflicting_turns.extend(children);
        }
    }
    if heads.len() > 1 {
        conflicting_turns.extend(heads.iter().cloned());
    }
    conflicting_turns.extend(missing_parent_children);

    let fork =
        if heads.len() > 1 || !conflicting_turns.is_empty() || !missing_parent_refs.is_empty() {
            Some(SessionFork {
                heads: heads.clone(),
                conflicting_turns: conflicting_turns.into_iter().collect(),
                missing_parent_refs: missing_parent_refs.into_iter().collect(),
            })
        } else {
            None
        };

    Ok((heads, fork))
}

/// An encrypted, conflict-safe session over a [`SessionTransport`].
pub struct Session<T: SessionTransport, C: LocalSessionCache = MemorySessionCache> {
    pub session_id: SessionId,
    crypto_config: SessionCryptoConfig,
    session_key: SessionKey,
    transport: T,
    cache: C,
    head: std::cell::RefCell<Option<TurnId>>,
}

impl<T: SessionTransport> Session<T, MemorySessionCache> {
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
        Self::new_with_cache(
            session_id,
            pairing_secret,
            transport,
            crypto_config,
            MemorySessionCache::new(),
        )
    }
}

impl<T: SessionTransport, C: LocalSessionCache> Session<T, C> {
    pub fn new_with_cache(
        session_id: impl AsRef<str>,
        pairing_secret: Vec<u8>,
        transport: T,
        crypto_config: SessionCryptoConfig,
        cache: C,
    ) -> Result<Self, AgentError> {
        let session_key = SessionKey::derive(&pairing_secret, &crypto_config)?;
        Ok(Self {
            session_id: SessionId::new(session_id.as_ref())?,
            crypto_config,
            session_key,
            transport,
            cache,
            head: std::cell::RefCell::new(None),
        })
    }

    pub fn begin_active_turn(
        &self,
        device_id: &DeviceId,
    ) -> Result<Option<ActiveTurn<'_, T, C>>, AgentError> {
        let observed_head = self.head.borrow().clone();
        let lease = match self.transport.acquire_lease(
            &self.session_id,
            device_id,
            observed_head,
            now_ms()?,
            ACTIVE_TURN_LEASE_TTL_MS,
        )? {
            Some(lease) => lease,
            None => return Ok(None),
        };
        Ok(Some(ActiveTurn {
            session: self,
            next_observed_head: lease.observed_head.clone(),
            lease,
            finished: false,
        }))
    }

    pub fn append_offline_pending(
        &self,
        device_id: &DeviceId,
        role: &str,
        content: &str,
    ) -> Result<Turn, AgentError> {
        let observed_head = self.head.borrow().clone();
        let (turn_id, turn, sealed) = self.prepare_turn(
            observed_head,
            role,
            content,
            TurnLeaseState::offline_unleased(device_id),
        )?;
        let bytes = serde_json::to_vec(&sealed).map_err(|e| AgentError::Provider(e.to_string()))?;
        self.cache.put_pending(&self.session_id, &turn_id, &bytes)?;
        *self.head.borrow_mut() = Some(turn_id);
        Ok(turn)
    }

    fn prepare_turn(
        &self,
        observed_head: Option<TurnId>,
        role: &str,
        content: &str,
        lease_state: TurnLeaseState,
    ) -> Result<(TurnId, Turn, SealedTurn), AgentError> {
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
            lease_state,
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
        Ok((turn_id, turn, sealed))
    }

    /// Upload any pending (offline-written) turns that the transport does not yet have.
    /// Idempotent: ULIDs already present are skipped. Returns how many were uploaded.
    pub fn sync(&self) -> Result<usize, AgentError> {
        let present: std::collections::HashSet<TurnId> =
            self.transport.list(&self.session_id)?.into_iter().collect();
        let mut uploaded = 0;
        for turn_id in self.cache.list_pending(&self.session_id)? {
            if present.contains(&turn_id) {
                self.cache.remove_pending(&self.session_id, &turn_id)?;
                continue; // already there — no duplicate
            }
            let bytes = self.cache.get_pending(&self.session_id, &turn_id)?;
            if let Ok(()) = self.transport.put(&self.session_id, &turn_id, &bytes) {
                self.cache.remove_pending(&self.session_id, &turn_id)?;
                uploaded += 1;
            }
        }
        Ok(uploaded)
    }

    /// Load the whole conversation, decrypted and sorted by ULID (= creation order).
    /// Merges transport files with any still-pending local turns.
    pub fn load(&self) -> Result<Vec<Turn>, AgentError> {
        Ok(self.load_full()?.turns)
    }

    /// Load the whole conversation plus computed heads/fork state. Deterministic
    /// display order remains ULID sort; callers must inspect `fork` before choosing a head.
    pub fn load_full(&self) -> Result<LoadedSession, AgentError> {
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
        for turn_id in self.cache.list_pending(&self.session_id)? {
            let bytes = self.cache.get_pending(&self.session_id, &turn_id)?;
            let sealed: SealedTurn =
                serde_json::from_slice(&bytes).map_err(|e| AgentError::Provider(e.to_string()))?;
            if sealed.ulid != turn_id.as_str() {
                return Err(AgentError::Provider(format!(
                    "pending turn id mismatch: file {} envelope {}",
                    turn_id, sealed.ulid
                )));
            }
            sealed_by_ulid.entry(turn_id).or_insert(sealed);
        }
        let mut turns = Vec::with_capacity(sealed_by_ulid.len());
        for (_ulid, sealed) in sealed_by_ulid {
            let plaintext = session_crypto::open(&self.session_key, &self.crypto_config, &sealed)?;
            let turn: Turn = serde_json::from_slice(&plaintext)
                .map_err(|e| AgentError::Provider(e.to_string()))?;
            turns.push(turn);
        }
        let (heads, fork) = analyze_loaded_turns(&turns)?;
        if fork.is_none() {
            *self.head.borrow_mut() = heads.first().cloned();
        }
        Ok(LoadedSession { turns, heads, fork }) // BTreeMap iterates keys (ULIDs) in sorted order
    }

    /// Acquire the active-turn lease (anti-fork) for `holder`.
    pub fn begin_turn(&self, holder: &DeviceId) -> Result<bool, AgentError> {
        self.transport
            .acquire_lease(
                &self.session_id,
                holder,
                self.head.borrow().clone(),
                now_ms()?,
                ACTIVE_TURN_LEASE_TTL_MS,
            )
            .map(|lease| lease.is_some())
    }

    /// Release the active-turn lease.
    pub fn end_turn(&self, holder: &DeviceId) -> Result<(), AgentError> {
        if let Some(lease) = self.transport.current_lease(&self.session_id)? {
            if lease.holder_device_id == *holder {
                if self.transport.release_lease(&lease)? {
                    return Ok(());
                }
                return Err(AgentError::Transport(
                    "active-turn lease was not released".into(),
                ));
            }
        }
        Ok(())
    }
}

impl<T: SessionTransport, C: LocalSessionCache> ActiveTurn<'_, T, C> {
    pub fn lease(&self) -> &LeaseRecord {
        &self.lease
    }

    pub fn append(&mut self, role: &str, content: &str) -> Result<Turn, AgentError> {
        let (turn_id, turn, sealed) = self.session.prepare_turn(
            self.next_observed_head.clone(),
            role,
            content,
            TurnLeaseState::active(&self.lease.holder_device_id, &self.lease.lease_id),
        )?;
        let bytes = serde_json::to_vec(&sealed).map_err(|e| AgentError::Provider(e.to_string()))?;
        self.session
            .transport
            .put(&self.lease.session_id, &turn_id, &bytes)?;
        *self.session.head.borrow_mut() = Some(turn_id);
        self.next_observed_head = Some(TurnId::new(&turn.ulid)?);
        Ok(turn)
    }

    pub fn renew(&mut self) -> Result<bool, AgentError> {
        match self.session.transport.renew_lease(
            &self.lease,
            now_ms()?,
            ACTIVE_TURN_LEASE_TTL_MS,
        )? {
            Some(lease) => {
                self.lease = lease;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub fn finish(mut self) -> Result<(), AgentError> {
        if !self.session.transport.release_lease(&self.lease)? {
            return Err(AgentError::Transport(
                "active-turn lease was not released".into(),
            ));
        }
        self.finished = true;
        Ok(())
    }
}

// ----- in-memory transport (tests + offline simulation) -----

/// In-memory [`SessionTransport`] with an `offline` switch, for tests.
#[derive(Default)]
pub struct InMemoryTransport {
    files: std::cell::RefCell<std::collections::HashMap<(SessionId, TurnId), Vec<u8>>>,
    lease: std::cell::RefCell<std::collections::HashMap<SessionId, LeaseRecord>>,
    offline: std::cell::Cell<bool>,
    put_count: std::cell::Cell<usize>,
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

    pub fn put_count(&self) -> usize {
        self.put_count.get()
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
        self.put_count.set(self.put_count.get() + 1);
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
    fn acquire_lease(
        &self,
        session_id: &SessionId,
        holder: &DeviceId,
        observed_head: Option<TurnId>,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<Option<LeaseRecord>, AgentError> {
        self.guard()?;
        let mut lease = self.lease.borrow_mut();
        match lease.get(session_id) {
            Some(existing) if existing.expires_at_ms > now_ms => Ok(None),
            _ => {
                let next = new_lease_record(session_id, holder, observed_head, now_ms, ttl_ms)?;
                lease.insert(session_id.clone(), next.clone());
                Ok(Some(next))
            }
        }
    }
    fn renew_lease(
        &self,
        lease_record: &LeaseRecord,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<Option<LeaseRecord>, AgentError> {
        self.guard()?;
        let mut lease = self.lease.borrow_mut();
        let Some(existing) = lease.get(&lease_record.session_id) else {
            return Ok(None);
        };
        if existing.holder_device_id != lease_record.holder_device_id
            || existing.lease_id != lease_record.lease_id
        {
            return Ok(None);
        }
        let mut next = existing.clone();
        next.expires_at_ms = now_ms.saturating_add(ttl_ms);
        lease.insert(lease_record.session_id.clone(), next.clone());
        Ok(Some(next))
    }
    fn release_lease(&self, lease_record: &LeaseRecord) -> Result<bool, AgentError> {
        let mut lease = self.lease.borrow_mut();
        if lease
            .get(&lease_record.session_id)
            .map(|existing| {
                existing.holder_device_id == lease_record.holder_device_id
                    && existing.lease_id == lease_record.lease_id
            })
            .unwrap_or(false)
        {
            lease.remove(&lease_record.session_id);
            return Ok(true);
        }
        Ok(false)
    }
    fn current_lease(&self, session_id: &SessionId) -> Result<Option<LeaseRecord>, AgentError> {
        Ok(self.lease.borrow().get(session_id).cloned())
    }
}

// ----- OneDrive transport (real, feature-gated) -----

#[cfg(feature = "onedrive")]
mod onedrive {
    use super::{
        new_lease_record, onedrive_lease_file, onedrive_session_dir, onedrive_turn_file, DeviceId,
        LeaseId, LeaseRecord, SessionId, SessionTransport, TurnId,
    };
    use crate::AgentError;
    use isyncyou_graph::http::{ConflictBehavior, GraphClient, UploadError};

    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    struct LeaseFileBody {
        v: u8,
        session_id: String,
        holder_device_id: String,
        lease_id: String,
        observed_head: Option<String>,
        created_at_ms: u64,
        expires_at_ms: u64,
    }

    impl LeaseFileBody {
        fn from_record(record: &LeaseRecord) -> Self {
            Self {
                v: 1,
                session_id: record.session_id.to_string(),
                holder_device_id: record.holder_device_id.to_string(),
                lease_id: record.lease_id.to_string(),
                observed_head: record.observed_head.as_ref().map(ToString::to_string),
                created_at_ms: record.created_at_ms,
                expires_at_ms: record.expires_at_ms,
            }
        }

        fn into_record(
            self,
            cloud_item_id: Option<String>,
            cloud_etag: Option<String>,
        ) -> Result<LeaseRecord, AgentError> {
            if self.v != 1 {
                return Err(AgentError::Transport(
                    "unsupported agent lease version".into(),
                ));
            }
            Ok(LeaseRecord {
                session_id: SessionId::new(&self.session_id)?,
                holder_device_id: DeviceId::new(&self.holder_device_id)?,
                lease_id: LeaseId::new(&self.lease_id)?,
                observed_head: self.observed_head.map(TurnId::new).transpose()?,
                created_at_ms: self.created_at_ms,
                expires_at_ms: self.expires_at_ms,
                cloud_item_id,
                cloud_etag,
            })
        }
    }

    fn attach_remote_fields(mut lease: LeaseRecord, value: &serde_json::Value) -> LeaseRecord {
        lease.cloud_item_id = value
            .get("id")
            .and_then(|id| id.as_str())
            .map(ToString::to_string)
            .or(lease.cloud_item_id);
        lease.cloud_etag = value
            .get("eTag")
            .or_else(|| value.get("@odata.etag"))
            .and_then(|etag| etag.as_str())
            .map(ToString::to_string)
            .or(lease.cloud_etag);
        lease
    }

    fn required_remote_field<'a>(
        lease: &'a LeaseRecord,
        field: &'static str,
    ) -> Result<&'a str, AgentError> {
        match field {
            "id" => lease.cloud_item_id.as_deref(),
            "etag" => lease.cloud_etag.as_deref(),
            _ => None,
        }
        .ok_or_else(|| AgentError::Transport(format!("lease had no remote {field}")))
    }

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

        #[cfg(test)]
        pub(crate) fn with_base_url(token: impl Into<String>, base_url: &str) -> Self {
            Self {
                client: GraphClient::new(token).with_base_url(base_url),
            }
        }

        fn lease_bytes(lease: &LeaseRecord) -> Result<Vec<u8>, AgentError> {
            serde_json::to_vec(&LeaseFileBody::from_record(lease))
                .map_err(|e| AgentError::Provider(e.to_string()))
        }

        fn read_lease(&self, session_id: &SessionId) -> Result<Option<LeaseRecord>, AgentError> {
            let path = onedrive_lease_file(session_id);
            let Some(item) = self
                .client
                .get_drive_item_by_path(&path, &["id", "eTag", "name"])
                .map_err(|e| AgentError::Transport(e.to_string()))?
            else {
                return Ok(None);
            };
            let item_id = item
                .get("id")
                .and_then(|id| id.as_str())
                .map(ToString::to_string);
            let etag = item
                .get("eTag")
                .or_else(|| item.get("@odata.etag"))
                .and_then(|etag| etag.as_str())
                .map(ToString::to_string);
            let url = format!("/me/drive/root:/{}:/content", path);
            let body = match self.client.get_bytes(&url) {
                Ok(bytes) => bytes,
                Err(UploadError::Http { status: 404, .. }) => return Ok(None),
                Err(err) => return Err(AgentError::Transport(err.to_string())),
            };
            let file: LeaseFileBody =
                serde_json::from_slice(&body).map_err(|e| AgentError::Transport(e.to_string()))?;
            let lease = file.into_record(item_id, etag)?;
            if lease.session_id != *session_id {
                return Err(AgentError::Transport("lease session id mismatch".into()));
            }
            Ok(Some(lease))
        }
    }

    impl SessionTransport for OneDriveTransport {
        fn put(
            &self,
            session_id: &SessionId,
            turn_id: &TurnId,
            bytes: &[u8],
        ) -> Result<(), AgentError> {
            let created = self
                .client
                .upload_content_with_conflict_behavior(
                    &onedrive_turn_file(session_id, turn_id),
                    bytes,
                    ConflictBehavior::Fail,
                )
                .map_err(|e| AgentError::Transport(e.to_string()))?;
            if created.is_none() {
                return Err(AgentError::Transport(format!(
                    "turn already exists: {turn_id}"
                )));
            }
            Ok(())
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
            let items = self
                .client
                .get_json_paged(&url)
                .map_err(|e| AgentError::Transport(e.to_string()))?;
            let mut ulids = Vec::new();
            for it in items {
                if let Some(name) = it.get("name").and_then(|n| n.as_str()) {
                    if name.starts_with('.') {
                        continue;
                    }
                    if let Some(ulid) = name.strip_suffix(".json") {
                        ulids.push(TurnId::new(ulid)?);
                    }
                }
            }
            Ok(ulids)
        }
        fn acquire_lease(
            &self,
            session_id: &SessionId,
            holder: &DeviceId,
            observed_head: Option<TurnId>,
            now_ms: u64,
            ttl_ms: u64,
        ) -> Result<Option<LeaseRecord>, AgentError> {
            let proposed = new_lease_record(session_id, holder, observed_head, now_ms, ttl_ms)?;
            let bytes = Self::lease_bytes(&proposed)?;
            if let Some(item) = self
                .client
                .upload_content_with_conflict_behavior(
                    &onedrive_lease_file(session_id),
                    &bytes,
                    ConflictBehavior::Fail,
                )
                .map_err(|e| AgentError::Transport(e.to_string()))?
            {
                return Ok(Some(attach_remote_fields(proposed, &item)));
            }

            let Some(existing) = self.read_lease(session_id)? else {
                return Ok(None);
            };
            if existing.expires_at_ms > now_ms {
                return Ok(None);
            }

            let item_id = required_remote_field(&existing, "id")?;
            let etag = required_remote_field(&existing, "etag")?;
            let takeover =
                new_lease_record(session_id, holder, proposed.observed_head, now_ms, ttl_ms)?;
            let bytes = Self::lease_bytes(&takeover)?;
            match self
                .client
                .replace_content_if_match(item_id, &bytes, etag)
                .map_err(|e| AgentError::Transport(e.to_string()))?
            {
                Some(item) => Ok(Some(attach_remote_fields(takeover, &item))),
                None => Ok(None),
            }
        }
        fn renew_lease(
            &self,
            lease: &LeaseRecord,
            now_ms: u64,
            ttl_ms: u64,
        ) -> Result<Option<LeaseRecord>, AgentError> {
            let Some(current) = self.read_lease(&lease.session_id)? else {
                return Ok(None);
            };
            if current.holder_device_id != lease.holder_device_id
                || current.lease_id != lease.lease_id
            {
                return Ok(None);
            }
            let item_id = required_remote_field(&current, "id")?;
            let etag = required_remote_field(&current, "etag")?;
            let mut renewed = current.clone();
            renewed.expires_at_ms = now_ms.saturating_add(ttl_ms);
            let bytes = Self::lease_bytes(&renewed)?;
            match self
                .client
                .replace_content_if_match(item_id, &bytes, etag)
                .map_err(|e| AgentError::Transport(e.to_string()))?
            {
                Some(item) => Ok(Some(attach_remote_fields(renewed, &item))),
                None => Ok(None),
            }
        }
        fn release_lease(&self, lease: &LeaseRecord) -> Result<bool, AgentError> {
            let Some(current) = self.read_lease(&lease.session_id)? else {
                return Ok(false);
            };
            if current.holder_device_id != lease.holder_device_id
                || current.lease_id != lease.lease_id
            {
                return Ok(false);
            }
            let item_id = required_remote_field(&current, "id")?;
            let etag = required_remote_field(&current, "etag")?;
            self.client
                .delete_item_if_match(item_id, etag)
                .map_err(|e| AgentError::Transport(e.to_string()))
        }
        fn current_lease(&self, session_id: &SessionId) -> Result<Option<LeaseRecord>, AgentError> {
            self.read_lease(session_id)
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

    fn test_lease_state() -> TurnLeaseState {
        TurnLeaseState::offline_unleased(&did("fixture"))
    }

    fn fixture_turn(
        ulid: &str,
        observed_head: Option<&str>,
        parent_turn_ids: Vec<&str>,
        content: &str,
    ) -> Turn {
        Turn {
            ulid: ulid.into(),
            role: "user".into(),
            content: content.into(),
            observed_head: observed_head.map(String::from),
            parent_turn_ids: parent_turn_ids.into_iter().map(String::from).collect(),
            lease_state: test_lease_state(),
            ts_ms: 0,
        }
    }

    fn put_fixture_turn_with_key(
        transport: &InMemoryTransport,
        config: &SessionCryptoConfig,
        key: &SessionKey,
        turn: Turn,
    ) {
        let turn_id = tid(&turn.ulid);
        let plaintext = serde_json::to_vec(&turn).unwrap();
        let sealed =
            session_crypto::seal(key, config, &sid("sess1"), &turn_id, &plaintext).unwrap();
        transport
            .put(
                &sid("sess1"),
                &turn_id,
                &serde_json::to_vec(&sealed).unwrap(),
            )
            .unwrap();
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
        let mut active = s.begin_active_turn(&did("deviceA")).unwrap().unwrap();
        active.append("user", "find the spotify invoice").unwrap();
        active.append("assistant", "it is item-42").unwrap();
        active.finish().unwrap();
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
        let mut active = s.begin_active_turn(&did("deviceA")).unwrap().unwrap();
        let mut prev = String::new();
        for i in 0..50 {
            let t = active.append("user", &format!("turn {i}")).unwrap();
            assert!(
                t.ulid > prev,
                "ulid must strictly increase: {} !> {}",
                t.ulid,
                prev
            );
            prev = t.ulid;
        }
        active.finish().unwrap();
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
                lease_state: test_lease_state(),
                ts_ms: 0,
            };
            put_fixture_turn_with_key(&t, &config, &key, turn);
        }
        let s = Session::new_with_crypto_config("sess1", KEY.to_vec(), t, config).unwrap();
        let ulids: Vec<String> = s.load().unwrap().into_iter().map(|t| t.ulid).collect();
        assert_eq!(ulids, vec![TURN_A, TURN_B, TURN_C]);
    }

    #[test]
    fn linear_session_has_single_head_and_no_fork() {
        let t = InMemoryTransport::new();
        let config = crypto_config();
        let key = crypto_key(&config);
        put_fixture_turn_with_key(&t, &config, &key, fixture_turn(TURN_A, None, vec![], "a"));
        put_fixture_turn_with_key(
            &t,
            &config,
            &key,
            fixture_turn(TURN_B, Some(TURN_A), vec![TURN_A], "b"),
        );
        put_fixture_turn_with_key(
            &t,
            &config,
            &key,
            fixture_turn(TURN_C, Some(TURN_B), vec![TURN_B], "c"),
        );
        let s = Session::new_with_crypto_config("sess1", KEY.to_vec(), t, config).unwrap();
        let loaded = s.load_full().unwrap();
        assert_eq!(loaded.heads, vec![tid(TURN_C)]);
        assert!(loaded.fork.is_none());
        assert_eq!(
            loaded
                .turns
                .iter()
                .map(|turn| turn.ulid.as_str())
                .collect::<Vec<_>>(),
            vec![TURN_A, TURN_B, TURN_C]
        );
    }

    #[test]
    fn load_reports_two_heads_as_fork() {
        let t = InMemoryTransport::new();
        let config = crypto_config();
        let key = crypto_key(&config);
        put_fixture_turn_with_key(
            &t,
            &config,
            &key,
            fixture_turn(TURN_A, None, vec![], "root"),
        );
        put_fixture_turn_with_key(
            &t,
            &config,
            &key,
            fixture_turn(TURN_B, Some(TURN_A), vec![TURN_A], "left"),
        );
        put_fixture_turn_with_key(
            &t,
            &config,
            &key,
            fixture_turn(TURN_C, Some(TURN_A), vec![TURN_A], "right"),
        );
        let s = Session::new_with_crypto_config("sess1", KEY.to_vec(), t, config).unwrap();
        let loaded = s.load_full().unwrap();
        assert_eq!(loaded.heads, vec![tid(TURN_B), tid(TURN_C)]);
        let fork = loaded.fork.expect("two heads should be a fork");
        assert_eq!(fork.heads, vec![tid(TURN_B), tid(TURN_C)]);
        assert_eq!(fork.conflicting_turns, vec![tid(TURN_B), tid(TURN_C)]);
        assert!(fork.missing_parent_refs.is_empty());
    }

    #[test]
    fn load_reports_missing_parent_as_fork_or_corruption() {
        let t = InMemoryTransport::new();
        let config = crypto_config();
        let key = crypto_key(&config);
        put_fixture_turn_with_key(
            &t,
            &config,
            &key,
            fixture_turn(TURN_B, Some(TURN_A), vec![TURN_A], "orphan"),
        );
        let s = Session::new_with_crypto_config("sess1", KEY.to_vec(), t, config).unwrap();
        let loaded = s.load_full().unwrap();
        assert_eq!(loaded.heads, vec![tid(TURN_B)]);
        let fork = loaded.fork.expect("missing parent should be reported");
        assert_eq!(fork.missing_parent_refs, vec![tid(TURN_A)]);
        assert_eq!(fork.conflicting_turns, vec![tid(TURN_B)]);
    }

    #[test]
    fn forced_offline_concurrent_turns_surface_fork_on_reconnect() {
        let t = InMemoryTransport::new();
        let config = crypto_config();
        let key = crypto_key(&config);
        put_fixture_turn_with_key(
            &t,
            &config,
            &key,
            fixture_turn(TURN_A, None, vec![], "root"),
        );
        put_fixture_turn_with_key(
            &t,
            &config,
            &key,
            fixture_turn(TURN_B, Some(TURN_A), vec![TURN_A], "offline-left"),
        );
        put_fixture_turn_with_key(
            &t,
            &config,
            &key,
            fixture_turn(TURN_C, Some(TURN_A), vec![TURN_A], "offline-right"),
        );
        let s = Session::new_with_crypto_config("sess1", KEY.to_vec(), t, config).unwrap();
        let loaded = s.load_full().unwrap();
        let fork = loaded.fork.expect("forced concurrent children should fork");
        assert_eq!(fork.heads, vec![tid(TURN_B), tid(TURN_C)]);
        assert_eq!(fork.conflicting_turns, vec![tid(TURN_B), tid(TURN_C)]);
    }

    #[test]
    fn offline_writes_then_sync_is_idempotent() {
        // RcTransport lets the test toggle offline on the same transport the session uses.
        let t = std::rc::Rc::new(InMemoryTransport::new());
        let s = Session::new("sess1", KEY.to_vec(), RcTransport(t.clone())).unwrap();
        s.append_offline_pending(&did("deviceA"), "user", "a")
            .unwrap();
        s.append_offline_pending(&did("deviceA"), "assistant", "b")
            .unwrap();
        assert_eq!(t.put_count(), 0);
        t.set_offline(false);
        assert_eq!(s.sync().unwrap(), 2); // two uploaded
        assert_eq!(s.sync().unwrap(), 0); // idempotent — no duplicates
        assert_eq!(s.load().unwrap().len(), 2);
    }

    #[test]
    fn lease_prevents_concurrent_turns() {
        let s = Session::new("sess1", KEY.to_vec(), InMemoryTransport::new()).unwrap();
        let active = s.begin_active_turn(&did("deviceA")).unwrap();
        assert!(active.is_some());
        assert!(s.begin_active_turn(&did("deviceB")).unwrap().is_none()); // A holds it
        active.unwrap().finish().unwrap();
        assert!(s.begin_active_turn(&did("deviceB")).unwrap().is_some()); // now free
    }

    #[test]
    fn active_turn_records_observed_head_and_lease_state() {
        let s = Session::new("sess1", KEY.to_vec(), InMemoryTransport::new()).unwrap();
        let mut first = s.begin_active_turn(&did("deviceA")).unwrap().unwrap();
        assert!(first.lease().observed_head.is_none());
        let first_turn = first.append("user", "first").unwrap();
        first.finish().unwrap();

        let mut second = s.begin_active_turn(&did("deviceA")).unwrap().unwrap();
        assert_eq!(
            second
                .lease()
                .observed_head
                .as_ref()
                .map(ToString::to_string),
            Some(first_turn.ulid.clone())
        );
        let second_turn = second.append("assistant", "second").unwrap();
        assert_eq!(
            second_turn.observed_head.as_deref(),
            Some(first_turn.ulid.as_str())
        );
        match second_turn.lease_state {
            TurnLeaseState::Active {
                device_id,
                lease_id,
            } => {
                assert_eq!(device_id, "deviceA");
                assert_eq!(lease_id, second.lease().lease_id.to_string());
            }
            other => panic!("unexpected lease state: {other:?}"),
        }
        second.finish().unwrap();
    }

    #[test]
    fn active_turn_renew_succeeds_only_for_current_holder() {
        let s = Session::new("sess1", KEY.to_vec(), InMemoryTransport::new()).unwrap();
        let mut active = s.begin_active_turn(&did("deviceA")).unwrap().unwrap();
        assert!(active.renew().unwrap());
        let lease = active.lease().clone();
        active.finish().unwrap();
        assert!(s
            .transport
            .renew_lease(&lease, now_ms().unwrap(), ACTIVE_TURN_LEASE_TTL_MS)
            .unwrap()
            .is_none());
    }

    #[test]
    fn offline_append_records_unleased_fork_risk_and_skips_transport_put() {
        let t = std::rc::Rc::new(InMemoryTransport::new());
        let s = Session::new("sess1", KEY.to_vec(), RcTransport(t.clone())).unwrap();
        let turn = s
            .append_offline_pending(&did("deviceA"), "user", "offline")
            .unwrap();
        assert_eq!(t.put_count(), 0);
        assert_eq!(
            turn.lease_state,
            TurnLeaseState::OfflineUnleased {
                device_id: "deviceA".into()
            }
        );
    }

    #[test]
    fn filesystem_cache_persists_offline_pending_restart() {
        let dir = tempfile::tempdir().unwrap();
        let config = crypto_config();
        let first = Session::new_with_cache(
            "sess1",
            KEY.to_vec(),
            InMemoryTransport::new(),
            config.clone(),
            FileSessionCache::new(dir.path()),
        )
        .unwrap();
        let turn = first
            .append_offline_pending(&did("deviceA"), "user", "restart-sentinel")
            .unwrap();
        drop(first);

        let second = Session::new_with_cache(
            "sess1",
            KEY.to_vec(),
            InMemoryTransport::new(),
            config,
            FileSessionCache::new(dir.path()),
        )
        .unwrap();
        let loaded = second.load().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].ulid, turn.ulid);
        assert_eq!(loaded[0].content, "restart-sentinel");
    }

    #[test]
    fn filesystem_cache_stores_sealed_pending_bytes_only() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileSessionCache::new(dir.path());
        let session = Session::new_with_cache(
            "sess1",
            KEY.to_vec(),
            InMemoryTransport::new(),
            crypto_config(),
            cache,
        )
        .unwrap();
        let turn = session
            .append_offline_pending(&did("deviceA"), "user", "PLAINTEXT-PENDING-SENTINEL")
            .unwrap();
        let path = session.cache.pending_path(&sid("sess1"), &tid(&turn.ulid));
        let raw = std::fs::read(path).unwrap();
        let raw_text = String::from_utf8_lossy(&raw);
        assert!(!raw_text.contains("PLAINTEXT-PENDING-SENTINEL"));
        assert!(raw_text.contains("\"ct\""));
    }

    #[test]
    fn sync_pending_removes_files_after_success_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let transport = std::rc::Rc::new(InMemoryTransport::new());
        let cache = FileSessionCache::new(dir.path());
        let session = Session::new_with_cache(
            "sess1",
            KEY.to_vec(),
            RcTransport(transport.clone()),
            crypto_config(),
            cache,
        )
        .unwrap();
        let turn = session
            .append_offline_pending(&did("deviceA"), "user", "sync-me")
            .unwrap();
        assert_eq!(session.cache.list_pending(&sid("sess1")).unwrap().len(), 1);
        assert_eq!(session.sync().unwrap(), 1);
        assert!(session
            .cache
            .list_pending(&sid("sess1"))
            .unwrap()
            .is_empty());
        assert!(transport.raw(&sid("sess1"), &tid(&turn.ulid)).is_some());
        assert_eq!(session.sync().unwrap(), 0);
    }

    #[test]
    fn filesystem_cache_rejects_symlink_and_traversal_hazards() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileSessionCache::new(dir.path());
        assert!(SessionId::new("../bad").is_err());

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path(), dir.path().join("sess1")).unwrap();
            let err = cache
                .put_pending(&sid("sess1"), &tid(TURN_A), b"sealed")
                .unwrap_err()
                .to_string();
            assert!(err.contains("symlink"), "{err}");
        }
    }

    #[test]
    fn fork_detection_flags_two_children_of_one_head() {
        let mk = |ulid: &str, head: Option<&str>| Turn {
            ulid: ulid.into(),
            role: "user".into(),
            content: "x".into(),
            observed_head: head.map(|h| h.into()),
            parent_turn_ids: head.into_iter().map(|h| h.to_string()).collect(),
            lease_state: test_lease_state(),
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
        let mut active = s.begin_active_turn(&did("deviceA")).unwrap().unwrap();
        let turn = active.append("user", "VERY-SECRET-MAIL-CONTENT").unwrap();
        active.finish().unwrap();
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
            "Apps/iSyncYou/agent/sess1/.active_turn_lease.json"
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
            lease_state: test_lease_state(),
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

    #[cfg(feature = "onedrive")]
    fn read_request(sock: &mut std::net::TcpStream) -> String {
        use std::io::Read;
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") {
            if sock.read(&mut byte).unwrap_or(0) == 0 {
                break;
            }
            buf.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&buf).to_string();
        let content_length = head
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(str::to_owned)
            })
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            sock.read_exact(&mut body).unwrap();
        }
        if body.is_empty() {
            head
        } else {
            format!("{head}\n{}", String::from_utf8_lossy(&body))
        }
    }

    #[cfg(feature = "onedrive")]
    fn serve(responses: Vec<String>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for response in responses {
                let (mut sock, _) = listener.accept().unwrap();
                seen.push(read_request(&mut sock));
                use std::io::Write;
                sock.write_all(response.as_bytes()).unwrap();
            }
            seen
        });
        (format!("http://{addr}"), handle)
    }

    #[cfg(feature = "onedrive")]
    fn http_response(status: u16, reason: &str, extra_headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[cfg(feature = "onedrive")]
    fn lease_body(holder: &str, lease_id: &str, expires_at_ms: u64) -> String {
        serde_json::json!({
            "v": 1,
            "session_id": "sess1",
            "holder_device_id": holder,
            "lease_id": lease_id,
            "observed_head": null,
            "created_at_ms": 1,
            "expires_at_ms": expires_at_ms
        })
        .to_string()
    }

    #[cfg(feature = "onedrive")]
    #[test]
    fn graph_lease_create_conflict_returns_busy() {
        let (base, server) = serve(vec![
            http_response(409, "Conflict", "", "exists"),
            http_response(
                200,
                "OK",
                "",
                "{\"id\":\"lease-item\",\"eTag\":\"\\\"e1\\\"\"}",
            ),
            http_response(200, "OK", "", &lease_body("deviceB", "lease-b", 10_000)),
        ]);
        let transport = super::onedrive::OneDriveTransport::with_base_url("tok", &base);
        let acquired = transport
            .acquire_lease(&sid("sess1"), &did("deviceA"), None, 5_000, 120_000)
            .unwrap();
        assert!(acquired.is_none());
        let seen = server.join().unwrap();
        assert!(seen[0].contains(
            "PUT /me/drive/root:/Apps/iSyncYou/agent/sess1/.active_turn_lease.json:/content?@microsoft.graph.conflictBehavior=fail "
        ));
        assert!(seen[1].contains(
            "GET /me/drive/root:/Apps/iSyncYou/agent/sess1/.active_turn_lease.json:?$select=id,eTag,name "
        ));
        assert!(seen[2].contains(
            "GET /me/drive/root:/Apps/iSyncYou/agent/sess1/.active_turn_lease.json:/content "
        ));
    }

    #[cfg(feature = "onedrive")]
    #[test]
    fn graph_lease_takeover_uses_if_match() {
        let (base, server) = serve(vec![
            http_response(409, "Conflict", "", "exists"),
            http_response(
                200,
                "OK",
                "",
                "{\"id\":\"lease-item\",\"eTag\":\"\\\"old-etag\\\"\"}",
            ),
            http_response(200, "OK", "", &lease_body("deviceB", "lease-b", 4_000)),
            http_response(
                200,
                "OK",
                "",
                "{\"id\":\"lease-item\",\"eTag\":\"\\\"new-etag\\\"\"}",
            ),
        ]);
        let transport = super::onedrive::OneDriveTransport::with_base_url("tok", &base);
        let acquired = transport
            .acquire_lease(&sid("sess1"), &did("deviceA"), None, 5_000, 120_000)
            .unwrap()
            .expect("expired lease should be taken over");
        assert_eq!(acquired.holder_device_id, did("deviceA"));
        assert_eq!(acquired.cloud_etag.as_deref(), Some("\"new-etag\""));
        let seen = server.join().unwrap();
        assert!(seen[3].contains("PUT /me/drive/items/lease-item/content "));
        assert!(seen[3].contains("if-match: \"old-etag\""));
        assert!(seen[3].contains("\"holder_device_id\":\"deviceA\""));
    }

    #[cfg(feature = "onedrive")]
    #[test]
    fn graph_lease_takeover_lost_race_does_not_acquire() {
        let (base, server) = serve(vec![
            http_response(409, "Conflict", "", "exists"),
            http_response(
                200,
                "OK",
                "",
                "{\"id\":\"lease-item\",\"eTag\":\"\\\"old-etag\\\"\"}",
            ),
            http_response(200, "OK", "", &lease_body("deviceB", "lease-b", 4_000)),
            http_response(412, "Precondition Failed", "", ""),
        ]);
        let transport = super::onedrive::OneDriveTransport::with_base_url("tok", &base);
        let acquired = transport
            .acquire_lease(&sid("sess1"), &did("deviceA"), None, 5_000, 120_000)
            .unwrap();
        assert!(acquired.is_none());
        let seen = server.join().unwrap();
        assert!(seen[3].contains("if-match: \"old-etag\""));
    }

    #[cfg(feature = "onedrive")]
    #[test]
    fn graph_turn_put_uses_create_if_absent() {
        let (base, server) = serve(vec![http_response(409, "Conflict", "", "exists")]);
        let transport = super::onedrive::OneDriveTransport::with_base_url("tok", &base);
        let err = transport
            .put(&sid("sess1"), &tid(TURN_A), b"sealed")
            .unwrap_err()
            .to_string();
        assert!(err.contains("turn already exists"), "{err}");
        let seen = server.join().unwrap();
        assert!(seen[0].contains(&format!(
            "PUT /me/drive/root:/Apps/iSyncYou/agent/sess1/{TURN_A}.json:/content?@microsoft.graph.conflictBehavior=fail "
        )));
    }

    #[cfg(feature = "onedrive")]
    #[test]
    fn graph_lease_renew_checks_holder_and_lease_id() {
        let mut lease =
            new_lease_record(&sid("sess1"), &did("deviceA"), None, 5_000, 120_000).unwrap();
        lease.lease_id = LeaseId::new("lease-a").unwrap();

        let (base, server) = serve(vec![
            http_response(
                200,
                "OK",
                "",
                "{\"id\":\"lease-item\",\"eTag\":\"\\\"e1\\\"\"}",
            ),
            http_response(200, "OK", "", &lease_body("deviceB", "lease-b", 125_000)),
        ]);
        let transport = super::onedrive::OneDriveTransport::with_base_url("tok", &base);
        assert!(transport
            .renew_lease(&lease, 10_000, 120_000)
            .unwrap()
            .is_none());
        assert_eq!(server.join().unwrap().len(), 2);

        let (base, server) = serve(vec![
            http_response(
                200,
                "OK",
                "",
                "{\"id\":\"lease-item\",\"eTag\":\"\\\"e2\\\"\"}",
            ),
            http_response(200, "OK", "", &lease_body("deviceA", "lease-a", 125_000)),
            http_response(
                200,
                "OK",
                "",
                "{\"id\":\"lease-item\",\"eTag\":\"\\\"e3\\\"\"}",
            ),
        ]);
        let transport = super::onedrive::OneDriveTransport::with_base_url("tok", &base);
        let renewed = transport
            .renew_lease(&lease, 10_000, 120_000)
            .unwrap()
            .expect("matching lease should renew");
        assert_eq!(renewed.cloud_etag.as_deref(), Some("\"e3\""));
        let seen = server.join().unwrap();
        assert!(seen[2].contains("PUT /me/drive/items/lease-item/content "));
        assert!(seen[2].contains("if-match: \"e2\""));
        assert!(seen[2].contains("\"expires_at_ms\":130000"));
    }

    #[cfg(feature = "onedrive")]
    #[test]
    fn graph_lease_release_checks_holder_and_lease_id() {
        let mut lease =
            new_lease_record(&sid("sess1"), &did("deviceA"), None, 5_000, 120_000).unwrap();
        lease.lease_id = LeaseId::new("lease-a").unwrap();
        let (base, server) = serve(vec![
            http_response(
                200,
                "OK",
                "",
                "{\"id\":\"lease-item\",\"eTag\":\"\\\"e1\\\"\"}",
            ),
            http_response(200, "OK", "", &lease_body("deviceB", "lease-b", 125_000)),
        ]);
        let transport = super::onedrive::OneDriveTransport::with_base_url("tok", &base);
        assert!(!transport.release_lease(&lease).unwrap());
        assert_eq!(server.join().unwrap().len(), 2);

        let (base, server) = serve(vec![
            http_response(
                200,
                "OK",
                "",
                "{\"id\":\"lease-item\",\"eTag\":\"\\\"e2\\\"\"}",
            ),
            http_response(200, "OK", "", &lease_body("deviceA", "lease-a", 125_000)),
            http_response(204, "No Content", "", ""),
        ]);
        let transport = super::onedrive::OneDriveTransport::with_base_url("tok", &base);
        assert!(transport.release_lease(&lease).unwrap());
        let seen = server.join().unwrap();
        assert!(seen[2].contains("DELETE /me/drive/items/lease-item "));
        assert!(seen[2].contains("if-match: \"e2\""));
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
        fn acquire_lease(
            &self,
            s: &SessionId,
            h: &DeviceId,
            head: Option<TurnId>,
            now_ms: u64,
            ttl_ms: u64,
        ) -> Result<Option<LeaseRecord>, AgentError> {
            self.0.acquire_lease(s, h, head, now_ms, ttl_ms)
        }
        fn renew_lease(
            &self,
            lease: &LeaseRecord,
            now_ms: u64,
            ttl_ms: u64,
        ) -> Result<Option<LeaseRecord>, AgentError> {
            self.0.renew_lease(lease, now_ms, ttl_ms)
        }
        fn release_lease(&self, lease: &LeaseRecord) -> Result<bool, AgentError> {
            self.0.release_lease(lease)
        }
        fn current_lease(&self, s: &SessionId) -> Result<Option<LeaseRecord>, AgentError> {
            self.0.current_lease(s)
        }
    }
}
