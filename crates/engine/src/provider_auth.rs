//! OAuth authorization-code flow for a configured HTTPS provider.
//! Refresh credentials are stored only in the desktop keyring.

use crate::provider_credential::{ProviderCredential, ProviderCredentialStore};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use fs2::FileExt;
use isyncyou_core::{config::ProviderConfig, Config};
use isyncyou_graph::auth::{TokenCache, TokenResponse};
use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::OpenOptions,
    io::Read,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const SCOPE: &str = "urn:ietf:params:oauth:scope:mail urn:ietf:params:oauth:scope:calendars urn:ietf:params:oauth:scope:contacts openid offline_access";
const MAX_RESPONSE: u64 = 65536;

/// Contains a verifier and state; keep in native memory, never expose or log it.
pub struct AuthAttempt {
    pub authorization_uri: String,
    origin: String,
    client_id: String,
    token_endpoint: String,
    redirect_uri: String,
    verifier: String,
    state: String,
    cache_path: PathBuf,
    account_id: String,
    started_at: u64,
}

fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|_| "OAuth transport unavailable".into())
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn random() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "OAuth random source unavailable")?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
fn json_response(response: reqwest::blocking::Response) -> Result<Value, String> {
    if !response.status().is_success() {
        return Err("OAuth server rejected request".into());
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_RESPONSE + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "OAuth response unavailable")?;
    if bytes.len() as u64 > MAX_RESPONSE {
        return Err("OAuth response too large".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "Invalid OAuth response".into())
}
fn same_origin(origin: &reqwest::Url, endpoint: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(endpoint).map_err(|_| "Invalid OAuth endpoint")?;
    if url.scheme() != "https"
        || url.origin() != origin.origin()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("Untrusted OAuth endpoint".into());
    }
    Ok(url)
}
fn account(cfg: &Config, id: &str) -> Result<(String, String, PathBuf), String> {
    let (origin, client_id) = match cfg.provider_accounts.get(id) {
        Some(ProviderConfig::SelfHosted { origin, client_id }) => {
            (origin.clone(), client_id.clone())
        }
        None => return Err("Not a self-hosted account".into()),
    };
    let acc = cfg
        .accounts
        .iter()
        .find(|a| a.id == id)
        .ok_or("Account missing")?;
    let mut h = Sha256::new();
    h.update(id.as_bytes());
    h.update([0]);
    h.update(origin.as_bytes());
    let path = acc
        .archive_root
        .join(format!(".provider-token-{:x}.json", h.finalize()));
    Ok((origin, client_id, path))
}
fn metadata(origin: &str) -> Result<(String, String, String), String> {
    let base = reqwest::Url::parse(origin).map_err(|_| "Invalid provider origin")?;
    if base.scheme() != "https"
        || base.host_str().is_none()
        || base.path() != "/"
        || base.query().is_some()
        || base.fragment().is_some()
        || !base.username().is_empty()
        || base.password().is_some()
    {
        return Err("Invalid provider origin".into());
    }
    let url = base
        .join("/.well-known/oauth-authorization-server")
        .map_err(|_| "Invalid provider origin")?;
    let metadata = json_response(
        client()?
            .get(url)
            .send()
            .map_err(|_| "OAuth discovery unavailable")?,
    )?;
    let issuer = metadata["issuer"].as_str().ok_or("Missing OAuth issuer")?;
    if issuer != origin {
        return Err("OAuth issuer mismatch".into());
    }
    if !metadata["code_challenge_methods_supported"]
        .as_array()
        .is_some_and(|a| a.iter().any(|v| v.as_str() == Some("S256")))
    {
        return Err("Provider does not support S256".into());
    }
    let authorization = same_origin(
        &base,
        metadata["authorization_endpoint"]
            .as_str()
            .ok_or("Missing authorization endpoint")?,
    )?;
    let token = same_origin(
        &base,
        metadata["token_endpoint"]
            .as_str()
            .ok_or("Missing token endpoint")?,
    )?;
    let revoke = same_origin(
        &base,
        metadata["revocation_endpoint"]
            .as_str()
            .ok_or("Missing revocation endpoint")?,
    )?;
    Ok((authorization.into(), token.into(), revoke.into()))
}
fn valid_redirect(uri: &str) -> bool {
    matches!(
        uri,
        "http://127.0.0.1:53682/oauth/callback"
            | "com.silentspike.isyncyou:/oauth/callback"
            | "com.silentspike.isyncyou.debug:/oauth/callback"
    )
}

