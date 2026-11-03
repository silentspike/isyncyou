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
    revocation_endpoint: String,
    redirect_uri: String,
    verifier: String,
    state: String,
    cache_path: PathBuf,
    account_id: String,
    expected_username: String,
    account_config: Config,
    started_at: u64,
}

// Secret material: this representation is only for an authenticated,
// encrypted platform store. It must never be exposed to a WebView or logged.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AndroidAuthAttempt {
    version: u8,
    origin: String,
    client_id: String,
    token_endpoint: String,
    revocation_endpoint: String,
    redirect_uri: String,
    verifier: String,
    state: String,
    account_id: String,
    expected_username: String,
    started_at: u64,
}

/// Export only to an authenticated, encrypted Android-owned pending-attempt store.
pub fn export_android_auth_attempt(attempt: &AuthAttempt) -> Result<Vec<u8>, String> {
    let current = now();
    if !android_redirect(&attempt.redirect_uri) || current < attempt.started_at || current - attempt.started_at > 600 {
        return Err("Invalid or expired Android OAuth attempt".into());
    }
    serde_json::to_vec(&AndroidAuthAttempt {
        version: 1, origin: attempt.origin.clone(), client_id: attempt.client_id.clone(),
        token_endpoint: attempt.token_endpoint.clone(), revocation_endpoint: attempt.revocation_endpoint.clone(),
        redirect_uri: attempt.redirect_uri.clone(), verifier: attempt.verifier.clone(), state: attempt.state.clone(),
        account_id: attempt.account_id.clone(), expected_username: attempt.expected_username.clone(), started_at: attempt.started_at,
    }).map_err(|_| "Invalid Android OAuth attempt".into())
}

fn android_redirect(uri: &str) -> bool {
    matches!(uri, "com.silentspike.isyncyou:/oauth/callback" | "com.silentspike.isyncyou.debug:/oauth/callback")
}

/// Restore a pending attempt after the platform has authenticated its encrypted
/// record. The platform must atomically claim it before any token exchange and
/// must never replay an attempt whose exchange has started.
pub fn import_android_auth_attempt(cfg: &Config, id: &str, redirect_uri: &str, bytes: &[u8]) -> Result<AuthAttempt, String> {
    if bytes.len() > 16 * 1024 || !android_redirect(redirect_uri) { return Err("Invalid Android OAuth attempt".into()); }
    let saved: AndroidAuthAttempt = serde_json::from_slice(bytes).map_err(|_| "Invalid Android OAuth attempt")?;
    let (origin, client_id, cache_path) = account(cfg, id)?;
    let username = &cfg.accounts.iter().find(|a| a.id == id).ok_or("Account missing")?.username;
    let valid_secret = |s: &str| s.len() == 43 && URL_SAFE_NO_PAD.decode(s).is_ok_and(|v| v.len() == 32);
    let current = now();
    if saved.version != 1 || saved.account_id != id || saved.origin != origin || saved.client_id != client_id
        || saved.expected_username != *username || saved.redirect_uri != redirect_uri
        || saved.started_at > current || current - saved.started_at > 600
        || !valid_secret(&saved.verifier) || !valid_secret(&saved.state) {
        return Err("Invalid or expired Android OAuth attempt".into());
    }
    let base = reqwest::Url::parse(&origin).map_err(|_| "Invalid provider origin")?;
    same_origin(&base, &saved.token_endpoint)?;
    same_origin(&base, &saved.revocation_endpoint)?;
    Ok(AuthAttempt { authorization_uri: String::new(), origin, client_id, cache_path,
        token_endpoint: saved.token_endpoint, revocation_endpoint: saved.revocation_endpoint,
        redirect_uri: saved.redirect_uri, verifier: saved.verifier, state: saved.state,
        account_id: id.into(), expected_username: username.clone(), account_config: cfg.clone(), started_at: saved.started_at })
}

/// Typed completion state. No variant contains credentials or provider payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthFinishError {
    Failed(String),
    RevocationPending,
    RevocationUnrecoverable,
}
impl std::fmt::Display for AuthFinishError {
    fn fmt(&self, f:&mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self { Self::Failed(message)=>f.write_str(message), Self::RevocationPending=>f.write_str("Grant revocation pending"), Self::RevocationUnrecoverable=>f.write_str("Grant revocation unconfirmed and local recovery unavailable") }
    }
}
impl From<String> for AuthFinishError { fn from(value:String)->Self { Self::Failed(value) } }
impl From<&str> for AuthFinishError { fn from(value:&str)->Self { Self::Failed(value.into()) } }

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
    if !quarantined_grants(cfg,id)?.is_empty() { return Err("Previous grant revocation requires recovery before signing in again".into()); }
    let (origin, client_id, cache_path) = account(cfg, id)?;
    let expected_username = cfg.accounts.iter().find(|account| account.id == id).ok_or("Account missing")?.username.clone();
    let (authorization, token_endpoint, revocation_endpoint) = metadata(&origin)?;
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
        revocation_endpoint,
        redirect_uri: redirect_uri.into(),
        verifier,
        state,
        cache_path,
        account_id: id.into(),
        expected_username,
        account_config: cfg.clone(),
        started_at: now(),
    })
}

