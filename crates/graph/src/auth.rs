//! OAuth for personal Microsoft accounts: a persisted token cache (always
//! available, pure) and the device-code / refresh network flow (feature `http`).
//!
//! Personal accounts use the `consumers` authority and a public client (no
//! secret). The interactive device-code login needs a human once; afterwards the
//! cached refresh token renews access silently.

use ring::{aead, pbkdf2, rand};
use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

/// `consumers` OAuth 2.0 v2 endpoint base.
pub const AUTHORITY: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0";

const TOKEN_CACHE_MAGIC: &str = "isyncyou-token-cache-encrypted-v1";
const TOKEN_CACHE_KEYRING_MAGIC: &str = "isyncyou-token-cache-keyring-v1";
const TOKEN_CACHE_KEYRING_SERVICE: &str = "org.silentspike.isyncyou.token-cache";
const TOKEN_CACHE_KDF: &str = "pbkdf2-hmac-sha256";
const TOKEN_CACHE_AEAD: &str = "aes-256-gcm";
const TOKEN_CACHE_KDF_ITERS: u32 = 210_000;
const TOKEN_CACHE_SALT_LEN: usize = 16;
const TOKEN_CACHE_NONCE_LEN: usize = 12;
const TOKEN_CACHE_KEY_LEN: usize = 32;
const TOKEN_CACHE_SECRET_ENV: &str = "ISYNCYOU_TOKEN_CACHE_KEY";
const TOKEN_CACHE_SECRET_FILE_ENV: &str = "ISYNCYOU_TOKEN_CACHE_KEY_FILE";
const TOKEN_CACHE_SYSTEMD_CREDENTIAL: &str = "isyncyou-token-cache-key";
const SYSTEMD_CREDENTIALS_DIR_ENV: &str = "CREDENTIALS_DIRECTORY";

/// Raw token-endpoint response.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: u64,
}