pub fn start_self_hosted_auth(
    cfg: &Config,
    id: &str,
    redirect_uri: &str,
) -> Result<AuthAttempt, String> {
    if !valid_redirect(redirect_uri) {
        return Err("Invalid OAuth callback".into());
    }
    let (origin, client_id, cache_path) = account(cfg, id)?;
    let (authorization, token_endpoint, _) = metadata(&origin)?;
    let verifier = random()?;
    let state = random()?;
    let challenge = URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()));
    let mut url =
        reqwest::Url::parse(&authorization).map_err(|_| "Invalid authorization endpoint")?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state);
    Ok(AuthAttempt {
        authorization_uri: url.into(),
        origin,
        client_id,
        token_endpoint,
        redirect_uri: redirect_uri.into(),
        verifier,
        state,
        cache_path,
        account_id: id.into(),
        started_at: now(),
    })
}

/// Consumes an attempt, validates exact callback, state and issuer, then saves to
/// the OS keyring. A missing keyring is an explicit error, never a file fallback.
pub fn finish_self_hosted_auth(attempt: AuthAttempt, callback: &str) -> Result<(), String> {
    #[cfg(target_os = "android")]
    return finish_self_hosted_auth_with_store(
        attempt,
        callback,
        crate::provider_credential::platform_store()?,
    );
    #[cfg(not(target_os = "android"))]
    {
        let store = DesktopCredentialStore {
            path: attempt.cache_path.clone(),
            account: attempt.account_id.clone(),
            origin: attempt.origin.clone(),
        };
        finish_self_hosted_auth_with_store(attempt, callback, &store)
    }
}

pub fn finish_self_hosted_auth_with_store(
    attempt: AuthAttempt,
    callback: &str,
    store: &dyn ProviderCredentialStore,
) -> Result<(), String> {
    let code = callback_code(&attempt, callback)?.ok_or("Authorization rejected")?;
    let response = client()?
        .post(&attempt.token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", &attempt.client_id),
            ("code", &code),
            ("redirect_uri", &attempt.redirect_uri),
            ("code_verifier", &attempt.verifier),
        ])
        .send()
        .map_err(|_| "OAuth exchange unavailable")?;
    let token: TokenResponse = serde_json::from_value(json_response(response)?)
        .map_err(|_| "Invalid OAuth token response")?;
    if token.access_token.is_empty() || token.refresh_token.as_deref().is_none_or(str::is_empty) {
        return Err("Incomplete OAuth token response".into());
    }
    let _lock = lock(&attempt.cache_path)?;
    let cache = TokenCache::from_response(&token, now());
    store.replace(
        &attempt.account_id,
        &attempt.origin,
        &ProviderCredential {
            access_token: cache.access_token,
            refresh_token: cache.refresh_token,
            expires_at: cache.expires_at,
        },
    )
}

/// Reject unrelated loopback requests without consuming the pending attempt.
pub fn validate_callback(attempt: &AuthAttempt, callback: &str) -> Result<(), String> {
    callback_code(attempt, callback).map(|_| ())
}

