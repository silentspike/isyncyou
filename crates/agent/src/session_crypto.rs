//! Session-turn encryption (REQ-AGENT-006).
//!
//! Each turn is sealed with AES-256-GCM under a session key derived from the
//! cross-device pairing secret with Argon2id followed by HKDF-SHA256. The active KDF
//! profile is local trusted configuration from pairing/session setup. Envelope KDF data
//! is cleartext match metadata only; it is never allowed to choose KDF parameters while
//! opening a cloud file.

use crate::session_ids::{SessionId, TurnId};
use crate::AgentError;
use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ring::digest;
use ring::rand::{SecureRandom, SystemRandom};
use ring::{aead, hkdf};
use serde::{Deserialize, Serialize};

/// Envelope schema version (also part of the AEAD AAD).
pub const SCHEMA_VERSION: u32 = 2;
pub const KDF_ALG: &str = "argon2id-hkdf-sha256";
pub const KDF_PROFILE_VERSION: u32 = 1;

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const SESSION_SALT_LEN: usize = 16;
const HKDF_SALT: &[u8] = b"isyncyou-agent-session-root-salt-v1";
const HKDF_INFO: &[u8] = b"isyncyou-agent-session-root-v1";
const MIN_MEMORY_KIB: u32 = 65_536;
const MAX_MEMORY_KIB: u32 = 262_144;
const MIN_ITERATIONS: u32 = 3;
const MAX_ITERATIONS: u32 = 8;
const MIN_LANES: u32 = 4;
const MAX_LANES: u32 = 8;

/// Validated Argon2id/HKDF profile for one paired session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfProfile {
    pub alg: String,
    pub version: u32,
    pub memory_kib: u32,
    pub iterations: u32,
    pub lanes: u32,
    pub session_salt: String,
}

/// Local trusted crypto configuration. Constructing this validates the profile; opening
/// a turn later only accepts envelopes whose metadata exactly matches it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCryptoConfig {
    profile: KdfProfile,
}

/// Derived session key. This is the expensive Argon2id output after HKDF expansion; turns
/// use it without re-running Argon2id for every append/load.
#[derive(Clone)]
pub struct SessionKey {
    bytes: [u8; KEY_LEN],
}

/// One encrypted turn file. Envelope fields are cleartext metadata; `ct` is the
/// AES-256-GCM ciphertext+tag of the turn JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SealedTurn {
    pub v: u32,
    pub alg: String,
    pub kdf: KdfProfile,
    pub session_id: String,
    pub ulid: String,
    pub nonce: String,
    pub ct: String,
}

struct KeyLen(usize);
impl hkdf::KeyType for KeyLen {
    fn len(&self) -> usize {
        self.0
    }
}

impl KdfProfile {
    pub fn production(session_salt: [u8; SESSION_SALT_LEN]) -> Self {
        Self {
            alg: KDF_ALG.to_string(),
            version: KDF_PROFILE_VERSION,
            memory_kib: MIN_MEMORY_KIB,
            iterations: MIN_ITERATIONS,
            lanes: MIN_LANES,
            session_salt: B64.encode(session_salt),
        }
    }
}

impl SessionCryptoConfig {
    pub fn new(profile: KdfProfile) -> Result<Self, AgentError> {
        validate_profile(&profile)?;
        Ok(Self { profile })
    }

    pub fn generate_default() -> Result<Self, AgentError> {
        let rng = SystemRandom::new();
        let mut salt = [0u8; SESSION_SALT_LEN];
        rng.fill(&mut salt)
            .map_err(|_| crypto_err("rng session salt"))?;
        Self::new(KdfProfile::production(salt))
    }

    pub fn profile(&self) -> &KdfProfile {
        &self.profile
    }
}

impl SessionKey {
    pub fn derive(pairing_secret: &[u8], config: &SessionCryptoConfig) -> Result<Self, AgentError> {
        let salt = session_salt(config.profile())?;
        let params = Params::new(
            config.profile.memory_kib,
            config.profile.iterations,
            config.profile.lanes,
            Some(KEY_LEN),
        )
        .map_err(|_| crypto_err("argon2 params"))?;
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let mut stretched = [0u8; KEY_LEN];
        argon2
            .hash_password_into(pairing_secret, &salt, &mut stretched)
            .map_err(|_| crypto_err("argon2 derive"))?;

        let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, HKDF_SALT);
        let prk = salt.extract(&stretched);
        let okm = prk
            .expand(&[HKDF_INFO], KeyLen(KEY_LEN))
            .map_err(|_| crypto_err("hkdf expand"))?;
        let mut bytes = [0u8; KEY_LEN];
        okm.fill(&mut bytes).map_err(|_| crypto_err("hkdf fill"))?;
        Ok(Self { bytes })
    }
}

