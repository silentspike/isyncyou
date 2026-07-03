//! Session-turn encryption (REQ-AGENT-006).
//!
//! Each turn is sealed with AES-256-GCM under a key derived (HKDF-SHA256) from the
//! cross-device **pairing secret** — a high-entropy key the user shares between their
//! devices, *not* the device-local token-cache key (which would block cross-device
//! decryption). A random salt + nonce are stored per file; the AAD binds
//! `schema_version | session_id | ulid`, so tampering any of those fails decryption.
//! Only the turn ciphertext (which carries M365 excerpts) is stored; the envelope's
//! metadata is just IDs.
//!
//! **Recoverability:** the pairing secret is the only way to decrypt. Lose it on every
//! device and the history is unrecoverable by design (privacy over recoverability).

use crate::AgentError;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ring::rand::{SecureRandom, SystemRandom};
use ring::{aead, hkdf};
use serde::{Deserialize, Serialize};

/// Envelope schema version (also part of the AEAD AAD).
pub const SCHEMA_VERSION: u32 = 1;
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const SALT_LEN: usize = 16;
const HKDF_INFO: &[u8] = b"isyncyou-agent-session-key-v1";

/// One encrypted turn file (the bytes written per `<ulid>.json`). Envelope fields are
/// cleartext IDs; `ct` is the AES-256-GCM ciphertext+tag of the turn JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SealedTurn {
    pub v: u32,
    pub session_id: String,
    pub ulid: String,
    pub salt: String,
    pub nonce: String,
    pub ct: String,
}

struct KeyLen(usize);
impl hkdf::KeyType for KeyLen {
    fn len(&self) -> usize {
        self.0
    }
}

fn crypto_err(what: &str) -> AgentError {
    AgentError::Provider(format!("session crypto: {what}"))
}

fn aad_string(v: u32, session_id: &str, ulid: &str) -> String {
    format!("{v}|{session_id}|{ulid}")
}

fn derive_key(pairing_secret: &[u8], salt: &[u8]) -> Result<[u8; KEY_LEN], AgentError> {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, salt);
    let prk = salt.extract(pairing_secret);
    let okm = prk
        .expand(&[HKDF_INFO], KeyLen(KEY_LEN))
        .map_err(|_| crypto_err("hkdf expand"))?;
    let mut key = [0u8; KEY_LEN];
    okm.fill(&mut key).map_err(|_| crypto_err("hkdf fill"))?;
    Ok(key)
}

/// Seal a turn's plaintext into a [`SealedTurn`] envelope.
pub fn seal(
    pairing_secret: &[u8],
    session_id: &str,
    ulid: &str,
    plaintext: &[u8],
) -> Result<SealedTurn, AgentError> {
    let rng = SystemRandom::new();
    let mut salt = [0u8; SALT_LEN];
    rng.fill(&mut salt).map_err(|_| crypto_err("rng salt"))?;
    let mut nonce = [0u8; NONCE_LEN];
    rng.fill(&mut nonce).map_err(|_| crypto_err("rng nonce"))?;

    let key = derive_key(pairing_secret, &salt)?;
    let unbound =
        aead::UnboundKey::new(&aead::AES_256_GCM, &key).map_err(|_| crypto_err("aead key"))?;
    let sealing = aead::LessSafeKey::new(unbound);
    let aad = aad_string(SCHEMA_VERSION, session_id, ulid);
    let mut in_out = plaintext.to_vec();
    sealing
        .seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(aad.as_bytes()),
            &mut in_out,
        )
        .map_err(|_| crypto_err("seal"))?;

    Ok(SealedTurn {
        v: SCHEMA_VERSION,
        session_id: session_id.to_string(),
        ulid: ulid.to_string(),
        salt: B64.encode(salt),
        nonce: B64.encode(nonce),
        ct: B64.encode(&in_out),
    })
}

/// Open a [`SealedTurn`] back to plaintext. Fails if the pairing secret is wrong or any
/// AAD-bound field (`v`/`session_id`/`ulid`) was tampered.
pub fn open(pairing_secret: &[u8], sealed: &SealedTurn) -> Result<Vec<u8>, AgentError> {
    let salt = B64
        .decode(&sealed.salt)
        .map_err(|_| crypto_err("b64 salt"))?;
    let nonce_v = B64
        .decode(&sealed.nonce)
        .map_err(|_| crypto_err("b64 nonce"))?;
    let mut ct = B64.decode(&sealed.ct).map_err(|_| crypto_err("b64 ct"))?;
    if nonce_v.len() != NONCE_LEN {
        return Err(crypto_err("bad nonce length"));
    }
    let key = derive_key(pairing_secret, &salt)?;
    let unbound =
        aead::UnboundKey::new(&aead::AES_256_GCM, &key).map_err(|_| crypto_err("aead key"))?;
    let opening = aead::LessSafeKey::new(unbound);
    let nonce: [u8; NONCE_LEN] = nonce_v.try_into().map_err(|_| crypto_err("nonce"))?;
    let aad = aad_string(sealed.v, &sealed.session_id, &sealed.ulid);
    let plaintext = opening
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(aad.as_bytes()),
            &mut ct,
        )
        .map_err(|_| crypto_err("decryption failed (wrong pairing key or tampered)"))?;
    Ok(plaintext.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_with_the_pairing_secret() {
        let key = b"a-high-entropy-pairing-secret-32b";
        let sealed = seal(key, "sess1", "01ULID", b"hello M365 excerpt").unwrap();
        assert_eq!(open(key, &sealed).unwrap(), b"hello M365 excerpt");
    }

    #[test]
    fn a_different_device_local_key_cannot_decrypt() {
        let paired = b"the-shared-pairing-secret-value!!";
        let device_local = b"some-other-device-local-only-key!";
        let sealed = seal(paired, "sess1", "01ULID", b"secret body").unwrap();
        assert!(open(device_local, &sealed).is_err());
    }

    #[test]
    fn tampering_aad_fields_fails_decryption() {
        let key = b"a-high-entropy-pairing-secret-32b";
        let sealed = seal(key, "sess1", "01ULID", b"body").unwrap();
        let mut t = sealed.clone();
        t.ulid = "02OTHER".into(); // AAD bind broken
        assert!(open(key, &t).is_err());
        let mut t2 = sealed.clone();
        t2.session_id = "other".into();
        assert!(open(key, &t2).is_err());
        let mut t3 = sealed.clone();
        t3.v = 999;
        assert!(open(key, &t3).is_err());
    }

    #[test]
    fn ciphertext_contains_no_plaintext() {
        let key = b"a-high-entropy-pairing-secret-32b";
        let sealed = seal(key, "sess1", "01ULID", b"SENSITIVE-MAIL-BODY").unwrap();
        let blob = serde_json::to_string(&sealed).unwrap();
        assert!(!blob.contains("SENSITIVE-MAIL-BODY"));
    }
}