/// Consumes an attempt, validates exact callback, state and issuer, then saves to
/// the OS keyring. A missing keyring is an explicit error, never a file fallback.
pub fn finish_self_hosted_auth(attempt: AuthAttempt, callback: &str) -> Result<(), String> {
    finish_self_hosted_auth_detailed(attempt,callback).map_err(|error| error.to_string())
}

pub fn finish_self_hosted_auth_detailed(attempt: AuthAttempt, callback: &str) -> Result<(), AuthFinishError> {
    #[cfg(target_os = "android")]
    return finish_self_hosted_auth_detailed_with_store(
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
        finish_self_hosted_auth_detailed_with_store(attempt, callback, &store)
    }
}

pub fn finish_self_hosted_auth_with_store(
    attempt: AuthAttempt,
    callback: &str,
    store: &dyn ProviderCredentialStore,
) -> Result<(), String> {
    finish_self_hosted_auth_detailed_with_store(attempt,callback,store).map_err(|error| error.to_string())
}

pub fn finish_self_hosted_auth_detailed_with_store(
    attempt: AuthAttempt,
    callback: &str,
    store: &dyn ProviderCredentialStore,
) -> Result<(), AuthFinishError> {
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
        return Err(AuthFinishError::Failed("Incomplete OAuth token response".into()));
    }
    let subject = verify_userinfo(&attempt.origin,&token.access_token,&attempt.expected_username);
    let _lock = lock(&attempt.cache_path)?;
    let subject = match subject.and_then(|subject| { bind_subject(&attempt,store,&subject)?; Ok(subject) }) {
        Ok(subject) => subject,
        Err(error) => {
            return match revoke_unaccepted_grant(&attempt,store,&token) {
                Ok(()) => Err(AuthFinishError::Failed(error)),
                Err(recovery) => Err(recovery),
            };
        }
    };
    let cache = TokenCache::from_response(&token, now());
    let saved = store.replace(
        &attempt.account_id,
        &attempt.origin,
        &ProviderCredential {
            access_token: cache.access_token,
            refresh_token: cache.refresh_token,
            expires_at: cache.expires_at,
            subject: Some(subject),
        },
    );
    match saved {
        Ok(()) => {
            let marker = revocation_pending_key(&attempt.account_id);
            if store.load(&marker, &attempt.origin)?.is_some() { store.delete(&marker, &attempt.origin)?; }
            Ok(())
        },
        Err(error) => match revoke_unaccepted_grant(&attempt,store,&token) {
            Ok(()) => Err(AuthFinishError::Failed(error)),
            Err(recovery) => Err(recovery),
        }
    }
}

fn quarantine_key(account:&str) -> String { format!("isyncyou-internal:quarantine-index:{:x}",Sha256::digest(account.as_bytes())) }
fn revocation_pending_key(account:&str) -> String { format!("isyncyou-internal:revocation-pending:{:x}",Sha256::digest(account.as_bytes())) }

fn load_quarantine(store:&dyn ProviderCredentialStore,account:&str,origin:&str) -> Result<std::collections::BTreeMap<String,String>,String> {
    let Some(record) = store.load(&quarantine_key(account),origin)? else { return Ok(Default::default()); };
    if record.access_token.len()>65536 { return Err("Invalid grant recovery state".into()); }
    let map:std::collections::BTreeMap<String,String> = serde_json::from_str(&record.access_token).map_err(|_| "Invalid grant recovery state")?;
    if map.len()>32 || map.iter().any(|(id,token)| !valid_recovery_id(id) || token.is_empty()) { return Err("Invalid grant recovery state".into()); }
    Ok(map)
}
fn save_quarantine(store:&dyn ProviderCredentialStore,account:&str,origin:&str,map:&std::collections::BTreeMap<String,String>) -> Result<(),String> {
    let encoded=serde_json::to_string(map).map_err(|_| "Invalid grant recovery state")?;
    if encoded.len()>65536 || map.len()>32 { return Err("Grant recovery storage is full".into()); }
    store.replace(&quarantine_key(account),origin,&ProviderCredential {access_token:encoded,refresh_token:None,expires_at:0,subject:None})
}
fn valid_recovery_id(id:&str) -> bool { id.len()==64 && id.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) }

fn revoke_unaccepted_grant(attempt:&AuthAttempt, store:&dyn ProviderCredentialStore, token:&TokenResponse) -> Result<(),AuthFinishError> {
    let refresh = token.refresh_token.as_deref().filter(|value| !value.is_empty()).ok_or(AuthFinishError::RevocationUnrecoverable)?;
    let id = format!("{:x}",Sha256::digest(attempt.state.as_bytes()));
    let saved = (|| -> Result<(),String> {
        let mut map=load_quarantine(store,&attempt.account_id,&attempt.origin)?;
        if map.contains_key(&id) { return Err("Duplicate grant recovery identity".into()); }
        map.insert(id.clone(),refresh.into());
        save_quarantine(store,&attempt.account_id,&attempt.origin,&map)
    })().is_ok();
    let revoked = client().and_then(|http| http.post(&attempt.revocation_endpoint).form(&[("token",refresh),("token_type_hint","refresh_token"),("client_id",&attempt.client_id)]).send().map_err(|_| "Grant revocation unconfirmed".to_string())).is_ok_and(|response| response.status().is_success());
    if !revoked { return Err(if saved { AuthFinishError::RevocationPending } else { AuthFinishError::RevocationUnrecoverable }); }
    if saved {
        let mut map=load_quarantine(store,&attempt.account_id,&attempt.origin).map_err(|_| AuthFinishError::RevocationPending)?;
        map.remove(&id);
        save_quarantine(store,&attempt.account_id,&attempt.origin,&map).map_err(|_| AuthFinishError::RevocationPending)?;
    }
    Ok(())
}