fn callback_code(attempt: &AuthAttempt, callback: &str) -> Result<Option<String>, String> {
    if now().saturating_sub(attempt.started_at) > 600 {
        return Err("OAuth attempt expired".into());
    }
    let url = reqwest::Url::parse(callback).map_err(|_| "Invalid OAuth callback")?;
    let mut base = url.clone();
    base.set_query(None);
    base.set_fragment(None);
    if base.as_str() != attempt.redirect_uri || url.fragment().is_some() {
        return Err("Invalid OAuth callback".into());
    }
    let mut state = None;
    let mut issuer = None;
    let mut code = None;
    let mut error = None;
    for (key, value) in url.query_pairs() {
        let slot = match key.as_ref() {
            "state" => &mut state,
            "iss" => &mut issuer,
            "code" => &mut code,
            "error" => &mut error,
            _ => continue,
        };
        if slot.replace(value.into_owned()).is_some() {
            return Err("Duplicate OAuth callback parameter".into());
        }
    }
    if state.as_deref() != Some(attempt.state.as_str())
        || issuer.as_deref() != Some(attempt.origin.as_str())
    {
        return Err("OAuth callback verification failed".into());
    }
    match (
        code.filter(|c| !c.is_empty()),
        error.filter(|e| !e.is_empty()),
    ) {
        (Some(code), None) => Ok(Some(code)),
        (None, Some(_)) => Ok(None),
        _ => Err("Invalid OAuth callback result".into()),
    }
}

fn lock(path: &std::path::Path) -> Result<std::fs::File, String> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))
        .map_err(|_| "Credential lock unavailable")?;
    lock.lock_exclusive()
        .map_err(|_| "Credential lock unavailable")?;
    Ok(lock)
}

pub fn self_hosted_access_token(cfg: &Config, id: &str) -> Result<String, String> {
    #[cfg(target_os = "android")]
    return self_hosted_access_token_with_store(
        cfg,
        id,
        crate::provider_credential::platform_store()?,
    );
    #[cfg(not(target_os = "android"))]
    {
        let (origin, _, path) = account(cfg, id)?;
        let store = DesktopCredentialStore {
            path,
            account: id.into(),
            origin,
        };
        self_hosted_access_token_with_store(cfg, id, &store)
    }
}

pub fn self_hosted_access_token_with_store(
    cfg: &Config,
    id: &str,
    store: &dyn ProviderCredentialStore,
) -> Result<String, String> {
    let (origin, client_id, path) = account(cfg, id)?;
    let _lock = lock(&path)?;
    let mut cached = store.load(id, &origin)?.ok_or("Provider login required")?;
    if !cached.access_token.is_empty() && now() < cached.expires_at {
        return Ok(cached.access_token);
    }
    let refresh = cached
        .refresh_token
        .as_deref()
        .ok_or("Provider login required")?;
    let (_, endpoint, _) = metadata(&origin)?;
    let response = client()?
        .post(endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", &client_id),
            ("refresh_token", refresh),
        ])
        .send()
        .map_err(|_| "OAuth refresh unavailable")?;
    let token: TokenResponse = serde_json::from_value(json_response(response)?)
        .map_err(|_| "Invalid OAuth refresh response")?;
    if token.access_token.is_empty() {
        return Err("Invalid OAuth refresh response".into());
    }
    let previous = cached.refresh_token.take();
    let cache = TokenCache::from_response(&token, now());
    cached = ProviderCredential {
        access_token: cache.access_token,
        refresh_token: cache.refresh_token.or(previous),
        expires_at: cache.expires_at,
    };
    store.replace(id, &origin, &cached)?;
    Ok(cached.access_token)
}

#[cfg(not(target_os = "android"))]
fn load_keyring(path: &std::path::Path) -> Result<TokenCache, String> {
    let marker: Value =
        serde_json::from_slice(&std::fs::read(path).map_err(|_| "Provider login required")?)
            .map_err(|_| "Invalid credential marker")?;
    if marker["isyncyou_token_cache"] != "isyncyou-token-cache-keyring-v1"
        || marker["keyring_service"] != "org.silentspike.isyncyou.token-cache"
        || marker["keyring_user"].as_str() != Some(path.to_string_lossy().as_ref())
    {
        return Err("System keyring required".into());
    }
    TokenCache::load(path).map_err(|_| "System keyring unavailable".into())
}