/// Our own persisted token state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenCache {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix seconds at which the access token should be considered expired.
    pub expires_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EncryptedTokenCache {
    isyncyou_token_cache: String,
    kdf: String,
    aead: String,
    iterations: u32,
    salt_hex: String,
    nonce_hex: String,
    ciphertext_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KeyringTokenCache {
    isyncyou_token_cache: String,
    keyring_service: String,
    keyring_user: String,
}

impl TokenCache {
    /// Build from a token response, expiring a minute early for safety.
    pub fn from_response(r: &TokenResponse, now_unix: u64) -> Self {
        TokenCache {
            access_token: r.access_token.clone(),
            refresh_token: r.refresh_token.clone(),
            expires_at: now_unix.saturating_add(r.expires_in.saturating_sub(60)),
        }
    }

    /// Whether the cached access token is present and not yet expired.
    pub fn is_access_valid(&self, now_unix: u64) -> bool {
        !self.access_token.is_empty() && now_unix < self.expires_at
    }

    pub fn load(path: &Path) -> std::io::Result<Self> {
        let bytes = std::fs::read(path)?;
        match token_cache_file_kind(&bytes)? {
            TokenCacheFileKind::Keyring(marker) => Self::load_from_keyring_marker(&marker),
            TokenCacheFileKind::Encrypted => {
                let secret = match token_cache_secret()? {
                    Some(secret) => secret,
                    // No explicit secret configured: fall back to the auto-generated
                    // owner-only local key written by `save` (secure-by-default). If
                    // that is also absent, the cache was sealed with an out-of-band
                    // secret that is no longer available.
                    None => read_local_key(path)?.ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            encrypted_cache_needs_secret_message(),
                        )
                    })?,
                };
                Self::load_encrypted_with_secret_bytes(&bytes, &secret)
            }
            TokenCacheFileKind::Plain => {
                serde_json::from_slice(&bytes).map_err(std::io::Error::other)
            }
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(marker) = existing_keyring_marker(path)? {
            self.save_to_keyring_marker(path, &marker)?;
            return Ok(());
        }
        // Secure-by-default: with no explicit secret (env/file/systemd) and no
        // keyring backend, encrypt the cache with an auto-generated, owner-only
        // local key instead of writing plaintext. See `local_key_path` for scope.
        let secret = match token_cache_secret()? {
            Some(secret) => secret,
            None => load_or_create_local_key(path)?,
        };
        let bytes = self.encrypted_bytes_with_secret(&secret)?;
        write_token_cache(path, &bytes)
    }

    pub fn save_to_keyring(&self, path: &Path) -> std::io::Result<()> {
        let marker = keyring_marker_for_path(path);
        self.save_to_keyring_marker(path, &marker)
    }

    pub fn load_encrypted_with_secret(path: &Path, secret: &[u8]) -> std::io::Result<Self> {
        let bytes = std::fs::read(path)?;
        Self::load_encrypted_with_secret_bytes(&bytes, secret)
    }

    pub fn save_encrypted_with_secret(&self, path: &Path, secret: &[u8]) -> std::io::Result<()> {
        let bytes = self.encrypted_bytes_with_secret(secret)?;
        write_token_cache(path, &bytes)
    }

    fn encrypted_bytes_with_secret(&self, secret: &[u8]) -> std::io::Result<Vec<u8>> {
        if secret.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "token cache encryption secret must not be empty",
            ));
        }
        let salt = random_bytes(TOKEN_CACHE_SALT_LEN)?;
        let nonce = random_bytes(TOKEN_CACHE_NONCE_LEN)?;
        let mut key = derive_token_cache_key(secret, &salt, TOKEN_CACHE_KDF_ITERS)?;
        let plaintext = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        let ciphertext = seal_token_cache(&mut key, &nonce, &plaintext)?;
        key.fill(0);
        serde_json::to_vec_pretty(&EncryptedTokenCache {
            isyncyou_token_cache: TOKEN_CACHE_MAGIC.into(),
            kdf: TOKEN_CACHE_KDF.into(),
            aead: TOKEN_CACHE_AEAD.into(),
            iterations: TOKEN_CACHE_KDF_ITERS,
            salt_hex: hex_encode(&salt),
            nonce_hex: hex_encode(&nonce),
            ciphertext_hex: hex_encode(&ciphertext),
        })
        .map_err(std::io::Error::other)
    }

    fn load_encrypted_with_secret_bytes(bytes: &[u8], secret: &[u8]) -> std::io::Result<Self> {
        if secret.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "token cache encryption secret must not be empty",
            ));
        }
        let env: EncryptedTokenCache =
            serde_json::from_slice(bytes).map_err(std::io::Error::other)?;
        validate_encrypted_token_cache_header(&env)?;
        let salt = hex_decode(&env.salt_hex)?;
        let nonce = hex_decode(&env.nonce_hex)?;
        let mut ciphertext = hex_decode(&env.ciphertext_hex)?;
        let mut key = derive_token_cache_key(secret, &salt, env.iterations)?;
        let plaintext = open_token_cache(&mut key, &nonce, &mut ciphertext)?;
        key.fill(0);
        serde_json::from_slice(plaintext).map_err(std::io::Error::other)
    }

    fn save_to_keyring_marker(
        &self,
        path: &Path,
        marker: &KeyringTokenCache,
    ) -> std::io::Result<()> {
        save_token_cache_to_keyring(marker, self)?;
        let bytes = serde_json::to_vec_pretty(marker).map_err(std::io::Error::other)?;
        write_token_cache(path, &bytes)
    }

    fn load_from_keyring_marker(marker: &KeyringTokenCache) -> std::io::Result<Self> {
        load_token_cache_from_keyring(marker)
    }
}

enum TokenCacheFileKind {
    Plain,
    Encrypted,
    Keyring(KeyringTokenCache),
}

fn token_cache_file_kind(bytes: &[u8]) -> std::io::Result<TokenCacheFileKind> {
    let v: serde_json::Value = serde_json::from_slice(bytes).map_err(std::io::Error::other)?;
    match v.get("isyncyou_token_cache").and_then(|m| m.as_str()) {
        Some(TOKEN_CACHE_MAGIC) => Ok(TokenCacheFileKind::Encrypted),
        Some(TOKEN_CACHE_KEYRING_MAGIC) => {
            let marker: KeyringTokenCache =
                serde_json::from_value(v).map_err(std::io::Error::other)?;
            validate_keyring_token_cache_marker(&marker)?;
            Ok(TokenCacheFileKind::Keyring(marker))
        }
        _ => Ok(TokenCacheFileKind::Plain),
    }
}

fn existing_keyring_marker(path: &Path) -> std::io::Result<Option<KeyringTokenCache>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let v: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    match v.get("isyncyou_token_cache").and_then(|m| m.as_str()) {
        Some(TOKEN_CACHE_KEYRING_MAGIC) => {
            let marker: KeyringTokenCache =
                serde_json::from_value(v).map_err(std::io::Error::other)?;
            validate_keyring_token_cache_marker(&marker)?;
            Ok(Some(marker))
        }
        _ => Ok(None),
    }
}

fn keyring_marker_for_path(path: &Path) -> KeyringTokenCache {
    KeyringTokenCache {
        isyncyou_token_cache: TOKEN_CACHE_KEYRING_MAGIC.into(),
        keyring_service: TOKEN_CACHE_KEYRING_SERVICE.into(),
        keyring_user: path.to_string_lossy().into_owned(),
    }
}