/// Retry revocation of an explicitly identified quarantined grant; never returns tokens.
pub fn revoke_quarantined_grant_with_store(cfg:&Config, account_id:&str, recovery_id:&str, store:&dyn ProviderCredentialStore) -> Result<(),String> {
    let (origin,client_id,path) = account(cfg,account_id)?;
    if !valid_recovery_id(recovery_id) { return Err("Invalid recovery identity".into()); }
    let _lock = lock(&path)?;
    let mut map=load_quarantine(store,account_id,&origin)?;
    let token = map.get(recovery_id).ok_or("Quarantined grant not found")?;
    let (_,_,endpoint) = metadata(&origin)?;
    let response = client()?.post(endpoint).form(&[("token",token.as_str()),("token_type_hint","refresh_token"),("client_id",client_id.as_str())]).send().map_err(|_| "Grant revocation unconfirmed")?;
    if !response.status().is_success() { return Err("Grant revocation unconfirmed".into()); }
    map.remove(recovery_id);
    save_quarantine(store,account_id,&origin,&map)
}

pub fn revoke_quarantined_grant(cfg:&Config, account_id:&str, recovery_id:&str) -> Result<(),String> {
    #[cfg(target_os = "android")]
    return revoke_quarantined_grant_with_store(cfg,account_id,recovery_id,crate::provider_credential::platform_store()?);
    #[cfg(not(target_os = "android"))]
    {
        let (origin,_,path) = account(cfg,account_id)?;
        let store = DesktopCredentialStore { path, account:account_id.into(), origin };
        revoke_quarantined_grant_with_store(cfg,account_id,recovery_id,&store)
    }
}

/// List only opaque recovery IDs; never return quarantined credentials.
pub fn quarantined_grants_with_store(cfg:&Config,account_id:&str,store:&dyn ProviderCredentialStore)->Result<Vec<String>,String> {
    let (origin,_,path)=account(cfg,account_id)?;
    let _lock=lock(&path)?;
    Ok(load_quarantine(store,account_id,&origin)?.into_keys().collect())
}

pub fn quarantined_grants(cfg:&Config,account_id:&str)->Result<Vec<String>,String> {
    #[cfg(target_os = "android")]
    return quarantined_grants_with_store(cfg,account_id,crate::provider_credential::platform_store()?);
    #[cfg(not(target_os = "android"))]
    {
        let (origin,_,path)=account(cfg,account_id)?;
        let store=DesktopCredentialStore {path,account:account_id.into(),origin};
        quarantined_grants_with_store(cfg,account_id,&store)
    }
}

fn bind_subject(attempt:&AuthAttempt, store:&dyn ProviderCredentialStore, subject:&str) -> Result<(),String> {
    let binding_id = format!("isyncyou-internal:subject:{:x}",Sha256::digest(attempt.account_id.as_bytes()));
    match store.load(&binding_id,&attempt.origin)? {
        Some(binding) if binding.subject.as_deref() == Some(subject) => Ok(()),
        Some(_) => Err("Signed-in identity differs from the account previously bound to this provider".into()),
        None => {
            if store.load(&attempt.account_id,&attempt.origin)?.is_some() || crate::provider_store::has_cached_data(&attempt.account_config,&attempt.account_id)? {
                return Err("Existing provider data lacks a verified identity binding".into());
            }
            store.replace(&binding_id,&attempt.origin,&ProviderCredential {access_token:subject.into(),refresh_token:None,expires_at:0,subject:Some(subject.into())})
        }
    }
}

fn verify_userinfo(origin:&str, token:&str, expected_username:&str) -> Result<String,String> {
    let base = reqwest::Url::parse(origin).map_err(|_| "Invalid provider origin")?;
    let discovery = json_response(client()?.get(base.join("/.well-known/openid-configuration").map_err(|_| "Invalid provider origin")?).send().map_err(|_| "Identity discovery unavailable")?)?;
    if discovery["issuer"].as_str() != Some(origin) { return Err("OAuth issuer mismatch".into()); }
    let endpoint = same_origin(&base,discovery["userinfo_endpoint"].as_str().ok_or("Identity endpoint unavailable")?)?;
    let identity = json_response(client()?.get(endpoint).bearer_auth(token).send().map_err(|_| "Identity lookup unavailable")?)?;
    validate_userinfo(&identity,expected_username)
}

fn validate_userinfo(identity:&Value, expected_username:&str) -> Result<String,String> {
    let subject = identity["sub"].as_str().filter(|value| !value.is_empty() && value.len() <= 4096).ok_or("Identity subject missing")?;
    if identity["preferred_username"].as_str() != Some(expected_username) && identity["email"].as_str() != Some(expected_username) { return Err("Signed-in account does not match configured username".into()); }
    Ok(subject.into())
}

