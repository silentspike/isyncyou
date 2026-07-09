//! Typed, encrypted-at-rest credential storage (REQ-AGENT-010 / S-AG.5).
//!
//! Provider API keys, OAuth refresh tokens, and the session pairing key are **distinct**
//! secret classes; each is stored in its own AES-256-GCM file (owner-only `0600` on
//! Unix), with canonical AEAD AAD binding `version`, `class`, and `id` so a
//! wrong-class or wrong-id load is rejected. Secret values are wrapped in [`Secret`],
//! whose `Debug` redacts the bytes, so a secret can never be logged by accident.
//!
//! The at-rest key comes from an [`AtRestKey`]: [`LocalKey`] resolves it from an env var
//! or an auto-generated owner-only key file; [`ProvidedKey`] takes a key handed in by the
//! caller — the seam for Android, where the key is unwrapped by the Android Keystore on
//! the Kotlin side (#626) and passed to Rust.

use crate::AgentError;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ring::rand::{SecureRandom, SystemRandom};
use ring::{aead, digest, hkdf};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const ENVELOPE_VERSION: u32 = 2;
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const AAD_DOMAIN: &[u8] = b"isyncyou-agent-credential-aad-v1";
const HKDF_SALT: &[u8] = b"isyncyou-agent-credential-store-v1";

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
            tighten_owner_only(&self.key_file)?;
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

struct KeyLen(usize);

impl hkdf::KeyType for KeyLen {
    fn len(&self) -> usize {
        self.0
    }
}

fn credential_err(message: &str) -> AgentError {
    AgentError::Provider(message.to_string())
}

fn push_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).expect("credential AAD field length fits u32");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

fn aad(class: SecretClass, id: &str) -> Vec<u8> {
    let mut out = Vec::new();
    push_len_prefixed(&mut out, AAD_DOMAIN);
    push_len_prefixed(&mut out, &ENVELOPE_VERSION.to_be_bytes());
    push_len_prefixed(&mut out, class.tag().as_bytes());
    push_len_prefixed(&mut out, id.as_bytes());
    out
}

fn class_key(master: &[u8; KEY_LEN], class: SecretClass) -> Result<[u8; KEY_LEN], AgentError> {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, HKDF_SALT);
    let prk = salt.extract(master);
    let info = [b"class:".as_slice(), class.tag().as_bytes()];
    let okm = prk
        .expand(&info, KeyLen(KEY_LEN))
        .map_err(|_| credential_err("credential key derivation failed"))?;
    let mut out = [0u8; KEY_LEN];
    okm.fill(&mut out)
        .map_err(|_| credential_err("credential key derivation failed"))?;
    Ok(out)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn hash_id_full(id: &str) -> String {
    hex_encode(digest::digest(&digest::SHA256, id.as_bytes()).as_ref())
}

fn hash_id_legacy(id: &str) -> String {
    hex_encode(&digest::digest(&digest::SHA256, id.as_bytes()).as_ref()[..8])
}

fn validate_envelope(env: &Envelope, class: SecretClass, id: &str) -> Result<(), AgentError> {
    if env.v != ENVELOPE_VERSION || env.class != class.tag() || env.id != id {
        return Err(credential_err("credential envelope metadata mismatch"));
    }
    Ok(())
}

fn decode_nonce(encoded: &str) -> Result<[u8; NONCE_LEN], AgentError> {
    let raw = B64
        .decode(encoded)
        .map_err(|_| credential_err("credential envelope nonce is invalid"))?;
    raw.try_into()
        .map_err(|_| credential_err("credential envelope nonce is invalid"))
}

fn decode_ciphertext(encoded: &str) -> Result<Vec<u8>, AgentError> {
    let ct = B64
        .decode(encoded)
        .map_err(|_| credential_err("credential envelope ciphertext is invalid"))?;
    if ct.is_empty() {
        return Err(credential_err("credential envelope ciphertext is invalid"));
    }
    Ok(ct)
}