fn validate_keyring_token_cache_marker(marker: &KeyringTokenCache) -> std::io::Result<()> {
    if marker.isyncyou_token_cache != TOKEN_CACHE_KEYRING_MAGIC {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unsupported token cache keyring marker",
        ));
    }
    if marker.keyring_service.is_empty() || marker.keyring_user.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "token cache keyring marker is missing service or user",
        ));
    }
    Ok(())
}

#[cfg(feature = "desktop-keyring")]
fn ensure_desktop_keyring_store() -> std::io::Result<()> {
    if keyring_core::get_default_store().is_some() {
        return Ok(());
    }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        keyring_core::set_default_store(
            zbus_secret_service_keyring_store::Store::new().map_err(map_keyring_error)?,
        );
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "desktop keyring token cache is not supported on this target yet",
        ))
    }
}

#[cfg(feature = "desktop-keyring")]
fn load_token_cache_from_keyring(marker: &KeyringTokenCache) -> std::io::Result<TokenCache> {
    ensure_desktop_keyring_store()?;
    let entry = keyring_core::Entry::new(&marker.keyring_service, &marker.keyring_user)
        .map_err(map_keyring_error)?;
    let json = entry.get_password().map_err(map_keyring_error)?;
    serde_json::from_str(&json).map_err(std::io::Error::other)
}

#[cfg(not(feature = "desktop-keyring"))]
fn load_token_cache_from_keyring(_marker: &KeyringTokenCache) -> std::io::Result<TokenCache> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "desktop keyring token cache support is not compiled in",
    ))
}

#[cfg(feature = "desktop-keyring")]
fn save_token_cache_to_keyring(
    marker: &KeyringTokenCache,
    cache: &TokenCache,
) -> std::io::Result<()> {
    ensure_desktop_keyring_store()?;
    let entry = keyring_core::Entry::new(&marker.keyring_service, &marker.keyring_user)
        .map_err(map_keyring_error)?;
    let json = serde_json::to_string(cache).map_err(std::io::Error::other)?;
    entry.set_password(&json).map_err(map_keyring_error)
}

#[cfg(not(feature = "desktop-keyring"))]
fn save_token_cache_to_keyring(
    _marker: &KeyringTokenCache,
    _cache: &TokenCache,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "desktop keyring token cache support is not compiled in",
    ))
}

#[cfg(feature = "desktop-keyring")]
fn map_keyring_error(err: keyring_core::Error) -> std::io::Error {
    let kind = match &err {
        keyring_core::Error::NoEntry => std::io::ErrorKind::NotFound,
        keyring_core::Error::NoStorageAccess(_) => std::io::ErrorKind::PermissionDenied,
        keyring_core::Error::NoDefaultStore | keyring_core::Error::NotSupportedByStore(_) => {
            std::io::ErrorKind::Unsupported
        }
        keyring_core::Error::Invalid(_, _)
        | keyring_core::Error::BadEncoding(_)
        | keyring_core::Error::BadDataFormat(_, _)
        | keyring_core::Error::BadStoreFormat(_)
        | keyring_core::Error::TooLong(_, _)
        | keyring_core::Error::Ambiguous(_) => std::io::ErrorKind::InvalidData,
        _ => std::io::ErrorKind::Other,
    };
    std::io::Error::new(kind, err.to_string())
}

fn validate_encrypted_token_cache_header(env: &EncryptedTokenCache) -> std::io::Result<()> {
    if env.isyncyou_token_cache != TOKEN_CACHE_MAGIC {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unsupported token cache envelope",
        ));
    }
    if env.kdf != TOKEN_CACHE_KDF || env.aead != TOKEN_CACHE_AEAD {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unsupported token cache crypto parameters",
        ));
    }
    if env.iterations == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "token cache KDF iterations must be non-zero",
        ));
    }
    Ok(())
}

fn token_cache_secret() -> std::io::Result<Option<Vec<u8>>> {
    if let Some(path) = std::env::var_os(TOKEN_CACHE_SECRET_FILE_ENV) {
        return read_secret_file(Path::new(&path)).map(Some);
    }
    if let Some(dir) = std::env::var_os(SYSTEMD_CREDENTIALS_DIR_ENV) {
        let path = PathBuf::from(dir).join(TOKEN_CACHE_SYSTEMD_CREDENTIAL);
        if path.exists() {
            return read_secret_file(&path).map(Some);
        }
    }
    match std::env::var(TOKEN_CACHE_SECRET_ENV) {
        Ok(s) if !s.is_empty() => Ok(Some(s.into_bytes())),
        _ => Ok(None),
    }
}