/// Reject unrelated loopback requests without consuming the pending attempt.
pub fn validate_callback(attempt: &AuthAttempt, callback: &str) -> Result<(), String> {
    callback_code(attempt, callback).map(|_| ())
}

/// Classify an authenticated callback without exposing its authorization code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthCallbackKind { Code, Rejected }

pub fn classify_callback(attempt: &AuthAttempt, callback: &str) -> Result<AuthCallbackKind, String> {
    Ok(if callback_code(attempt, callback)?.is_some() { AuthCallbackKind::Code } else { AuthCallbackKind::Rejected })
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

/// Cross-process lease for the one event stream of a configured account.
pub(crate) fn try_event_stream_lease(cfg: &Config, id: &str) -> Result<std::fs::File, String> {
    let (_, _, path) = account(cfg,id)?;
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path.with_extension("event.lock")).map_err(|_| "Event stream lease unavailable")?;
    file.try_lock_exclusive().map_err(|_| "Event stream already active")?;
    Ok(file)
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
    access_token_locked(id, &origin, &client_id, store)
}

fn access_token_locked(id: &str, origin: &str, client_id: &str, store: &dyn ProviderCredentialStore) -> Result<String, String> {
    if store.load(&revocation_pending_key(id), &origin)?.is_some() { return Err("Provider revocation pending; sign in again after recovery".into()); }
    let mut cached = store.load(id, &origin)?.ok_or("Provider login required")?;
    if cached.subject.as_deref().is_none_or(str::is_empty) { return Err("Provider account identity must be verified by signing in again".into()); }
    let binding_id = format!("isyncyou-internal:subject:{:x}",Sha256::digest(id.as_bytes()));
    let binding = store.load(&binding_id,&origin)?.ok_or("Provider account identity binding missing")?;
    if binding.subject != cached.subject { return Err("Provider account identity binding mismatch".into()); }
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
    if token.access_token.is_empty() || token.refresh_token.as_deref() == Some("") {
        return Err("Invalid OAuth refresh response".into());
    }
    let previous = cached.refresh_token.take();
    let cache = TokenCache::from_response(&token, now());
    cached = ProviderCredential {
        access_token: cache.access_token,
        refresh_token: cache.refresh_token.or(previous),
        expires_at: cache.expires_at,
        subject: cached.subject,
    };
    store.replace(id, &origin, &cached)?;
    Ok(cached.access_token)
}