#[cfg(not(target_os = "android"))]
struct DesktopCredentialStore {
    path: PathBuf,
    account: String,
    origin: String,
}
#[cfg(not(target_os = "android"))]
impl ProviderCredentialStore for DesktopCredentialStore {
    fn load(&self, account: &str, origin: &str) -> Result<Option<ProviderCredential>, String> {
        if account != self.account || origin != self.origin {
            return Err("Credential account mismatch".into());
        }
        if !self.path.exists() {
            return Ok(None);
        }
        let token = load_keyring(&self.path)?;
        Ok(Some(ProviderCredential {
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            expires_at: token.expires_at,
        }))
    }
    fn replace(
        &self,
        account: &str,
        origin: &str,
        credential: &ProviderCredential,
    ) -> Result<(), String> {
        if account != self.account || origin != self.origin {
            return Err("Credential account mismatch".into());
        }
        TokenCache {
            access_token: credential.access_token.clone(),
            refresh_token: credential.refresh_token.clone(),
            expires_at: credential.expires_at,
            account_subject: None,
        }
        .save_to_keyring(&self.path)
        .map_err(|_| "System keyring unavailable".into())
    }
    fn delete(&self, account: &str, origin: &str) -> Result<(), String> {
        if account != self.account || origin != self.origin {
            return Err("Credential account mismatch".into());
        }
        delete_desktop_credential(&self.path)
    }
}

#[cfg(all(feature = "desktop-keyring", not(target_os = "android")))]
fn delete_desktop_credential(path: &std::path::Path) -> Result<(), String> {
    let entry = keyring_core::Entry::new(
        "org.silentspike.isyncyou.token-cache",
        &path.to_string_lossy(),
    )
    .map_err(|_| "System keyring unavailable")?;
    entry
        .delete_credential()
        .map_err(|_| "Grant revoked but local keyring cleanup failed")?;
    std::fs::remove_file(path).map_err(|_| "Grant revoked but local marker cleanup failed")?;
    Ok(())
}
#[cfg(all(not(feature = "desktop-keyring"), not(target_os = "android")))]
fn delete_desktop_credential(_path: &std::path::Path) -> Result<(), String> {
    Err("System keyring unavailable".into())
}

/// Reports local grant availability only; does not imply server-side validity.
pub fn self_hosted_auth_status(cfg: &Config, id: &str) -> Result<bool, String> {
    #[cfg(target_os = "android")]
    return self_hosted_auth_status_with_store(
        cfg,
        id,
        crate::provider_credential::platform_store()?,
    );
    #[cfg(not(target_os = "android"))]
    {
        let (origin, _, path) = account(cfg, id)?;
        let store = DesktopCredentialStore {
            path,
            account: id.into(),
            origin,
        };
        self_hosted_auth_status_with_store(cfg, id, &store)
    }
}

pub fn self_hosted_auth_status_with_store(
    cfg: &Config,
    id: &str,
    store: &dyn ProviderCredentialStore,
) -> Result<bool, String> {
    let (origin, _, path) = account(cfg, id)?;
    let _lock = lock(&path)?;
    let Some(token) = store.load(id, &origin)? else {
        return Ok(false);
    };
    Ok(token
        .refresh_token
        .as_deref()
        .is_some_and(|value| !value.is_empty()))
}

/// Revoke the installation grant first, then remove its local keyring entry.
/// An unknown network result keeps the credential for explicit recovery.
pub fn logout_self_hosted(cfg: &Config, id: &str) -> Result<(), String> {
    #[cfg(target_os = "android")]
    return logout_self_hosted_with_store(cfg, id, crate::provider_credential::platform_store()?);
    #[cfg(not(target_os = "android"))]
    {
        let (origin, _, path) = account(cfg, id)?;
        let store = DesktopCredentialStore {
            path,
            account: id.into(),
            origin,
        };
        logout_self_hosted_with_store(cfg, id, &store)
    }
}

pub fn logout_self_hosted_with_store(
    cfg: &Config,
    id: &str,
    store: &dyn ProviderCredentialStore,
) -> Result<(), String> {
    let (origin, client_id, path) = account(cfg, id)?;
    let _lock = lock(&path)?;
    let cache = store.load(id, &origin)?.ok_or("Provider login required")?;
    let refresh = cache
        .refresh_token
        .as_deref()
        .ok_or("Provider login required")?;
    let (_, _, endpoint) = metadata(&origin)?;
    let response = client()?
        .post(endpoint)
        .form(&[
            ("token", refresh),
            ("token_type_hint", "refresh_token"),
            ("client_id", &client_id),
        ])
        .send()
        .map_err(|_| "Revocation outcome unknown")?;
    if !response.status().is_success() {
        return Err("Provider rejected revocation".into());
    }
    store.delete(id, &origin)
}