fn crypto_err(what: &str) -> AgentError {
    AgentError::Provider(format!("session crypto: {what}"))
}

fn validate_profile(profile: &KdfProfile) -> Result<(), AgentError> {
    if profile.alg != KDF_ALG {
        return Err(crypto_err("unsupported kdf alg"));
    }
    if profile.version != KDF_PROFILE_VERSION {
        return Err(crypto_err("unsupported kdf profile version"));
    }
    if profile.memory_kib < MIN_MEMORY_KIB {
        return Err(crypto_err("weak kdf memory"));
    }
    if profile.memory_kib > MAX_MEMORY_KIB {
        return Err(crypto_err("excessive kdf memory"));
    }
    if profile.iterations < MIN_ITERATIONS {
        return Err(crypto_err("weak kdf iterations"));
    }
    if profile.iterations > MAX_ITERATIONS {
        return Err(crypto_err("excessive kdf iterations"));
    }
    if profile.lanes < MIN_LANES {
        return Err(crypto_err("weak kdf lanes"));
    }
    if profile.lanes > MAX_LANES {
        return Err(crypto_err("excessive kdf lanes"));
    }
    let salt = session_salt(profile)?;
    if salt.len() < SESSION_SALT_LEN {
        return Err(crypto_err("short session salt"));
    }
    Ok(())
}

fn session_salt(profile: &KdfProfile) -> Result<Vec<u8>, AgentError> {
    B64.decode(&profile.session_salt)
        .map_err(|_| crypto_err("b64 session salt"))
}

fn profile_hash(profile: &KdfProfile) -> Result<[u8; KEY_LEN], AgentError> {
    let encoded = serde_json::to_vec(profile).map_err(|e| AgentError::Provider(e.to_string()))?;
    let digest = digest::digest(&digest::SHA256, &encoded);
    let mut out = [0u8; KEY_LEN];
    out.copy_from_slice(digest.as_ref());
    Ok(out)
}

fn push_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).expect("session AAD field length fits u32");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

fn aad_bytes(
    v: u32,
    session_id: &SessionId,
    turn_id: &TurnId,
    profile_hash: &[u8; KEY_LEN],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(96);
    out.extend_from_slice(b"isyncyou-agent-session-aad-v2");
    out.extend_from_slice(&v.to_be_bytes());
    push_len_prefixed(&mut out, session_id.as_str().as_bytes());
    push_len_prefixed(&mut out, turn_id.as_str().as_bytes());
    push_len_prefixed(&mut out, profile_hash);
    out
}

fn ensure_envelope_matches_config(
    sealed: &SealedTurn,
    config: &SessionCryptoConfig,
) -> Result<(), AgentError> {
    if sealed.v != SCHEMA_VERSION {
        return Err(crypto_err("unsupported session envelope version"));
    }
    if sealed.alg != "AES-256-GCM" {
        return Err(crypto_err("unsupported envelope alg"));
    }
    if sealed.kdf != *config.profile() {
        return Err(crypto_err("kdf profile mismatch"));
    }
    Ok(())
}

/// Seal a turn's plaintext into a [`SealedTurn`] envelope.
pub fn seal(
    session_key: &SessionKey,
    config: &SessionCryptoConfig,
    session_id: &SessionId,
    turn_id: &TurnId,
    plaintext: &[u8],
) -> Result<SealedTurn, AgentError> {
    validate_profile(config.profile())?;
    let rng = SystemRandom::new();
    let mut nonce = [0u8; NONCE_LEN];
    rng.fill(&mut nonce).map_err(|_| crypto_err("rng nonce"))?;

    let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, &session_key.bytes)
        .map_err(|_| crypto_err("aead key"))?;
    let sealing = aead::LessSafeKey::new(unbound);
    let kdf_hash = profile_hash(config.profile())?;
    let aad = aad_bytes(SCHEMA_VERSION, session_id, turn_id, &kdf_hash);
    let mut in_out = plaintext.to_vec();
    sealing
        .seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(aad.as_slice()),
            &mut in_out,
        )
        .map_err(|_| crypto_err("seal"))?;

    Ok(SealedTurn {
        v: SCHEMA_VERSION,
        alg: "AES-256-GCM".into(),
        kdf: config.profile().clone(),
        session_id: session_id.to_string(),
        ulid: turn_id.to_string(),
        nonce: B64.encode(nonce),
        ct: B64.encode(&in_out),
    })
}