/// Internal engine-only seam for a bounded native dispatch that must be
/// serialized with logout. The callback must not reacquire the account lock.
pub(crate) fn with_locked_provider_access_token<T>(cfg: &Config, id: &str, store: &dyn ProviderCredentialStore, action: impl FnOnce(&str) -> Result<T, String>) -> Result<T, String> {
    let (origin, client_id, path) = account(cfg,id)?;
    let _lock = lock(&path)?;
    let token = access_token_locked(id,&origin,&client_id,store)?;
    action(&token)
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

/// Supply the existing account/origin-bound keyring adapter to native recovery
/// code. The callback must use the receiving operation's own account lock and
/// must never expose credential contents to the UI or logs.
#[cfg(not(target_os = "android"))]
pub fn with_desktop_credential_store<T>(cfg: &Config, id: &str, action: impl FnOnce(&dyn ProviderCredentialStore) -> Result<T, String>) -> Result<T, String> {
    let (origin, _, path) = account(cfg, id)?;
    let store = DesktopCredentialStore {path,account:id.into(),origin};
    action(&store)
}

/// Supply the native credential adapter to account-scoped engine operations.
/// Operations remain responsible for their account lock and must not expose
/// credentials to the UI, logs, or an unrestricted signing interface.
pub fn with_platform_credential_store<T>(cfg: &Config, id: &str, action: impl FnOnce(&dyn ProviderCredentialStore) -> Result<T, String>) -> Result<T, String> {
    #[cfg(target_os = "android")]
    {
        account(cfg,id)?;
        action(crate::provider_credential::platform_store()?)
    }
    #[cfg(not(target_os = "android"))]
    { with_desktop_credential_store(cfg,id,action) }
}
#[cfg(not(target_os = "android"))]
impl DesktopCredentialStore {
    fn path_for(&self, account:&str, origin:&str) -> Result<PathBuf,String> {
        if origin != self.origin { return Err("Credential account mismatch".into()); }
        if account == self.account { return Ok(self.path.clone()); }
        if self.account.starts_with("isyncyou-internal:") { return Err("Credential account mismatch".into()); }
        let binding = format!("isyncyou-internal:subject:{:x}",Sha256::digest(self.account.as_bytes()));
        if account == binding { return Ok(self.path.with_extension("subject.json")); }
        if account == quarantine_key(&self.account) { return Ok(self.path.with_extension("quarantine.json")); }
        if account == revocation_pending_key(&self.account) { return Ok(self.path.with_extension("revocation-pending.json")); }
        Err("Credential account mismatch".into())
    }
}
#[cfg(not(target_os = "android"))]
impl ProviderCredentialStore for DesktopCredentialStore {
    fn load(&self, account: &str, origin: &str) -> Result<Option<ProviderCredential>, String> {
        let path = self.path_for(account,origin)?;
        if !path.exists() {
            return Ok(None);
        }
        let token = load_keyring(&path)?;
        Ok(Some(ProviderCredential {
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            expires_at: token.expires_at,
            subject: token.account_subject,
        }))
    }
    fn replace(
        &self,
        account: &str,
        origin: &str,
        credential: &ProviderCredential,
    ) -> Result<(), String> {
        let path = self.path_for(account,origin)?;
        TokenCache {
            access_token: credential.access_token.clone(),
            refresh_token: credential.refresh_token.clone(),
            expires_at: credential.expires_at,
            account_subject: credential.subject.clone(),
        }
        .save_to_keyring(&path)
        .map_err(|_| "System keyring unavailable".to_string())?;
        if account == revocation_pending_key(&self.account) {
            std::fs::File::open(&path).and_then(|file| file.sync_all()).map_err(|_| "Revocation marker durability unavailable")?;
            let parent = path.parent().ok_or("Revocation marker directory unavailable")?;
            std::fs::File::open(parent).and_then(|file| file.sync_all()).map_err(|_| "Revocation marker durability unavailable")?;
        }
        Ok(())
    }
    fn delete(&self, account: &str, origin: &str) -> Result<(), String> {
        let path = self.path_for(account,origin)?;
        delete_desktop_credential(&path)?;
        if account == revocation_pending_key(&self.account) {
            let parent = path.parent().ok_or("Revocation marker directory unavailable")?;
            std::fs::File::open(parent).and_then(|file| file.sync_all()).map_err(|_| "Revocation marker durability unavailable")?;
        }
        Ok(())
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
    Ok(verified_subject_locked(store,id,&origin)?.is_some())
}

/// Return only the locally bound provider subject when the current installation
/// still holds a matching refresh grant. This performs no token refresh or I/O
/// to the provider; the subject is sensitive account metadata, never a token.
pub fn verified_local_provider_subject(cfg: &Config, id: &str) -> Result<Option<String>, String> {
    #[cfg(target_os = "android")]
    return verified_local_provider_subject_with_store(cfg, id, crate::provider_credential::platform_store()?);
    #[cfg(not(target_os = "android"))]
    {
        let (origin, _, path) = account(cfg, id)?;
        let store = DesktopCredentialStore { path, account:id.into(), origin };
        verified_local_provider_subject_with_store(cfg, id, &store)
    }
}

pub fn verified_local_provider_subject_with_store(cfg: &Config, id: &str, store: &dyn ProviderCredentialStore) -> Result<Option<String>, String> {
    let (origin, _, path) = account(cfg, id)?;
    let _lock = lock(&path)?;
    verified_subject_locked(store,id,&origin)
}

fn verified_subject_locked(store: &dyn ProviderCredentialStore, id: &str, origin: &str) -> Result<Option<String>, String> {
    if store.load(&revocation_pending_key(id), origin)?.is_some() { return Ok(None); }
    let Some(credential) = store.load(id, &origin)? else { return Ok(None); };
    if credential.refresh_token.as_deref().is_none_or(str::is_empty) { return Ok(None); }
    let Some(subject) = credential.subject.filter(|s| !s.is_empty() && s.len() <= 4096) else { return Ok(None); };
    let binding_id = format!("isyncyou-internal:subject:{:x}", Sha256::digest(id.as_bytes()));
    let binding = store.load(&binding_id, &origin)?;
    Ok(if binding.and_then(|value| value.subject).as_deref() == Some(subject.as_str()) { Some(subject) } else { None })
}

/// Execute a local journal operation under the same account lock as refresh and
/// logout. The callback must not call another function that acquires this lock.
pub fn with_locked_provider_subject<T>(cfg: &Config, id: &str, action: impl FnOnce(Option<&str>) -> Result<T, String>) -> Result<T, String> {
    #[cfg(target_os = "android")]
    return with_locked_provider_subject_with_store(cfg, id, crate::provider_credential::platform_store()?, action);
    #[cfg(not(target_os = "android"))]
    {
        let (origin, _, path) = account(cfg, id)?;
        let desktop = DesktopCredentialStore { path, account:id.into(), origin };
        with_locked_provider_subject_with_store(cfg,id,&desktop,action)
    }
}

pub fn with_locked_provider_subject_with_store<T>(cfg: &Config, id: &str, store: &dyn ProviderCredentialStore, action: impl FnOnce(Option<&str>) -> Result<T, String>) -> Result<T, String> {
    let (origin, _, path) = account(cfg, id)?;
    let _lock = lock(&path)?;
    let subject = verified_subject_locked(store,id,&origin)?;
    action(subject.as_deref())
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
    if let Some(marker) = store.load(&revocation_pending_key(id), &origin)? {
        return Err(match marker.access_token.as_str() { "revoked" => "Grant already revoked; local cleanup may be required", "rejected" => "Provider rejected revocation; recovery required", _ => "Revocation outcome unknown; recovery required" }.into());
    }
    let cache = store.load(id, &origin)?.ok_or("Provider login required")?;
    let refresh = cache
        .refresh_token
        .as_deref()
        .ok_or("Provider login required")?;
    store.replace(&revocation_pending_key(id), &origin, &ProviderCredential { access_token:"pending".into(), refresh_token:None, expires_at:0, subject:None })?;
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
        if response.status().is_client_error() {
            store.replace(&revocation_pending_key(id), &origin, &ProviderCredential { access_token:"rejected".into(), refresh_token:None, expires_at:0, subject:None })?;
            return Err("Provider rejected revocation".into());
        }
        return Err("Revocation outcome unknown".into());
    }
    store.replace(&revocation_pending_key(id), &origin, &ProviderCredential { access_token:"revoked".into(), refresh_token:None, expires_at:0, subject:None })?;
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
                    subject: None,
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

    #[test]
    fn event_stream_lease_is_exclusive_and_released() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.accounts.push(isyncyou_core::AccountConfig {id:"a".into(),username:"user".into(),sync_root:PathBuf::new(),archive_root:dir.path().into(),cache_root:PathBuf::new(),mount_point:None});
        cfg.provider_accounts.insert("a".into(),ProviderConfig::SelfHosted {origin:"https://example.test".into(),client_id:"client".into()});
        let first = try_event_stream_lease(&cfg,"a").unwrap();
        assert!(try_event_stream_lease(&cfg,"a").is_err());
        drop(first);
        assert!(try_event_stream_lease(&cfg,"a").is_ok());
    }

    #[test]
    fn pending_revocation_disables_local_subject_and_access_under_account_lock() {
        struct Keys(std::sync::Mutex<std::collections::HashMap<(String,String),ProviderCredential>>);
        impl ProviderCredentialStore for Keys {
            fn load(&self,a:&str,o:&str)->Result<Option<ProviderCredential>,String>{Ok(self.0.lock().unwrap().get(&(a.into(),o.into())).cloned())}
            fn replace(&self,a:&str,o:&str,c:&ProviderCredential)->Result<(),String>{self.0.lock().unwrap().insert((a.into(),o.into()),c.clone());Ok(())}
            fn delete(&self,a:&str,o:&str)->Result<(),String>{self.0.lock().unwrap().remove(&(a.into(),o.into()));Ok(())}
        }
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.accounts.push(isyncyou_core::AccountConfig {id:"a".into(),username:"user".into(),sync_root:PathBuf::new(),archive_root:dir.path().into(),cache_root:PathBuf::new(),mount_point:None});
        cfg.provider_accounts.insert("a".into(),ProviderConfig::SelfHosted {origin:"https://example.test".into(),client_id:"client".into()});
        let keys=Keys(std::sync::Mutex::new(std::collections::HashMap::new()));
        let credential=ProviderCredential {access_token:"secret".into(),refresh_token:Some("refresh".into()),expires_at:now()+600,subject:Some("subject".into())};
        keys.replace("a","https://example.test",&credential).unwrap();
        let binding=format!("isyncyou-internal:subject:{:x}",Sha256::digest(b"a"));
        keys.replace(&binding,"https://example.test",&credential).unwrap();
        assert_eq!(with_locked_provider_subject_with_store(&cfg,"a",&keys,|subject| Ok(subject.map(str::to_owned))).unwrap().as_deref(),Some("subject"));
        keys.replace(&revocation_pending_key("a"),"https://example.test",&credential).unwrap();
        assert_eq!(with_locked_provider_subject_with_store(&cfg,"a",&keys,|subject| Ok(subject.map(str::to_owned))).unwrap(),None);
        assert!(self_hosted_access_token_with_store(&cfg,"a",&keys).is_err());
        assert!(!self_hosted_auth_status_with_store(&cfg,"a",&keys).unwrap());
        assert!(logout_self_hosted_with_store(&cfg,"a",&keys).unwrap_err().contains("outcome unknown"));
    }

    #[test]
    fn locked_dispatch_serializes_revocation_and_prevents_reuse() {
        struct Keys(std::sync::Mutex<std::collections::HashMap<(String,String),ProviderCredential>>);
        impl ProviderCredentialStore for Keys {
            fn load(&self,a:&str,o:&str)->Result<Option<ProviderCredential>,String>{Ok(self.0.lock().unwrap().get(&(a.into(),o.into())).cloned())}
            fn replace(&self,a:&str,o:&str,c:&ProviderCredential)->Result<(),String>{self.0.lock().unwrap().insert((a.into(),o.into()),c.clone());Ok(())}
            fn delete(&self,a:&str,o:&str)->Result<(),String>{self.0.lock().unwrap().remove(&(a.into(),o.into()));Ok(())}
        }
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.accounts.push(isyncyou_core::AccountConfig {id:"a".into(),username:"user".into(),sync_root:PathBuf::new(),archive_root:dir.path().into(),cache_root:PathBuf::new(),mount_point:None});
        cfg.provider_accounts.insert("a".into(),ProviderConfig::SelfHosted {origin:"https://example.test".into(),client_id:"client".into()});
        let keys=Keys(std::sync::Mutex::new(std::collections::HashMap::new()));
        let credential=ProviderCredential {access_token:"secret".into(),refresh_token:Some("refresh".into()),expires_at:now()+600,subject:Some("subject".into())};
        keys.replace("a","https://example.test",&credential).unwrap();
        keys.replace(&format!("isyncyou-internal:subject:{:x}",Sha256::digest(b"a")),"https://example.test",&credential).unwrap();
        let (entered_tx,entered_rx)=std::sync::mpsc::channel();
        let (release_tx,release_rx)=std::sync::mpsc::channel();
        let (attempt_tx,attempt_rx)=std::sync::mpsc::channel();
        let (done_tx,done_rx)=std::sync::mpsc::channel();
        std::thread::scope(|threads| {
            let cfg_ref = &cfg; let keys_ref = &keys; let credential_ref = &credential;
            threads.spawn(move || with_locked_provider_access_token(cfg_ref,"a",keys_ref,|_| {
                entered_tx.send(()).unwrap(); release_rx.recv().unwrap(); Ok(())
            }).unwrap());
            entered_rx.recv().unwrap();
            threads.spawn(move || {
                attempt_tx.send(()).unwrap();
                with_locked_provider_subject_with_store(cfg_ref,"a",keys_ref,|subject| {
                    assert_eq!(subject,Some("subject"));
                    keys_ref.replace(&revocation_pending_key("a"),"https://example.test",credential_ref)
                }).unwrap();
                done_tx.send(()).unwrap();
            });
            attempt_rx.recv().unwrap();
            let blocked = done_rx.recv_timeout(Duration::from_millis(100)).is_err();
            release_tx.send(()).unwrap();
            if blocked { done_rx.recv_timeout(Duration::from_secs(5)).unwrap(); }
            assert!(blocked);
        });
        assert!(with_locked_provider_access_token(&cfg,"a",&keys,|_| Ok(())).is_err());
    }

    #[test]
    fn android_attempt_roundtrip_is_scoped_and_expires() {
        let mut cfg = Config::default();
        cfg.accounts.push(isyncyou_core::AccountConfig { id:"a".into(), username:"user".into(), sync_root:PathBuf::new(), archive_root:PathBuf::new(), cache_root:PathBuf::new(), mount_point:None });
        cfg.provider_accounts.insert("a".into(), ProviderConfig::SelfHosted { origin:"https://example.test".into(), client_id:"client".into() });
        let redirect = "com.silentspike.isyncyou:/oauth/callback";
        let attempt = AuthAttempt { authorization_uri:String::new(), origin:"https://example.test".into(), client_id:"client".into(), token_endpoint:"https://example.test/token".into(), revocation_endpoint:"https://example.test/revoke".into(), redirect_uri:redirect.into(), verifier:random().unwrap(), state:random().unwrap(), cache_path:PathBuf::new(), account_id:"a".into(), expected_username:"user".into(), account_config:cfg.clone(), started_at:now() };
        let bytes = export_android_auth_attempt(&attempt).unwrap();
        let restored = import_android_auth_attempt(&cfg,"a",redirect,&bytes).unwrap();
        assert_eq!(restored.state, attempt.state);
        assert_eq!(restored.verifier, attempt.verifier);
        assert!(import_android_auth_attempt(&cfg,"a","com.silentspike.isyncyou.debug:/oauth/callback",&bytes).is_err());
        let mut changed = cfg.clone();
        changed.accounts[0].username = "other".into();
        assert!(import_android_auth_attempt(&changed,"a",redirect,&bytes).is_err());
        let mut saved: AndroidAuthAttempt = serde_json::from_slice(&bytes).unwrap();
        saved.started_at = now().saturating_sub(601);
        assert!(import_android_auth_attempt(&cfg,"a",redirect,&serde_json::to_vec(&saved).unwrap()).is_err());
        saved.started_at = now();
        saved.token_endpoint = "https://evil.test/token".into();
        assert!(import_android_auth_attempt(&cfg,"a",redirect,&serde_json::to_vec(&saved).unwrap()).is_err());
    }

    #[test]
    fn userinfo_requires_exact_configured_identity_and_stable_subject() {
        let identity = serde_json::json!({"sub":"subject-1","preferred_username":"user@example.test"});
        assert_eq!(validate_userinfo(&identity,"user@example.test").unwrap(),"subject-1");
        assert!(validate_userinfo(&identity,"other@example.test").is_err());
        assert!(validate_userinfo(&serde_json::json!({"sub":"","email":"user@example.test"}),"user@example.test").is_err());
    }

    #[test]
    fn quarantined_grant_index_is_scoped_and_exposes_only_opaque_ids() {
        struct Keys(std::sync::Mutex<std::collections::HashMap<(String,String),ProviderCredential>>);
        impl ProviderCredentialStore for Keys {
            fn load(&self,a:&str,o:&str)->Result<Option<ProviderCredential>,String>{Ok(self.0.lock().unwrap().get(&(a.into(),o.into())).cloned())}
            fn replace(&self,a:&str,o:&str,c:&ProviderCredential)->Result<(),String>{self.0.lock().unwrap().insert((a.into(),o.into()),c.clone());Ok(())}
            fn delete(&self,a:&str,o:&str)->Result<(),String>{self.0.lock().unwrap().remove(&(a.into(),o.into()));Ok(())}
        }
        let keys=Keys(std::sync::Mutex::new(std::collections::HashMap::new()));
        let id="a".repeat(64);
        let mut map=std::collections::BTreeMap::new(); map.insert(id.clone(),"refresh-secret".into());
        save_quarantine(&keys,"a","https://one.example",&map).unwrap();
        assert_eq!(load_quarantine(&keys,"a","https://one.example").unwrap().keys().cloned().collect::<Vec<_>>(),vec![id]);
        assert!(load_quarantine(&keys,"a","https://other.example").unwrap().is_empty());
        assert!(load_quarantine(&keys,"b","https://one.example").unwrap().is_empty());
        #[cfg(feature = "encrypted-store")]
        {
            let dir=tempfile::tempdir().unwrap();
            let mut cfg=Config::default();
            cfg.accounts.push(isyncyou_core::AccountConfig {id:"a".into(),username:"user".into(),sync_root:PathBuf::new(),archive_root:dir.path().into(),cache_root:PathBuf::new(),mount_point:None});
            cfg.provider_accounts.insert("a".into(),ProviderConfig::SelfHosted {origin:"https://one.example".into(),client_id:"client".into()});
            let attempt=AuthAttempt {authorization_uri:String::new(),origin:"https://one.example".into(),client_id:"client".into(),token_endpoint:String::new(),revocation_endpoint:String::new(),redirect_uri:String::new(),verifier:String::new(),state:String::new(),cache_path:PathBuf::new(),account_id:"a".into(),expected_username:"user".into(),account_config:cfg,started_at:now()};
            bind_subject(&attempt,&keys,"subject-one").unwrap();
            bind_subject(&attempt,&keys,"subject-one").unwrap();
            assert!(bind_subject(&attempt,&keys,"subject-two").is_err());
        }
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn desktop_identity_and_quarantine_paths_are_separate_and_account_bound() {
        let store = DesktopCredentialStore {path:PathBuf::from("/tmp/provider-token.json"),account:"a".into(),origin:"https://example.test".into()};
        let binding=format!("isyncyou-internal:subject:{:x}",Sha256::digest(b"a"));
        let quarantine=quarantine_key("a");
        let token_path=store.path_for("a","https://example.test").unwrap();
        assert_ne!(store.path_for(&binding,"https://example.test").unwrap(),token_path);
        assert_ne!(store.path_for(&quarantine,"https://example.test").unwrap(),token_path);
        assert!(store.path_for(&binding,"https://other.test").is_err());
        assert!(store.path_for("other","https://example.test").is_err());
    }

    struct FakeStore(std::sync::Mutex<Option<ProviderCredential>>);
    impl ProviderCredentialStore for FakeStore {
        fn load(&self, account: &str, origin: &str) -> Result<Option<ProviderCredential>, String> {
            if account == revocation_pending_key("a") && origin == "https://example.test" { return Ok(None); }
            if (account != "a" && account != format!("isyncyou-internal:subject:{:x}",Sha256::digest(b"a"))) || origin != "https://example.test" {
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
            subject: Some("stable-subject".into()),
        })));
        assert!(self_hosted_auth_status_with_store(&cfg, "a", &store).unwrap());
        assert_eq!(verified_local_provider_subject_with_store(&cfg, "a", &store).unwrap().as_deref(), Some("stable-subject"));
        with_locked_provider_access_token(&cfg,"a",&store,|token| { assert_eq!(token,"secret"); Ok(()) }).unwrap();
        assert_eq!(
            self_hosted_access_token_with_store(&cfg, "a", &store).unwrap(),
            "secret"
        );
        store.0.lock().unwrap().as_mut().unwrap().subject = None;
        assert_eq!(verified_local_provider_subject_with_store(&cfg, "a", &store).unwrap(), None);
        assert!(!self_hosted_auth_status_with_store(&cfg,"a",&store).unwrap());
        assert!(self_hosted_access_token_with_store(&cfg,"a",&store).is_err());
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
            revocation_endpoint: "https://example.test/revoke".into(),
            redirect_uri: "http://127.0.0.1:53682/oauth/callback".into(),
            verifier: "secret".into(),
            state: "expected".into(),
            cache_path: PathBuf::new(),
            account_id: "a".into(),
            expected_username: "user".into(),
            account_config: Config::default(),
            started_at: now(),
        };
        assert!(validate_callback(&attempt, "http://127.0.0.1:53682/oauth/callback?code=a&state=wrong&iss=https%3A%2F%2Fexample.test").is_err());
        assert!(validate_callback(&attempt, "http://127.0.0.1:53682/oauth/callback?code=a&state=expected&iss=https%3A%2F%2Fevil.test").is_err());
        assert!(validate_callback(&attempt, "http://127.0.0.1:53682/oauth/callback?code=a&state=expected&iss=https%3A%2F%2Fexample.test").is_ok());
        assert!(validate_callback(&attempt, "http://127.0.0.1:53682/oauth/callback?error=access_denied&state=expected&iss=https%3A%2F%2Fexample.test").is_ok());
        assert_eq!(classify_callback(&attempt, "http://127.0.0.1:53682/oauth/callback?code=a&state=expected&iss=https%3A%2F%2Fexample.test").unwrap(), AuthCallbackKind::Code);
        assert_eq!(classify_callback(&attempt, "http://127.0.0.1:53682/oauth/callback?error=access_denied&state=expected&iss=https%3A%2F%2Fexample.test").unwrap(), AuthCallbackKind::Rejected);
        assert!(validate_callback(&attempt, "http://127.0.0.1:53682/oauth/callback?error=access_denied&state=wrong&iss=https%3A%2F%2Fexample.test").is_err());
    }
}