/// Obtain a distinct provider database key from the platform secret owner.
/// An existing database with a missing key is never recreated or overwritten.
#[cfg(feature = "encrypted-store")]
pub(crate) fn provider_database_key(
    cfg: &Config,
    id: &str,
    database_exists: bool,
) -> Result<Vec<u8>, String> {
    #[cfg(target_os = "android")]
    return provider_database_key_with_store(
        cfg,
        id,
        database_exists,
        crate::provider_credential::platform_store()?,
    );
    #[cfg(not(target_os = "android"))]
    {
        let (origin, _, token_path) = account(cfg, id)?;
        if id.starts_with("isyncyou-internal:") {
            return Err("Reserved account id".into());
        }
        let key_path = token_path.with_extension("store-key.json");
        let scoped_id = format!(
            "isyncyou-internal:store:{:x}",
            Sha256::digest(id.as_bytes())
        );
        let desktop = DesktopCredentialStore {
            path: key_path.clone(),
            account: scoped_id.clone(),
            origin: origin.clone(),
        };
        let store: &dyn ProviderCredentialStore = &desktop;
        provider_database_key_with_store(cfg, id, database_exists, store)
    }
}

#[cfg(feature = "encrypted-store")]
fn provider_database_key_with_store(
    cfg: &Config,
    id: &str,
    database_exists: bool,
    store: &dyn ProviderCredentialStore,
) -> Result<Vec<u8>, String> {
    let (origin, _, token_path) = account(cfg, id)?;
    if id.starts_with("isyncyou-internal:") {
        return Err("Reserved account id".into());
    }
    let key_path = token_path.with_extension("store-key.json");
    let scoped_id = format!(
        "isyncyou-internal:store:{:x}",
        Sha256::digest(id.as_bytes())
    );
    let _lock = lock(&key_path)?;
    let encoded = match store.load(&scoped_id, &origin)? {
        Some(secret) => secret.access_token,
        None if database_exists => {
            return Err("Provider database key missing; existing data preserved".into())
        }
        None => {
            let encoded = random()?;
            store.replace(
                &scoped_id,
                &origin,
                &ProviderCredential {
                    access_token: encoded.clone(),
                    refresh_token: None,
                    expires_at: 0,
                },
            )?;
            encoded
        }
    };
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| "Invalid provider database key")?;
    if bytes.len() != 32 {
        return Err("Invalid provider database key".into());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeStore(std::sync::Mutex<Option<ProviderCredential>>);
    impl ProviderCredentialStore for FakeStore {
        fn load(&self, account: &str, origin: &str) -> Result<Option<ProviderCredential>, String> {
            if account != "a" || origin != "https://example.test" {
                return Err("wrong scope".into());
            }
            Ok(self.0.lock().unwrap().clone())
        }
        fn replace(
            &self,
            account: &str,
            origin: &str,
            token: &ProviderCredential,
        ) -> Result<(), String> {
            if account != "a" || origin != "https://example.test" {
                return Err("wrong scope".into());
            }
            *self.0.lock().unwrap() = Some(token.clone());
            Ok(())
        }
        fn delete(&self, account: &str, origin: &str) -> Result<(), String> {
            if account != "a" || origin != "https://example.test" {
                return Err("wrong scope".into());
            }
            *self.0.lock().unwrap() = None;
            Ok(())
        }
    }

    #[test]
    fn injected_store_is_used_for_scoped_cached_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.accounts.push(isyncyou_core::AccountConfig {
            id: "a".into(),
            username: "user".into(),
            sync_root: PathBuf::new(),
            archive_root: dir.path().to_path_buf(),
            cache_root: PathBuf::new(),
            mount_point: None,
        });
        cfg.provider_accounts.insert(
            "a".into(),
            ProviderConfig::SelfHosted {
                origin: "https://example.test".into(),
                client_id: "client".into(),
            },
        );
        let store = FakeStore(std::sync::Mutex::new(Some(ProviderCredential {
            access_token: "secret".into(),
            refresh_token: Some("refresh".into()),
            expires_at: now() + 600,
        })));
        assert!(self_hosted_auth_status_with_store(&cfg, "a", &store).unwrap());
        assert_eq!(
            self_hosted_access_token_with_store(&cfg, "a", &store).unwrap(),
            "secret"
        );
        cfg.provider_accounts.insert(
            "a".into(),
            ProviderConfig::SelfHosted {
                origin: "https://other.test".into(),
                client_id: "client".into(),
            },
        );
        assert!(self_hosted_access_token_with_store(&cfg, "a", &store).is_err());
    }

    #[cfg(feature = "encrypted-store")]
    #[test]
    fn provider_database_key_is_stable_and_missing_key_never_replaces_existing_database() {
        struct Keys(
            std::sync::Mutex<std::collections::HashMap<(String, String), ProviderCredential>>,
        );
        impl ProviderCredentialStore for Keys {
            fn load(&self, a: &str, o: &str) -> Result<Option<ProviderCredential>, String> {
                Ok(self.0.lock().unwrap().get(&(a.into(), o.into())).cloned())
            }
            fn replace(&self, a: &str, o: &str, c: &ProviderCredential) -> Result<(), String> {
                self.0
                    .lock()
                    .unwrap()
                    .insert((a.into(), o.into()), c.clone());
                Ok(())
            }
            fn delete(&self, a: &str, o: &str) -> Result<(), String> {
                self.0.lock().unwrap().remove(&(a.into(), o.into()));
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.accounts.push(isyncyou_core::AccountConfig {
            id: "a".into(),
            username: "user".into(),
            sync_root: PathBuf::new(),
            archive_root: dir.path().to_path_buf(),
            cache_root: PathBuf::new(),
            mount_point: None,
        });
        cfg.provider_accounts.insert(
            "a".into(),
            ProviderConfig::SelfHosted {
                origin: "https://example.test".into(),
                client_id: "client".into(),
            },
        );
        let store = Keys(std::sync::Mutex::new(std::collections::HashMap::new()));
        assert!(provider_database_key_with_store(&cfg, "a", true, &store).is_err());
        let first = provider_database_key_with_store(&cfg, "a", false, &store).unwrap();
        assert_eq!(first.len(), 32);
        assert_eq!(
            provider_database_key_with_store(&cfg, "a", true, &store).unwrap(),
            first
        );
        cfg.provider_accounts.insert(
            "a".into(),
            ProviderConfig::SelfHosted {
                origin: "https://other.test".into(),
                client_id: "client".into(),
            },
        );
        assert!(provider_database_key_with_store(&cfg, "a", true, &store).is_err());
    }

    #[test]
    fn callback_requires_exact_redirect_state_and_issuer_without_consuming_attempt() {
        let attempt = AuthAttempt {
            authorization_uri: String::new(),
            origin: "https://example.test".into(),
            client_id: "client".into(),
            token_endpoint: "https://example.test/token".into(),
            redirect_uri: "http://127.0.0.1:53682/oauth/callback".into(),
            verifier: "secret".into(),
            state: "expected".into(),
            cache_path: PathBuf::new(),
            account_id: "a".into(),
            started_at: now(),
        };
        assert!(validate_callback(&attempt, "http://127.0.0.1:53682/oauth/callback?code=a&state=wrong&iss=https%3A%2F%2Fexample.test").is_err());
        assert!(validate_callback(&attempt, "http://127.0.0.1:53682/oauth/callback?code=a&state=expected&iss=https%3A%2F%2Fevil.test").is_err());
        assert!(validate_callback(&attempt, "http://127.0.0.1:53682/oauth/callback?code=a&state=expected&iss=https%3A%2F%2Fexample.test").is_ok());
        assert!(validate_callback(&attempt, "http://127.0.0.1:53682/oauth/callback?error=access_denied&state=expected&iss=https%3A%2F%2Fexample.test").is_ok());
        assert!(validate_callback(&attempt, "http://127.0.0.1:53682/oauth/callback?error=access_denied&state=wrong&iss=https%3A%2F%2Fexample.test").is_err());
    }
}