fn read_secret_file(path: &Path) -> std::io::Result<Vec<u8>> {
    let bytes = std::fs::read(path)?;
    let trimmed = trim_ascii_whitespace(&bytes);
    if trimmed.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "token cache encryption secret file is empty",
        ));
    }
    Ok(trimmed.to_vec())
}

fn trim_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map(|idx| idx + 1)
        .unwrap_or(start);
    &bytes[start..end]
}

fn encrypted_cache_needs_secret_message() -> String {
    format!(
        "encrypted token cache needs {TOKEN_CACHE_SECRET_FILE_ENV}, systemd credential {TOKEN_CACHE_SYSTEMD_CREDENTIAL}, or {TOKEN_CACHE_SECRET_ENV}"
    )
}

/// Path of the auto-generated, owner-only local key kept next to the token cache.
///
/// It is used only when no explicit secret (env/file/systemd) and no keyring
/// backend are configured, so the on-disk cache is **encrypted-at-rest by default**
/// instead of plaintext.
///
/// Scope (documented honestly): a local key beside the cache protects the cache
/// file if it alone is copied, synced, backed up or logged. It does **not** protect
/// against an attacker with read access to the whole config directory (they obtain
/// the key too) — for that, use the OS keyring (`login --keyring`) or an
/// out-of-band `ISYNCYOU_TOKEN_CACHE_KEY*`, both of which take precedence here.
fn local_key_path(cache_path: &Path) -> PathBuf {
    let mut p = cache_path.as_os_str().to_os_string();
    p.push(".key");
    PathBuf::from(p)
}

/// Read the auto-generated local key if it exists; never creates it. Returns `None`
/// when the key file is absent or too short to be a valid key.
fn read_local_key(cache_path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match std::fs::read(local_key_path(cache_path)) {
        Ok(bytes) if bytes.len() >= TOKEN_CACHE_KEY_LEN => Ok(Some(bytes)),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Load the auto-generated local key, creating it (32 CSPRNG bytes, owner-only) if
/// absent. Written with the same owner-only helper as the cache itself.
fn load_or_create_local_key(cache_path: &Path) -> std::io::Result<Vec<u8>> {
    if let Some(key) = read_local_key(cache_path)? {
        return Ok(key);
    }
    let key = random_bytes(TOKEN_CACHE_KEY_LEN)?;
    write_token_cache(&local_key_path(cache_path), &key)?;
    Ok(key)
}

fn random_bytes(len: usize) -> std::io::Result<Vec<u8>> {
    let rng = rand::SystemRandom::new();
    let mut out = vec![0u8; len];
    rand::SecureRandom::fill(&rng, &mut out)
        .map_err(|_| std::io::Error::other("token cache RNG failed"))?;
    Ok(out)
}

fn derive_token_cache_key(
    secret: &[u8],
    salt: &[u8],
    iterations: u32,
) -> std::io::Result<[u8; TOKEN_CACHE_KEY_LEN]> {
    let iterations = NonZeroU32::new(iterations).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "token cache KDF iterations must be non-zero",
        )
    })?;
    let mut key = [0u8; TOKEN_CACHE_KEY_LEN];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        iterations,
        salt,
        secret,
        &mut key,
    );
    Ok(key)
}

fn seal_token_cache(
    key_bytes: &mut [u8; TOKEN_CACHE_KEY_LEN],
    nonce: &[u8],
    plaintext: &[u8],
) -> std::io::Result<Vec<u8>> {
    if nonce.len() != TOKEN_CACHE_NONCE_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "token cache nonce has invalid length",
        ));
    }
    let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, key_bytes)
        .map_err(|_| std::io::Error::other("token cache AEAD setup failed"))?;
    let key = aead::LessSafeKey::new(unbound);
    let nonce_bytes: [u8; TOKEN_CACHE_NONCE_LEN] = nonce.try_into().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "token cache nonce has invalid length",
        )
    })?;
    let mut in_out = plaintext.to_vec();
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(nonce_bytes),
        aead::Aad::from(TOKEN_CACHE_MAGIC.as_bytes()),
        &mut in_out,
    )
    .map_err(|_| std::io::Error::other("token cache encryption failed"))?;
    Ok(in_out)
}

fn open_token_cache<'a>(
    key_bytes: &mut [u8; TOKEN_CACHE_KEY_LEN],
    nonce: &[u8],
    ciphertext: &'a mut [u8],
) -> std::io::Result<&'a [u8]> {
    if nonce.len() != TOKEN_CACHE_NONCE_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "token cache nonce has invalid length",
        ));
    }
    let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, key_bytes)
        .map_err(|_| std::io::Error::other("token cache AEAD setup failed"))?;
    let key = aead::LessSafeKey::new(unbound);
    let nonce_bytes: [u8; TOKEN_CACHE_NONCE_LEN] = nonce.try_into().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "token cache nonce has invalid length",
        )
    })?;
    let plaintext = key
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce_bytes),
            aead::Aad::from(TOKEN_CACHE_MAGIC.as_bytes()),
            ciphertext,
        )
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "token cache decryption failed",
            )
        })?;
    Ok(plaintext)
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

