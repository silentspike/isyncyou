//! Typed, encrypted-at-rest credential storage (REQ-AGENT-008 / S-AG.5).
//!
//! Provider API keys, OAuth refresh tokens, and the session pairing key are **distinct**
//! secret classes; each is stored in its own AES-256-GCM file (owner-only `0600` on
//! Unix), with the AEAD AAD binding `class | id | version` so a wrong-class load is
//! rejected. Secret values are wrapped in [`Secret`], whose `Debug` redacts the bytes,
//! so a secret can never be logged by accident.
//!
//! The at-rest key comes from an [`AtRestKey`]: [`LocalKey`] resolves it from an env var
//! or an auto-generated owner-only key file; [`ProvidedKey`] takes a key handed in by the
//! caller — the seam for Android, where the key is unwrapped by the Android Keystore on
//! the Kotlin side (#626) and passed to Rust.

use crate::AgentError;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ring::rand::{SecureRandom, SystemRandom};
use ring::{aead, digest};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const ENVELOPE_VERSION: u32 = 1;
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;

/// The distinct classes of secret the agent stores. Mixing them is rejected by the AAD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretClass {
    ProviderApiKey,
    ProviderOAuthRefresh,
    SessionPairingKey,
}

impl SecretClass {
    pub fn tag(&self) -> &'static str {
        match self {
            SecretClass::ProviderApiKey => "provider-api-key",
            SecretClass::ProviderOAuthRefresh => "provider-oauth-refresh",
            SecretClass::SessionPairingKey => "session-pairing-key",
        }
    }
}

/// A secret value whose `Debug` is redacted — it can never be logged in the clear.
#[derive(Clone, PartialEq)]
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Secret(bytes.into())
    }
    /// Borrow the raw secret bytes. The only way to read the value — call sites are easy
    /// to audit.
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret([redacted; {} bytes])", self.0.len())
    }
}

/// Source of the 32-byte at-rest encryption key.
pub trait AtRestKey {
    fn key(&self) -> Result<[u8; KEY_LEN], AgentError>;
}

/// Env var holding a base64 32-byte at-rest key (highest precedence).
pub const CRED_KEY_ENV: &str = "ISYNCYOU_AGENT_CRED_KEY";

/// Resolves the at-rest key from `ISYNCYOU_AGENT_CRED_KEY`, else an owner-only key file
/// beside the store (auto-generated on first use — encrypted-at-rest by default).
pub struct LocalKey {
    key_file: PathBuf,
}

impl LocalKey {
    pub fn new(key_file: impl Into<PathBuf>) -> Self {
        Self {
            key_file: key_file.into(),
        }
    }
}

impl AtRestKey for LocalKey {
    fn key(&self) -> Result<[u8; KEY_LEN], AgentError> {
        if let Ok(b64) = std::env::var(CRED_KEY_ENV) {
            let raw = B64
                .decode(b64.trim())
                .map_err(|_| AgentError::Provider(format!("{CRED_KEY_ENV} is not valid base64")))?;
            return raw
                .try_into()
                .map_err(|_| AgentError::Provider(format!("{CRED_KEY_ENV} must be 32 bytes")));
        }
        if self.key_file.exists() {
            let raw =
                std::fs::read(&self.key_file).map_err(|e| AgentError::Provider(e.to_string()))?;
            return raw
                .try_into()
                .map_err(|_| AgentError::Provider("at-rest key file must be 32 bytes".into()));
        }
        // Auto-generate an owner-only local key (secure-by-default).
        let mut key = [0u8; KEY_LEN];
        SystemRandom::new()
            .fill(&mut key)
            .map_err(|_| AgentError::Provider("rng".into()))?;
        if let Some(parent) = self.key_file.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AgentError::Provider(e.to_string()))?;
        }
        write_owner_only(&self.key_file, &key)?;
        Ok(key)
    }
}

/// An at-rest key supplied by the caller (e.g. unwrapped from the Android Keystore).
pub struct ProvidedKey(pub [u8; KEY_LEN]);