/// Open a [`SealedTurn`] back to plaintext. The active `config` must already be trusted
/// local session state; envelope KDF data is only checked for exact equality.
pub fn open(
    session_key: &SessionKey,
    config: &SessionCryptoConfig,
    sealed: &SealedTurn,
) -> Result<Vec<u8>, AgentError> {
    ensure_envelope_matches_config(sealed, config)?;
    let session_id = SessionId::new(&sealed.session_id)?;
    let turn_id = TurnId::new(&sealed.ulid)?;
    let nonce_v = B64
        .decode(&sealed.nonce)
        .map_err(|_| crypto_err("b64 nonce"))?;
    let mut ct = B64.decode(&sealed.ct).map_err(|_| crypto_err("b64 ct"))?;
    if nonce_v.len() != NONCE_LEN {
        return Err(crypto_err("bad nonce length"));
    }
    let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, &session_key.bytes)
        .map_err(|_| crypto_err("aead key"))?;
    let opening = aead::LessSafeKey::new(unbound);
    let nonce: [u8; NONCE_LEN] = nonce_v.try_into().map_err(|_| crypto_err("nonce"))?;
    let kdf_hash = profile_hash(config.profile())?;
    let aad = aad_bytes(sealed.v, &session_id, &turn_id, &kdf_hash);
    let plaintext = opening
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(aad.as_slice()),
            &mut ct,
        )
        .map_err(|_| crypto_err("decryption failed (wrong pairing key or tampered)"))?;
    Ok(plaintext.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(value: &str) -> SessionId {
        SessionId::new(value).unwrap()
    }

    fn tid(value: &str) -> TurnId {
        TurnId::new(value).unwrap()
    }

    fn test_profile() -> KdfProfile {
        KdfProfile::production(*b"0123456789ABCDEF")
    }

    fn test_config() -> SessionCryptoConfig {
        SessionCryptoConfig::new(test_profile()).unwrap()
    }

    fn key_for(secret: &[u8], config: &SessionCryptoConfig) -> SessionKey {
        SessionKey::derive(secret, config).unwrap()
    }

    const TURN_A: &str = "0000000000000000000000000A";
    const TURN_B: &str = "0000000000000000000000000B";

    #[test]
    fn argon2id_hkdf_round_trips_with_pairing_secret() {
        let secret = b"a-high-entropy-pairing-secret-32b";
        let config = test_config();
        let key = key_for(secret, &config);
        let sealed = seal(
            &key,
            &config,
            &sid("sess1"),
            &tid(TURN_A),
            b"hello M365 excerpt",
        )
        .unwrap();
        assert_eq!(open(&key, &config, &sealed).unwrap(), b"hello M365 excerpt");
    }

    #[test]
    fn device_local_key_cannot_decrypt_pairing_session() {
        let config = test_config();
        let paired = key_for(b"the-shared-pairing-secret-value!!", &config);
        let device_local = key_for(b"some-other-device-local-only-key!", &config);
        let sealed = seal(
            &paired,
            &config,
            &sid("sess1"),
            &tid(TURN_A),
            b"secret body",
        )
        .unwrap();
        assert!(open(&device_local, &config, &sealed).is_err());
    }

    #[test]
    fn aad_binds_schema_session_ulid_and_kdf_profile() {
        let secret = b"a-high-entropy-pairing-secret-32b";
        let config = test_config();
        let key = key_for(secret, &config);
        let sealed = seal(&key, &config, &sid("sess1"), &tid(TURN_A), b"body").unwrap();
        let mut t = sealed.clone();
        t.ulid = TURN_B.into();
        assert!(open(&key, &config, &t).is_err());
        let mut t2 = sealed.clone();
        t2.session_id = "other".into();
        assert!(open(&key, &config, &t2).is_err());
        let mut t3 = sealed.clone();
        t3.v = 999;
        assert!(open(&key, &config, &t3).is_err());
        let mut t4 = sealed;
        t4.kdf.session_salt = B64.encode(*b"FEDCBA9876543210");
        assert!(open(&key, &config, &t4).is_err());
    }

    #[test]
    fn pairing_payload_rejects_weak_kdf_profile() {
        let mut weak = test_profile();
        weak.memory_kib = MIN_MEMORY_KIB - 1;
        assert!(SessionCryptoConfig::new(weak).is_err());
        let mut weak_iters = test_profile();
        weak_iters.iterations = MIN_ITERATIONS - 1;
        assert!(SessionCryptoConfig::new(weak_iters).is_err());
        let mut weak_lanes = test_profile();
        weak_lanes.lanes = MIN_LANES - 1;
        assert!(SessionCryptoConfig::new(weak_lanes).is_err());
    }

    #[test]
    fn envelope_kdf_profile_must_match_pairing_profile() {
        let secret = b"a-high-entropy-pairing-secret-32b";
        let config = test_config();
        let key = key_for(secret, &config);
        let mut sealed = seal(&key, &config, &sid("sess1"), &tid(TURN_A), b"body").unwrap();
        sealed.kdf.session_salt = B64.encode(*b"FEDCBA9876543210");
        let err = open(&key, &config, &sealed).unwrap_err().to_string();
        assert!(err.contains("kdf profile mismatch"), "{err}");
    }

    #[test]
    fn envelope_cannot_force_expensive_or_downgraded_kdf() {
        let config = test_config();
        let key = key_for(b"a-high-entropy-pairing-secret-32b", &config);
        let mut downgraded = seal(&key, &config, &sid("sess1"), &tid(TURN_A), b"body").unwrap();
        downgraded.kdf.memory_kib = MIN_MEMORY_KIB - 1;
        assert!(open(&key, &config, &downgraded).is_err());

        let mut excessive = test_profile();
        excessive.memory_kib = MAX_MEMORY_KIB + 1;
        assert!(SessionCryptoConfig::new(excessive).is_err());
    }

    #[test]
    fn nonce_reuse_is_not_observed_across_many_turns() {
        let config = test_config();
        let key = key_for(b"a-high-entropy-pairing-secret-32b", &config);
        let mut seen = std::collections::HashSet::new();
        for i in 0..64 {
            let turn_id = TurnId::new(format!("0000000000000000000000{:04X}", i)).unwrap();
            let sealed = seal(&key, &config, &sid("sess1"), &turn_id, b"body").unwrap();
            assert!(seen.insert(sealed.nonce), "nonce reused at {i}");
        }
    }

    #[test]
    fn ciphertext_contains_no_plaintext_m365_sentinel() {
        let config = test_config();
        let key = key_for(b"a-high-entropy-pairing-secret-32b", &config);
        let sealed = seal(
            &key,
            &config,
            &sid("sess1"),
            &tid(TURN_A),
            b"SENSITIVE-M365-MAIL-BODY",
        )
        .unwrap();
        let blob = serde_json::to_string(&sealed).unwrap();
        assert!(!blob.contains("SENSITIVE-M365-MAIL-BODY"));
    }

    #[test]
    fn hkdf_only_legacy_envelope_is_not_written() {
        let config = test_config();
        let key = key_for(b"a-high-entropy-pairing-secret-32b", &config);
        let sealed = seal(&key, &config, &sid("sess1"), &tid(TURN_A), b"body").unwrap();
        assert_eq!(sealed.v, SCHEMA_VERSION);
        assert_eq!(sealed.kdf.alg, KDF_ALG);
        assert_eq!(sealed.alg, "AES-256-GCM");

        let legacy = serde_json::json!({
            "v": 1,
            "session_id": "sess1",
            "ulid": TURN_A,
            "salt": B64.encode(*b"0123456789ABCDEF"),
            "nonce": B64.encode(*b"123456789012"),
            "ct": B64.encode(b"not real")
        });
        assert!(serde_json::from_value::<SealedTurn>(legacy).is_err());
    }

    #[test]
    fn invalid_envelope_ids_fail_before_decryption() {
        let config = test_config();
        let key = key_for(b"a-high-entropy-pairing-secret-32b", &config);
        let sealed = seal(&key, &config, &sid("sess1"), &tid(TURN_A), b"body").unwrap();
        let mut bad_session = sealed.clone();
        bad_session.session_id = "../sess".into();
        assert!(open(&key, &config, &bad_session).is_err());
        let mut bad_turn = sealed;
        bad_turn.ulid = "not-a-ulid".into();
        assert!(open(&key, &config, &bad_turn).is_err());
    }
}