fn hex_decode(s: &str) -> std::io::Result<Vec<u8>> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "hex value has odd length",
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        out.push((hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?);
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> std::io::Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "hex value contains a non-hex digit",
        )),
    }
}

#[cfg(unix)]
fn write_token_cache(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::fs::{OpenOptions, Permissions};
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    std::fs::set_permissions(path, Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_token_cache(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

/// Live OAuth network flow (device-code + refresh).
#[cfg(feature = "http")]
pub mod flow {
    use super::*;

    /// Device-code start response — show `message`/`user_code` to the user.
    #[derive(Debug, Clone, Deserialize)]
    pub struct DeviceCode {
        pub user_code: String,
        pub device_code: String,
        pub verification_uri: String,
        pub message: String,
        #[serde(default = "default_interval")]
        pub interval: u64,
        #[serde(default)]
        pub expires_in: u64,
    }
    fn default_interval() -> u64 {
        5
    }

    /// Outcome of one token poll.
    #[derive(Debug)]
    pub enum PollOutcome {
        Token(TokenResponse),
        Pending,
        SlowDown,
        Error(String),
    }

    fn client() -> reqwest::blocking::Client {
        reqwest::blocking::Client::new()
    }

    /// Begin the device-code flow; returns the code/URI to present to the user.
    pub fn start_device_code(client_id: &str, scopes: &[&str]) -> Result<DeviceCode, String> {
        let resp = client()
            .post(format!("{AUTHORITY}/devicecode"))
            .form(&[("client_id", client_id), ("scope", &scopes.join(" "))])
            .send()
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!(
                "devicecode HTTP {}: {}",
                resp.status().as_u16(),
                resp.text().unwrap_or_default()
            ));
        }
        resp.json::<DeviceCode>().map_err(|e| e.to_string())
    }

    /// Poll the token endpoint once for a pending device-code authorization.
    pub fn poll_token(client_id: &str, device_code: &str) -> PollOutcome {
        let resp = match client()
            .post(format!("{AUTHORITY}/token"))
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", client_id),
                ("device_code", device_code),
            ])
            .send()
        {
            Ok(r) => r,
            Err(e) => return PollOutcome::Error(e.to_string()),
        };
        let status = resp.status();
        let v: serde_json::Value = match resp.json() {
            Ok(v) => v,
            Err(e) => return PollOutcome::Error(e.to_string()),
        };
        if status.is_success() {
            match serde_json::from_value::<TokenResponse>(v) {
                Ok(t) => PollOutcome::Token(t),
                Err(e) => PollOutcome::Error(e.to_string()),
            }
        } else {
            match v.get("error").and_then(|e| e.as_str()) {
                Some("authorization_pending") => PollOutcome::Pending,
                Some("slow_down") => PollOutcome::SlowDown,
                other => PollOutcome::Error(other.unwrap_or("unknown").to_string()),
            }
        }
    }

    /// Renew an access token from a refresh token.
    pub fn refresh(
        client_id: &str,
        refresh_token: &str,
        scopes: &[&str],
    ) -> Result<TokenResponse, String> {
        let resp = client()
            .post(format!("{AUTHORITY}/token"))
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", client_id),
                ("refresh_token", refresh_token),
                ("scope", &scopes.join(" ")),
            ])
            .send()
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!(
                "refresh HTTP {}: {}",
                resp.status().as_u16(),
                resp.text().unwrap_or_default()
            ));
        }
        resp.json::<TokenResponse>().map_err(|e| e.to_string())
    }

    /// Return a valid access token from the cache at `cache_path`, refreshing it
    /// via the stored refresh token when expired and saving the renewed cache.
    /// Errors if there is no usable refresh token — the caller should then run
    /// [`device_code_login`]. This is the non-interactive path the daemon/CLI use
    /// on every run; only the initial login needs a human.
    pub fn ensure_access_token(
        cache_path: &Path,
        client_id: &str,
        scopes: &[&str],
        now_unix: u64,
    ) -> Result<String, String> {
        let mut cache = TokenCache::load(cache_path).map_err(|e| e.to_string())?;
        if cache.is_access_valid(now_unix) {
            return Ok(cache.access_token);
        }
        let rt = cache
            .refresh_token
            .clone()
            .ok_or("cached token expired and no refresh token; run the device-code login")?;
        let resp = refresh(client_id, &rt, scopes)?;
        cache = TokenCache::from_response(&resp, now_unix);
        // Graph does not always return a fresh refresh token; keep the old one.
        if cache.refresh_token.is_none() {
            cache.refresh_token = Some(rt);
        }
        cache.save(cache_path).map_err(|e| e.to_string())?;
        Ok(cache.access_token)
    }

    /// Run the device-code login to completion: show the code via `present`, poll
    /// until the user authorizes (or it times out), and return the token cache.
    /// Blocking; this is the one step that needs a human.
    pub fn device_code_login(
        client_id: &str,
        scopes: &[&str],
        now_unix: u64,
        present: impl Fn(&DeviceCode),
    ) -> Result<TokenCache, String> {
        let dc = start_device_code(client_id, scopes)?;
        present(&dc);
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(dc.expires_in.max(300));
        let mut interval = dc.interval.max(1);
        loop {
            if std::time::Instant::now() >= deadline {
                return Err("device-code login timed out".into());
            }
            std::thread::sleep(std::time::Duration::from_secs(interval));
            match poll_token(client_id, &dc.device_code) {
                PollOutcome::Token(t) => return Ok(TokenCache::from_response(&t, now_unix)),
                PollOutcome::Pending => {}
                PollOutcome::SlowDown => interval += 5,
                PollOutcome::Error(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "desktop-keyring")]
    static KEYRING_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn resp(access: &str, refresh: Option<&str>, ttl: u64) -> TokenResponse {
        TokenResponse {
            access_token: access.into(),
            refresh_token: refresh.map(String::from),
            expires_in: ttl,
        }
    }

    #[test]
    fn from_response_expires_a_minute_early() {
        let c = TokenCache::from_response(&resp("AT", Some("RT"), 3600), 1000);
        assert_eq!(c.access_token, "AT");
        assert_eq!(c.refresh_token.as_deref(), Some("RT"));
        assert_eq!(c.expires_at, 1000 + 3600 - 60);
    }

    #[test]
    fn validity_window() {
        let c = TokenCache::from_response(&resp("AT", None, 3600), 1000);
        assert!(c.is_access_valid(2000));
        assert!(!c.is_access_valid(1000 + 3600)); // past expiry
        assert!(!TokenCache::default().is_access_valid(0)); // empty token
    }

    #[test]
    fn short_ttl_does_not_underflow() {
        // expires_in < 60 must not panic/underflow
        let c = TokenCache::from_response(&resp("AT", None, 10), 1000);
        assert_eq!(c.expires_at, 1000); // saturating: 10-60 -> 0
        assert!(!c.is_access_valid(1000));
    }

    #[test]
    fn cache_save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("isyncyou-auth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("token.json");
        let c = TokenCache::from_response(&resp("AT", Some("RT"), 3600), 5000);
        c.save(&p).unwrap();
        let back = TokenCache::load(&p).unwrap();
        assert_eq!(back.access_token, c.access_token);
        assert_eq!(back.refresh_token, c.refresh_token);
        assert_eq!(back.expires_at, c.expires_at);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn token_cache_save_is_owner_only_on_unix() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let dir = std::env::temp_dir().join(format!("isyncyou-auth-mode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("token.json");
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o644)
            .open(&p)
            .unwrap();

        let c = TokenCache::from_response(&resp("AT", Some("RT"), 3600), 5000);
        c.save(&p).unwrap();

        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(TokenCache::load(&p).unwrap().access_token, "AT");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_without_secret_encrypts_with_auto_local_key() {
        let dir =
            std::env::temp_dir().join(format!("isyncyou-auth-autokey-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("token.json");
        let c = TokenCache::from_response(&resp("ACCESSSECRETXYZ", Some("RT"), 3600), 5000);
        c.save(&p).unwrap();

        // The cache is encrypted at rest (no plaintext token) by default...
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.contains(TOKEN_CACHE_MAGIC));
        assert!(!raw.contains("ACCESSSECRETXYZ"));
        // ...and an owner-only sibling key file was created.
        let key_path = local_key_path(&p);
        assert!(key_path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        // It round-trips via the same auto key.
        let back = TokenCache::load(&p).unwrap();
        assert_eq!(back.access_token, "ACCESSSECRETXYZ");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_plaintext_cache_still_loads() {
        // A pre-existing plaintext cache (written before secure-by-default) must
        // still load, for backward compatibility.
        let dir = std::env::temp_dir().join(format!("isyncyou-auth-legacy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("token.json");
        let c = TokenCache::from_response(&resp("LEGACYTOKEN", Some("RT"), 3600), 5000);
        std::fs::write(&p, serde_json::to_vec_pretty(&c).unwrap()).unwrap();
        let back = TokenCache::load(&p).unwrap();
        assert_eq!(back.access_token, "LEGACYTOKEN");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_plaintext_cache_migrates_to_encrypted_on_next_save() {
        // #328 AC-2: a pre-existing plaintext token cache is migrated to the
        // encrypted-at-rest format by the next save (load -> save roundtrip).
        let dir =
            std::env::temp_dir().join(format!("isyncyou-auth-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("token.json");
        let c = TokenCache::from_response(&resp("LEGACYPLAINTOKEN", Some("RT"), 3600), 5000);
        std::fs::write(&p, serde_json::to_vec_pretty(&c).unwrap()).unwrap();
        assert!(std::fs::read_to_string(&p)
            .unwrap()
            .contains("LEGACYPLAINTOKEN"));

        let loaded = TokenCache::load(&p).unwrap();
        loaded.save(&p).unwrap();

        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.contains(TOKEN_CACHE_MAGIC), "not migrated to encrypted");
        assert!(
            !raw.contains("LEGACYPLAINTOKEN"),
            "plaintext token survived the migration save"
        );
        // and it still round-trips via the auto local key
        assert_eq!(
            TokenCache::load(&p).unwrap().access_token,
            "LEGACYPLAINTOKEN"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_cache_encrypted_roundtrip_hides_plaintext() {
        let dir =
            std::env::temp_dir().join(format!("isyncyou-auth-encrypted-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("token.json");
        let c = TokenCache {
            access_token: "ACCESS-TOKEN-SECRET".into(),
            refresh_token: Some("REFRESH-TOKEN-SECRET".into()),
            expires_at: 1234,
        };

        c.save_encrypted_with_secret(&p, b"correct horse battery staple")
            .unwrap();

        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.contains(TOKEN_CACHE_MAGIC));
        assert!(!raw.contains("ACCESS-TOKEN-SECRET"), "raw cache: {raw}");
        assert!(!raw.contains("REFRESH-TOKEN-SECRET"), "raw cache: {raw}");
        let back =
            TokenCache::load_encrypted_with_secret(&p, b"correct horse battery staple").unwrap();
        assert_eq!(back.access_token, c.access_token);
        assert_eq!(back.refresh_token, c.refresh_token);
        assert_eq!(back.expires_at, c.expires_at);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_cache_encrypted_roundtrip_rejects_wrong_secret() {
        let dir =
            std::env::temp_dir().join(format!("isyncyou-auth-wrong-secret-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("token.json");
        let c = TokenCache::from_response(&resp("AT", Some("RT"), 3600), 5000);
        c.save_encrypted_with_secret(&p, b"right secret").unwrap();

        let err = TokenCache::load_encrypted_with_secret(&p, b"wrong secret").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("decryption failed"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_cache_secret_file_is_trimmed_and_nonempty() {
        let dir =
            std::env::temp_dir().join(format!("isyncyou-auth-secret-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(TOKEN_CACHE_SYSTEMD_CREDENTIAL);
        std::fs::write(&p, b"\n  secret from credential file \t\n").unwrap();
        assert_eq!(
            read_secret_file(&p).unwrap(),
            b"secret from credential file"
        );
        std::fs::write(&p, b"\n\t ").unwrap();
        let err = read_secret_file(&p).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "desktop-keyring")]
    fn with_mock_keyring(test: impl FnOnce()) {
        let _guard = KEYRING_TEST_LOCK.lock().unwrap();
        keyring_core::set_default_store(keyring_core::mock::Store::new().unwrap());
        test();
        let _ = keyring_core::unset_default_store();
    }

    #[cfg(feature = "desktop-keyring")]
    #[test]
    fn token_cache_keyring_roundtrip_writes_marker_only() {
        with_mock_keyring(|| {
            let dir =
                std::env::temp_dir().join(format!("isyncyou-auth-keyring-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join("token.json");
            let c = TokenCache {
                access_token: "ACCESS-TOKEN-IN-KEYRING".into(),
                refresh_token: Some("REFRESH-TOKEN-IN-KEYRING".into()),
                expires_at: 4242,
            };

            c.save_to_keyring(&p).unwrap();

            let raw = std::fs::read_to_string(&p).unwrap();
            assert!(raw.contains(TOKEN_CACHE_KEYRING_MAGIC));
            assert!(raw.contains(TOKEN_CACHE_KEYRING_SERVICE));
            assert!(!raw.contains("ACCESS-TOKEN-IN-KEYRING"), "raw cache: {raw}");
            assert!(
                !raw.contains("REFRESH-TOKEN-IN-KEYRING"),
                "raw cache: {raw}"
            );

            let back = TokenCache::load(&p).unwrap();
            assert_eq!(back.access_token, c.access_token);
            assert_eq!(back.refresh_token, c.refresh_token);
            assert_eq!(back.expires_at, c.expires_at);

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600);
            }
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[cfg(feature = "desktop-keyring")]
    #[test]
    fn token_cache_save_preserves_existing_keyring_backend() {
        with_mock_keyring(|| {
            let dir = std::env::temp_dir().join(format!(
                "isyncyou-auth-keyring-refresh-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join("token.json");

            TokenCache {
                access_token: "OLD".into(),
                refresh_token: Some("OLD-RT".into()),
                expires_at: 1,
            }
            .save_to_keyring(&p)
            .unwrap();

            let renewed = TokenCache {
                access_token: "NEW".into(),
                refresh_token: Some("NEW-RT".into()),
                expires_at: 999,
            };
            renewed.save(&p).unwrap();

            let raw = std::fs::read_to_string(&p).unwrap();
            assert!(raw.contains(TOKEN_CACHE_KEYRING_MAGIC));
            assert!(!raw.contains("NEW-RT"), "raw cache: {raw}");
            let back = TokenCache::load(&p).unwrap();
            assert_eq!(back.access_token, "NEW");
            assert_eq!(back.refresh_token.as_deref(), Some("NEW-RT"));
            assert_eq!(back.expires_at, 999);
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn deserializes_real_shaped_response() {
        let v =
            r#"{"access_token":"x","refresh_token":"y","expires_in":3599,"token_type":"Bearer"}"#;
        let t: TokenResponse = serde_json::from_str(v).unwrap();
        assert_eq!(t.access_token, "x");
        assert_eq!(t.expires_in, 3599);
    }

    /// A still-valid cached token is returned without any network call; an expired
    /// cache with no refresh token errors clearly (also no network).
    #[cfg(feature = "http")]
    #[test]
    fn ensure_returns_cached_token_when_valid_without_network() {
        let dir = std::env::temp_dir().join(format!("isyncyou-ensure-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("tok.json");

        TokenCache {
            access_token: "GOOD".into(),
            refresh_token: Some("RT".into()),
            expires_at: 10_000,
        }
        .save(&p)
        .unwrap();
        let tok = flow::ensure_access_token(&p, "cid", &["Files.Read"], 1_000).unwrap();
        assert_eq!(tok, "GOOD");

        TokenCache {
            access_token: String::new(),
            refresh_token: None,
            expires_at: 0,
        }
        .save(&p)
        .unwrap();
        let err = flow::ensure_access_token(&p, "cid", &["Files.Read"], 1_000).unwrap_err();
        assert!(err.contains("no refresh token"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Live: a real refresh token (from the cached test-account login) renews the
    /// access token non-interactively and the cache is persisted valid. Provide
    /// the RT via `ISYNCYOU_TEST_REFRESH_TOKEN` (extracted from the MSAL cache).
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_refresh_from_cached_refresh_token() {
        let rt = match std::env::var("ISYNCYOU_TEST_REFRESH_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!(
                    "skipping live_refresh_from_cached_refresh_token: ISYNCYOU_TEST_REFRESH_TOKEN not set"
                );
                return;
            }
        };
        let dir = std::env::temp_dir().join(format!("isyncyou-refresh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("tok.json");
        // empty (invalid) access + the real refresh token -> forces a live refresh
        TokenCache {
            access_token: String::new(),
            refresh_token: Some(rt),
            expires_at: 0,
        }
        .save(&p)
        .unwrap();
        let now = 1_000_000_000u64;
        // the read app (public client) used for the test account
        let client_id = "cee80dd9-c13e-4dbb-9d4c-73eb4987d447";
        match flow::ensure_access_token(&p, client_id, &["Files.Read"], now) {
            Ok(tok) => {
                assert!(!tok.is_empty(), "refreshed access token must be non-empty");
                let back = TokenCache::load(&p).unwrap();
                assert!(back.is_access_valid(now), "renewed cache should be valid");
                assert!(back.refresh_token.is_some(), "refresh token retained");
                eprintln!("live refresh: renewed access token of {} chars", tok.len());
            }
            // A well-formed refresh whose stored grant has aged out (AADSTS70000):
            // the request itself reached AAD correctly; renewing it needs a fresh
            // interactive login (#40) — the documented OAuth blocker, not a bug.
            Err(e) if e.contains("invalid_grant") || e.contains("AADSTS70000") => {
                eprintln!("live refresh skipped: cached grant expired, needs interactive login (#40): {e}");
            }
            // Any other failure (invalid_client/invalid_scope/malformed) is a real bug.
            Err(e) => panic!("refresh request was rejected as malformed/invalid: {e}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