fn random_temp_path(path: &Path) -> Result<PathBuf, AgentError> {
    let mut suffix = [0u8; 8];
    SystemRandom::new()
        .fill(&mut suffix)
        .map_err(|_| credential_err("credential temp path rng failed"))?;
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("secret");
    Ok(path.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        hex_encode(&suffix)
    )))
}

#[cfg(unix)]
fn tighten_owner_only(path: &Path) -> Result<(), AgentError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| AgentError::Provider(e.to_string()))
}

#[cfg(not(unix))]
fn tighten_owner_only(_path: &Path) -> Result<(), AgentError> {
    Ok(())
}

fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<(), AgentError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AgentError::Provider(e.to_string()))?;
    }
    let tmp = random_temp_path(path)?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| AgentError::Provider(e.to_string()))?;
        f.write_all(bytes)
            .map_err(|e| AgentError::Provider(e.to_string()))?;
        f.sync_all()
            .map_err(|e| AgentError::Provider(e.to_string()))?;
        drop(f);
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| AgentError::Provider(e.to_string()))?;
        std::fs::rename(&tmp, path).map_err(|e| AgentError::Provider(e.to_string()))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| AgentError::Provider(e.to_string()))?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::File::open(parent).and_then(|d| d.sync_all());
        }
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&tmp, bytes).map_err(|e| AgentError::Provider(e.to_string()))?;
        std::fs::rename(&tmp, path).map_err(|e| AgentError::Provider(e.to_string()))?;
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
        // Hash the id for a safe filename; the full id is also stored in the envelope
        // metadata and canonical AAD, so a wrong file can never decrypt under another id.
        self.dir
            .join(format!("{}__{}.cred", class.tag(), hash_id_full(id)))
    }

    fn legacy_path(&self, class: &SecretClass, id: &str) -> PathBuf {
        self.dir
            .join(format!("{}__{}.cred", class.tag(), hash_id_legacy(id)))
    }

    fn existing_path(&self, class: &SecretClass, id: &str) -> PathBuf {
        let path = self.path(class, id);
        if path.exists() {
            path
        } else {
            self.legacy_path(class, id)
        }
    }

    /// Store a secret (overwriting any existing one for `(class, id)`), owner-only.
    pub fn put(&self, class: SecretClass, id: &str, secret: &Secret) -> Result<(), AgentError> {
        if secret.expose().is_empty() {
            return Err(credential_err("credential secret must not be empty"));
        }
        let master = self.key.key()?;
        let key = class_key(&master, class)?;
        let rng = SystemRandom::new();
        let mut nonce = [0u8; NONCE_LEN];
        rng.fill(&mut nonce)
            .map_err(|_| credential_err("credential nonce rng failed"))?;
        let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, &key)
            .map_err(|_| credential_err("credential aead key setup failed"))?;
        let sealing = aead::LessSafeKey::new(unbound);
        let aad = aad(class, id);
        let mut in_out = secret.0.clone();
        sealing
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(aad.as_slice()),
                &mut in_out,
            )
            .map_err(|_| credential_err("credential encryption failed"))?;
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
        let path = self.existing_path(&class, id);
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read(&path).map_err(|e| AgentError::Provider(e.to_string()))?;
        let env: Envelope =
            serde_json::from_slice(&raw).map_err(|e| AgentError::Provider(e.to_string()))?;
        validate_envelope(&env, class, id)?;
        let nonce = decode_nonce(&env.nonce)?;
        let mut ct = decode_ciphertext(&env.ct)?;
        let master = self.key.key()?;
        let key = class_key(&master, class)?;
        let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, &key)
            .map_err(|_| credential_err("credential aead key setup failed"))?;
        let opening = aead::LessSafeKey::new(unbound);
        let aad = aad(class, id);
        let plaintext = opening
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(aad.as_slice()),
                &mut ct,
            )
            .map_err(|_| credential_err("credential decryption failed"))?;
        if plaintext.is_empty() {
            return Err(credential_err("credential plaintext is invalid"));
        }
        Ok(Some(Secret(plaintext.to_vec())))
    }

    /// Delete a secret if present.
    pub fn delete(&self, class: SecretClass, id: &str) -> Result<(), AgentError> {
        let path = self.path(&class, id);
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| AgentError::Provider(e.to_string()))?;
        }
        let legacy = self.legacy_path(&class, id);
        if legacy.exists() {
            std::fs::remove_file(&legacy).map_err(|e| AgentError::Provider(e.to_string()))?;
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

    fn read_env(path: &Path) -> Envelope {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    fn write_env(path: &Path, env: &Envelope) {
        let bytes = serde_json::to_vec(env).unwrap();
        write_owner_only(path, &bytes).unwrap();
    }

    fn err_text<T: std::fmt::Debug>(r: Result<T, AgentError>) -> String {
        r.unwrap_err().to_string()
    }

    fn assert_bytes_do_not_contain(haystack: &[u8], needle: &[u8]) {
        assert!(
            !haystack.windows(needle.len()).any(|w| w == needle),
            "plaintext sentinel must not be present"
        );
    }

    #[test]
    fn secrets_each_class_round_trips() {
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
    fn secrets_empty_secret_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let err = err_text(s.put(
            SecretClass::ProviderApiKey,
            "acct1",
            &Secret::new(Vec::new()),
        ));
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn secrets_wrong_class_file_copy_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.put(
            SecretClass::ProviderApiKey,
            "acct1",
            &Secret::new(b"sk-wrong-class-sentinel".as_slice()),
        )
        .unwrap();
        let api_path = s.path(&SecretClass::ProviderApiKey, "acct1");
        let pairing_path = s.path(&SecretClass::SessionPairingKey, "acct1");
        std::fs::copy(api_path, pairing_path).unwrap();
        let err = err_text(s.get(SecretClass::SessionPairingKey, "acct1"));
        assert!(err.contains("metadata mismatch"));
        assert!(!err.contains("sk-wrong-class-sentinel"));
    }

    #[test]
    fn secrets_envelope_class_mutation_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.put(
            SecretClass::ProviderApiKey,
            "acct1",
            &Secret::new(b"class-mutation-secret".as_slice()),
        )
        .unwrap();
        let path = s.path(&SecretClass::ProviderApiKey, "acct1");
        let mut env = read_env(&path);
        env.class = SecretClass::SessionPairingKey.tag().to_string();
        write_env(&path, &env);
        let err = err_text(s.get(SecretClass::ProviderApiKey, "acct1"));
        assert!(err.contains("metadata mismatch"));
        assert!(!err.contains("class-mutation-secret"));
    }

    #[test]
    fn secrets_envelope_id_mutation_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.put(
            SecretClass::ProviderApiKey,
            "acct1",
            &Secret::new(b"id-mutation-secret".as_slice()),
        )
        .unwrap();
        let path = s.path(&SecretClass::ProviderApiKey, "acct1");
        let mut env = read_env(&path);
        env.id = "acct2".to_string();
        write_env(&path, &env);
        let err = err_text(s.get(SecretClass::ProviderApiKey, "acct1"));
        assert!(err.contains("metadata mismatch"));
        assert!(!err.contains("id-mutation-secret"));
    }

    #[test]
    fn secrets_envelope_version_mutation_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.put(
            SecretClass::ProviderApiKey,
            "acct1",
            &Secret::new(b"version-mutation-secret".as_slice()),
        )
        .unwrap();
        let path = s.path(&SecretClass::ProviderApiKey, "acct1");
        let mut env = read_env(&path);
        env.v = ENVELOPE_VERSION + 1;
        write_env(&path, &env);
        let err = err_text(s.get(SecretClass::ProviderApiKey, "acct1"));
        assert!(err.contains("metadata mismatch"));
        assert!(!err.contains("version-mutation-secret"));
    }

    #[test]
    fn secrets_id_delimiter_collision_does_not_decrypt() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let id = "acct|provider-api-key|v2";
        let other_id = "acct|provider-api-key";
        s.put(
            SecretClass::ProviderApiKey,
            id,
            &Secret::new(b"delimiter-secret".as_slice()),
        )
        .unwrap();
        assert!(s
            .get(SecretClass::ProviderApiKey, other_id)
            .unwrap()
            .is_none());
        std::fs::copy(
            s.path(&SecretClass::ProviderApiKey, id),
            s.path(&SecretClass::ProviderApiKey, other_id),
        )
        .unwrap();
        let err = err_text(s.get(SecretClass::ProviderApiKey, other_id));
        assert!(err.contains("metadata mismatch"));
        assert!(!err.contains("delimiter-secret"));
    }

    #[test]
    fn secrets_ciphertext_does_not_contain_plaintext_sentinel() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let sentinel = b"PLAINTEXT-CREDENTIAL-SENTINEL";
        s.put(
            SecretClass::ProviderOAuthRefresh,
            "acct1",
            &Secret::new(sentinel.as_slice()),
        )
        .unwrap();
        let raw = std::fs::read(s.path(&SecretClass::ProviderOAuthRefresh, "acct1")).unwrap();
        assert_bytes_do_not_contain(&raw, sentinel);
    }

    #[test]
    fn secrets_provided_key_path_works_and_differs_from_local() {
        let tmp = tempfile::tempdir().unwrap();
        let provided = CredentialStore::new(tmp.path(), ProvidedKey([7u8; 32]));
        provided
            .put(
                SecretClass::ProviderApiKey,
                "a",
                &Secret::new(b"wrong-key-secret".as_slice()),
            )
            .unwrap();
        assert_eq!(
            provided
                .get(SecretClass::ProviderApiKey, "a")
                .unwrap()
                .unwrap()
                .expose(),
            b"wrong-key-secret"
        );
        // A store with a *different* key cannot decrypt it.
        let other = CredentialStore::new(tmp.path(), ProvidedKey([9u8; 32]));
        let err = err_text(other.get(SecretClass::ProviderApiKey, "a"));
        assert!(err.contains("decryption failed"));
        assert!(!err.contains("wrong-key-secret"));
    }

    #[cfg(unix)]
    #[test]
    fn secrets_files_are_owner_only_on_create_and_rewrite() {
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
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        s.put(
            SecretClass::ProviderApiKey,
            "a",
            &Secret::new(b"v2".as_slice()),
        )
        .unwrap();
        let rewritten = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(rewritten, 0o600);

        let key_path = tmp.path().join(".cred.key");
        let keymode = std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(keymode, 0o600);
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            s.get(SecretClass::ProviderApiKey, "a")
                .unwrap()
                .unwrap()
                .expose(),
            b"v2"
        );
        let tightened = std::fs::metadata(key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(tightened, 0o600);
    }

    #[test]
    fn secrets_debug_and_error_redact_secret_values() {
        let s = Secret::new(b"super-secret-value".as_slice());
        let shown = format!("{s:?}");
        assert!(
            !shown.contains("super-secret-value"),
            "Debug must not leak: {shown}"
        );
        assert!(shown.contains("redacted"));

        let tmp = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(tmp.path(), ProvidedKey([1u8; 32]));
        store
            .put(
                SecretClass::ProviderApiKey,
                "a",
                &Secret::new(b"super-secret-value".as_slice()),
            )
            .unwrap();
        let wrong = CredentialStore::new(tmp.path(), ProvidedKey([2u8; 32]));
        let err = err_text(wrong.get(SecretClass::ProviderApiKey, "a"));
        assert!(!err.contains("super-secret-value"));
    }

    #[test]
    fn secrets_local_key_persists_across_opens() {
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
    fn secrets_delete_removes_the_secret() {
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