impl AtRestKey for ProvidedKey {
    fn key(&self) -> Result<[u8; KEY_LEN], AgentError> {
        Ok(self.0)
    }
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    v: u32,
    class: String,
    id: String,
    nonce: String,
    ct: String,
}

fn aad(class: &SecretClass, id: &str) -> String {
    format!("{}|{}|{}", ENVELOPE_VERSION, class.tag(), id)
}

fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<(), AgentError> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| AgentError::Provider(e.to_string()))?;
        f.write_all(bytes)
            .map_err(|e| AgentError::Provider(e.to_string()))?;
        // Tighten even if the file pre-existed with a looser mode.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| AgentError::Provider(e.to_string()))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes).map_err(|e| AgentError::Provider(e.to_string()))?;
    }
    Ok(())
}

/// Typed, encrypted-at-rest credential store over a directory.
pub struct CredentialStore<K: AtRestKey> {
    dir: PathBuf,
    key: K,
}

impl<K: AtRestKey> CredentialStore<K> {
    pub fn new(dir: impl Into<PathBuf>, key: K) -> Self {
        Self {
            dir: dir.into(),
            key,
        }
    }

    fn path(&self, class: &SecretClass, id: &str) -> PathBuf {
        // Hash the id for a safe, collision-resistant filename; the id is also bound in
        // the AAD, so a wrong file can never be decrypted under the wrong id.
        let h = digest::digest(&digest::SHA256, id.as_bytes());
        let short = h.as_ref()[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        self.dir.join(format!("{}__{}.cred", class.tag(), short))
    }

    /// Store a secret (overwriting any existing one for `(class, id)`), owner-only.
    pub fn put(&self, class: SecretClass, id: &str, secret: &Secret) -> Result<(), AgentError> {
        let key = self.key.key()?;
        let rng = SystemRandom::new();
        let mut nonce = [0u8; NONCE_LEN];
        rng.fill(&mut nonce)
            .map_err(|_| AgentError::Provider("rng".into()))?;
        let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, &key)
            .map_err(|_| AgentError::Provider("aead key".into()))?;
        let sealing = aead::LessSafeKey::new(unbound);
        let aad = aad(&class, id);
        let mut in_out = secret.0.clone();
        sealing
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(aad.as_bytes()),
                &mut in_out,
            )
            .map_err(|_| AgentError::Provider("seal".into()))?;
        let env = Envelope {
            v: ENVELOPE_VERSION,
            class: class.tag().to_string(),
            id: id.to_string(),
            nonce: B64.encode(nonce),
            ct: B64.encode(&in_out),
        };
        std::fs::create_dir_all(&self.dir).map_err(|e| AgentError::Provider(e.to_string()))?;
        let bytes = serde_json::to_vec(&env).map_err(|e| AgentError::Provider(e.to_string()))?;
        write_owner_only(&self.path(&class, id), &bytes)
    }

    /// Load a secret for `(class, id)`; `None` if not present. A file written under a
    /// different class fails to decrypt (AAD mismatch) rather than returning it.
    pub fn get(&self, class: SecretClass, id: &str) -> Result<Option<Secret>, AgentError> {
        let path = self.path(&class, id);
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read(&path).map_err(|e| AgentError::Provider(e.to_string()))?;
        let env: Envelope =
            serde_json::from_slice(&raw).map_err(|e| AgentError::Provider(e.to_string()))?;
        let key = self.key.key()?;
        let nonce_v = B64
            .decode(&env.nonce)
            .map_err(|_| AgentError::Provider("b64 nonce".into()))?;
        let mut ct = B64
            .decode(&env.ct)
            .map_err(|_| AgentError::Provider("b64 ct".into()))?;
        let nonce: [u8; NONCE_LEN] = nonce_v
            .try_into()
            .map_err(|_| AgentError::Provider("bad nonce".into()))?;
        let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, &key)
            .map_err(|_| AgentError::Provider("aead key".into()))?;
        let opening = aead::LessSafeKey::new(unbound);
        // AAD is recomputed from the *requested* class+id; a cross-class read fails here.
        let aad = aad(&class, id);
        let plaintext = opening
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(aad.as_bytes()),
                &mut ct,
            )
            .map_err(|_| AgentError::Provider("credential decryption failed".into()))?;
        Ok(Some(Secret(plaintext.to_vec())))
    }

    /// Delete a secret if present.
    pub fn delete(&self, class: SecretClass, id: &str) -> Result<(), AgentError> {
        let path = self.path(&class, id);
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| AgentError::Provider(e.to_string()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &Path) -> CredentialStore<LocalKey> {
        CredentialStore::new(dir, LocalKey::new(dir.join(".cred.key")))
    }

    #[test]
    fn each_class_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        for (class, val) in [
            (SecretClass::ProviderApiKey, b"sk-test-api-key".as_slice()),
            (
                SecretClass::ProviderOAuthRefresh,
                b"refresh-token-xyz".as_slice(),
            ),
            (
                SecretClass::SessionPairingKey,
                b"pairing-secret-32-bytes!!!!!!!!!!".as_slice(),
            ),
        ] {
            s.put(class, "acct1", &Secret::new(val)).unwrap();
            assert_eq!(s.get(class, "acct1").unwrap().unwrap().expose(), val);
        }
    }

    #[test]
    fn wrong_class_is_rejected_no_cross_class_confusion() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.put(
            SecretClass::ProviderApiKey,
            "acct1",
            &Secret::new(b"k".as_slice()),
        )
        .unwrap();
        // A different class for the same id resolves to a different file -> None.
        assert!(s
            .get(SecretClass::SessionPairingKey, "acct1")
            .unwrap()
            .is_none());
    }

    #[test]
    fn provided_key_path_works_and_differs_from_local() {
        let tmp = tempfile::tempdir().unwrap();
        let provided = CredentialStore::new(tmp.path(), ProvidedKey([7u8; 32]));
        provided
            .put(
                SecretClass::ProviderApiKey,
                "a",
                &Secret::new(b"v".as_slice()),
            )
            .unwrap();
        assert_eq!(
            provided
                .get(SecretClass::ProviderApiKey, "a")
                .unwrap()
                .unwrap()
                .expose(),
            b"v"
        );
        // A store with a *different* key cannot decrypt it.
        let other = CredentialStore::new(tmp.path(), ProvidedKey([9u8; 32]));
        assert!(other.get(SecretClass::ProviderApiKey, "a").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn files_are_owner_only_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.put(
            SecretClass::ProviderApiKey,
            "a",
            &Secret::new(b"v".as_slice()),
        )
        .unwrap();
        let path = s.path(&SecretClass::ProviderApiKey, "a");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let keymode = std::fs::metadata(tmp.path().join(".cred.key"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(keymode, 0o600);
    }

    #[test]
    fn secret_debug_is_redacted() {
        let s = Secret::new(b"super-secret-value".as_slice());
        let shown = format!("{s:?}");
        assert!(
            !shown.contains("super-secret-value"),
            "Debug must not leak: {shown}"
        );
        assert!(shown.contains("redacted"));
    }

    #[test]
    fn local_key_persists_across_opens() {
        let tmp = tempfile::tempdir().unwrap();
        store(tmp.path())
            .put(
                SecretClass::ProviderApiKey,
                "a",
                &Secret::new(b"persisted".as_slice()),
            )
            .unwrap();
        // A fresh store over the same dir reuses the auto-generated key file.
        let reopened = store(tmp.path());
        assert_eq!(
            reopened
                .get(SecretClass::ProviderApiKey, "a")
                .unwrap()
                .unwrap()
                .expose(),
            b"persisted"
        );
    }

    #[test]
    fn delete_removes_the_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.put(
            SecretClass::ProviderApiKey,
            "a",
            &Secret::new(b"v".as_slice()),
        )
        .unwrap();
        s.delete(SecretClass::ProviderApiKey, "a").unwrap();
        assert!(s.get(SecretClass::ProviderApiKey, "a").unwrap().is_none());
    }
}
