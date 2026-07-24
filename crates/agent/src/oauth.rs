//! Agent provider OAuth login support.
//!
//! Flow (RFC 8252 native-app, PKCE): the app asks [`start`] for an authorize URL; the
//! WebView hands it to the **system browser**; the operator logs in to their own account
//! on the provider's official OAuth page (legitimate login); the browser redirects to the
//! engine's loopback `redirect_uri`; [`exchange`] swaps the code for a token (PKCE
//! verifier), which is then stored in the [`crate::CredentialStore`].
//!
//! Product defaults are non-secret public OAuth client metadata. The app-host product
//! path accepts only this compiled official endpoint/client/scope tuple; a local file
//! cannot replace it.

use crate::AgentError;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const PENDING_LOGIN_TTL: Duration = Duration::from_secs(8 * 60);

/// OAuth endpoints/client. Defaults to the public Claude OAuth client (PKCE public
/// client — these are not secrets). Product callers must enforce this exact tuple.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OAuthConfig {
    pub authorize_url: String,
    pub token_url: String,
    pub client_id: String,
    pub scopes: Vec<String>,
    /// The manual (copy-paste) redirect: Claude shows the code on this page instead of
    /// redirecting to a loopback server. Avoids the whole loopback-redirect fragility.
    pub manual_redirect_url: String,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            // Verified LIVE against the real `claude setup-token` 2.1.197 flow (2026-07-01):
            // an isolated `claude setup-token` (own CLAUDE_CONFIG_DIR) completed the consent AND
            // returned a code at `http://localhost:<port>/callback` using EXACTLY these values,
            // even with a pre-existing grant on the account. The factors that made our earlier
            // attempts fail with "Invalid request format" were (a) the authorize host —
            // `claude.com/cai`, NOT `claude.ai`, and (b) the scope — `user:inference` ONLY (the
            // full org:create_api_key/user:profile/user:sessions set is rejected once a grant
            // exists). token endpoint `platform.claude.com/v1/oauth/token` → HTTP 200.
            authorize_url: "https://claude.com/cai/oauth/authorize".to_string(),
            token_url: "https://platform.claude.com/v1/oauth/token".to_string(),
            client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e".to_string(),
            manual_redirect_url: "https://platform.claude.com/oauth/code/callback".to_string(),
            // setup-token scope: `user:inference` ONLY — exactly what the real `claude
            // setup-token` requests and what the live flow accepted. This is all the agent
            // needs (inference on the subscription).
            scopes: vec!["user:inference".to_string()],
        }
    }
}

fn rand_b64(n: usize) -> Result<String, AgentError> {
    let mut buf = vec![0u8; n];
    SystemRandom::new()
        .fill(&mut buf)
        .map_err(|_| AgentError::Provider("oauth rng".into()))?;
    Ok(B64URL.encode(buf))
}

/// Minimal RFC 3986 percent-encoding for query values (encode everything but unreserved).
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A PKCE pair: the secret `verifier` (kept server-side) and the `challenge` (sent in the
/// authorize URL). `challenge = base64url(sha256(verifier))` (S256).
pub fn pkce() -> Result<(String, String), AgentError> {
    let verifier = rand_b64(32)?;
    let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
    Ok((verifier, B64URL.encode(digest.as_ref())))
}

/// A random URL-safe CSRF `state` token (used by flows that don't go through
/// [`AgentOAuth`], e.g. the Codex loopback-server flow).
pub fn rand_state() -> Result<String, AgentError> {
    rand_b64(32)
}

/// Build the provider authorize URL (PKCE S256) the system browser opens.
pub fn build_authorize_url(
    cfg: &OAuthConfig,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
) -> String {
    let scope = cfg.scopes.join(" ");
    // Param set + order mirror the real Claude Code client (verified from its cli.js):
    // the leading `code=true` is required — without it claude.ai rejects the authorize
    // submit with "Invalid request format".
    format!(
        "{base}?code=true&client_id={cid}&response_type=code&redirect_uri={ru}\
         &scope={sc}&code_challenge={ch}&code_challenge_method=S256&state={st}",
        base = cfg.authorize_url,
        cid = pct(&cfg.client_id),
        ru = pct(redirect_uri),
        sc = pct(&scope),
        ch = pct(challenge),
        st = pct(state),
    )
}

/// One in-flight login, keyed by `state` (CSRF) until the callback completes it.
struct PendingLogin {
    verifier: String,
    redirect_uri: String,
    expires_at: Instant,
}

/// Tracks in-flight logins between [`AgentOAuth::start`] and the browser callback.
#[derive(Default)]
pub struct AgentOAuth {
    pending: Mutex<HashMap<String, PendingLogin>>,
}

/// What [`AgentOAuth::start`] hands back to the UI.
pub struct StartedLogin {
    pub authorize_url: String,
    pub state: String,
}

impl AgentOAuth {
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin a login: generate PKCE + state, remember them by state, return the authorize
    /// URL for the browser. `redirect_uri` is the engine's loopback callback (the client
    /// supplies its own origin, RFC 8252 loopback).
    pub fn start(&self, cfg: &OAuthConfig, redirect_uri: &str) -> Result<StartedLogin, AgentError> {
        let (verifier, challenge) = pkce()?;
        // 32 bytes (43 base64url chars) — matches the real `claude` CLI. A shorter state
        // (16 bytes / 22 chars) is REJECTED by claude.ai's authorize submit with "Invalid
        // request format" (LIVE-verified on-device 2026-07-01: 22-char state fails, 32-char
        // state succeeds). This was the root cause of the in-app Claude login failing.
        let state = rand_b64(32)?;
        let authorize_url = build_authorize_url(cfg, redirect_uri, &challenge, &state);
        self.pending.borrow_insert(&state, verifier, redirect_uri);
        Ok(StartedLogin {
            authorize_url,
            state,
        })
    }

    /// Take the pending login for `state` (single-use), returning (verifier, redirect_uri).
    pub fn take(&self, state: &str) -> Option<(String, String)> {
        let now = Instant::now();
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|_, login| login.expires_at > now);
        pending.remove(state).map(|p| (p.verifier, p.redirect_uri))
    }

    /// Forget one pending login without exchanging it. The opaque browser-facing state
    /// never leaves the host; callers use this only after matching their own attempt id.
    pub fn cancel(&self, state: &str) -> bool {
        self.pending.lock().unwrap().remove(state).is_some()
    }

    /// Remove expired verifier/state pairs without exposing their values.
    pub fn reap_expired(&self) -> usize {
        let now = Instant::now();
        let mut pending = self.pending.lock().unwrap();
        let before = pending.len();
        pending.retain(|_, login| login.expires_at > now);
        before.saturating_sub(pending.len())
    }
}

// Tiny helper so `start` stays readable.
trait PendingInsert {
    fn borrow_insert(&self, state: &str, verifier: String, redirect_uri: &str);
}
impl PendingInsert for Mutex<HashMap<String, PendingLogin>> {
    fn borrow_insert(&self, state: &str, verifier: String, redirect_uri: &str) {
        let now = Instant::now();
        let mut pending = self.lock().unwrap();
        pending.retain(|_, login| login.expires_at > now);
        pending.insert(
            state.to_string(),
            PendingLogin {
                verifier,
                redirect_uri: redirect_uri.to_string(),
                expires_at: now + PENDING_LOGIN_TTL,
            },
        );
    }
}

/// Split a pasted manual code. The manual page shows `code#state`; some flows show just
/// the code. Returns (code, optional state).
pub fn parse_pasted_code(pasted: &str) -> (String, Option<String>) {
    let p = pasted.trim();
    match p.split_once('#') {
        Some((c, s)) => (c.to_string(), Some(s.to_string())),
        None => (p.to_string(), None),
    }
}

/// Exchange an authorization `code` for an access token (PKCE). The token endpoint takes a
/// JSON body; the exact endpoint/client come from `cfg`. `state` is included because the
/// real Claude Code client sends it on exchange (verified from cli.js `ml0`).
#[cfg(feature = "http")]
pub fn exchange(
    http: &crate::http::HttpTransport,
    cfg: &OAuthConfig,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    state: &str,
) -> Result<RefreshedToken, AgentError> {
    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": redirect_uri,
        "client_id": cfg.client_id,
        "code_verifier": verifier,
        "state": state,
        // setup-token exchange: send expires_in=1y. With the `user:inference` scope this is the
        // long-lived-token flow the real `claude setup-token` uses (returns a ~1y token). The
        // earlier 400 "Invalid expiry for scope" happened only when expires_in was sent together
        // with the FULL scope set; it is correct with user:inference.
        "expires_in": 31_536_000,
    });
    let (status, text) = http.post_json(&cfg.token_url, &[], &body)?;
    if status >= 400 {
        return Err(AgentError::Provider(format!(
            "oauth token exchange failed (status {status})"
        )));
    }
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| AgentError::Provider(format!("oauth token JSON: {e}")))?;
    let access_token = v
        .get("access_token")
        .and_then(|t| t.as_str())
        .ok_or_else(|| AgentError::Provider("oauth response had no access_token".into()))?
        .to_string();
    // Keep the refresh token + lifetime so the credential can self-refresh; the subscription
    // token lasts only ~8h (expires_in 28800), so without these the client would
    // "connection-lost" every 8h with no way to renew (LIVE-verified 2026-07-01).
    let refresh_token = v
        .get("refresh_token")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    let expires_in = v.get("expires_in").and_then(|t| t.as_u64()).unwrap_or(0);
    Ok(RefreshedToken {
        access_token,
        refresh_token,
        expires_in,
    })
}

/// A refreshed subscription token: the new access token plus the (possibly rotated)
/// refresh token and its lifetime in seconds.
#[cfg(feature = "http")]
pub struct RefreshedToken {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
}

/// Refresh an expired subscription access token (mirrors the real client's `dl0`): POST the
/// refresh token to the token endpoint for a fresh access token (+ possibly a rotated refresh
/// token). The scope mirrors the real refresh request; the endpoint comes from `cfg`.
#[cfg(feature = "http")]
pub fn refresh(
    http: &crate::http::HttpTransport,
    cfg: &OAuthConfig,
    refresh_token: &str,
) -> Result<RefreshedToken, AgentError> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": cfg.client_id,
        "scope": "user:profile user:inference user:sessions:claude_code",
    });
    let (status, text) = http.post_json(&cfg.token_url, &[], &body)?;
    if status >= 400 {
        return Err(AgentError::Provider(format!(
            "oauth token refresh failed (status {status})"
        )));
    }
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| AgentError::Provider(format!("oauth refresh JSON: {e}")))?;
    let access_token = v
        .get("access_token")
        .and_then(|t| t.as_str())
        .ok_or_else(|| AgentError::Provider("refresh response had no access_token".into()))?
        .to_string();
    // The token endpoint may rotate the refresh token; if it doesn't, keep the current one.
    let refresh_token = v
        .get("refresh_token")
        .and_then(|t| t.as_str())
        .unwrap_or(refresh_token)
        .to_string();
    let expires_in = v.get("expires_in").and_then(|t| t.as_u64()).unwrap_or(0);
    Ok(RefreshedToken {
        access_token,
        refresh_token,
        expires_in,
    })
}

// ---------------------------------------------------------------------------------------
// Codex/ChatGPT (OpenAI) OAuth. Captured from the `codex` CLI (auth.openai.com).
// Loopback flow on the fixed port the client registers; the token
// endpoint wants form-urlencoding (not JSON), and the ChatGPT account id lives in the
// id_token's `https://api.openai.com/auth` claim.
// ---------------------------------------------------------------------------------------

/// OpenAI OAuth endpoints/client for the Codex flow. Defaults to the public `codex` CLI
/// client (a PKCE public client — not a secret).
#[derive(Debug, Clone)]
pub struct CodexOAuthConfig {
    pub authorize_url: String,
    pub token_url: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub scope: String,
}

impl Default for CodexOAuthConfig {
    fn default() -> Self {
        Self {
            authorize_url: "https://auth.openai.com/oauth/authorize".to_string(),
            token_url: "https://auth.openai.com/oauth/token".to_string(),
            client_id: "app_EMoamEEZ73f0CkXaXp7hrann".to_string(),
            // OpenAI registers exactly this loopback redirect; other ports are rejected
            // (verified: an arbitrary port returns `authorize_hydra_invalid_request`).
            redirect_uri: "http://localhost:1455/auth/callback".to_string(),
            scope: "openid profile email offline_access api.connectors.read api.connectors.invoke"
                .to_string(),
        }
    }
}

/// Required public-client identity used by the official Codex OAuth flow.
pub const CODEX_OAUTH_ORIGINATOR: &str = "codex_cli_rs";
pub const CODEX_OAUTH_SIMPLIFIED_FLOW: &str = "true";

/// Build the Codex authorize URL (PKCE S256) the system browser opens. The simplified-flow and
/// originator fields are part of the current official Codex public-client request. The official
/// client still receives the authorization code through the ordinary loopback redirect, so our
/// callback server can consume the same `redirect_uri?code=...&state=...` request.
pub fn codex_build_authorize_url(cfg: &CodexOAuthConfig, challenge: &str, state: &str) -> String {
    format!(
        "{base}?response_type=code&client_id={cid}&redirect_uri={ru}&scope={sc}\
         &code_challenge={ch}&code_challenge_method=S256&id_token_add_organizations=true\
         &codex_cli_simplified_flow={simplified}&state={st}&originator={originator}",
        base = cfg.authorize_url,
        cid = pct(&cfg.client_id),
        ru = pct(&cfg.redirect_uri),
        sc = pct(&cfg.scope),
        ch = pct(challenge),
        st = pct(state),
        simplified = CODEX_OAUTH_SIMPLIFIED_FLOW,
        originator = CODEX_OAUTH_ORIGINATOR,
    )
}

/// A Codex/ChatGPT credential from the token endpoint.
#[cfg(feature = "http")]
pub struct CodexTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
    /// Used only for immediate OIDC validation. Callers must not persist or log it.
    pub id_token: String,
    pub expires_in: u64,
}

/// Extract the ChatGPT `account_id` from an id_token JWT (claim
/// `https://api.openai.com/auth` → `chatgpt_account_id`). Empty if it cannot be read.
#[cfg(feature = "http")]
fn codex_account_id_from_id_token(id_token: &str) -> String {
    let payload = match id_token.split('.').nth(1) {
        Some(p) => p,
        None => return String::new(),
    };
    let bytes = match B64URL.decode(payload) {
        Ok(b) => b,
        Err(_) => return String::new(),
    };
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| {
            v.get("https://api.openai.com/auth")
                .and_then(|a| a.get("chatgpt_account_id"))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default()
}

#[cfg(feature = "http")]
fn codex_parse_tokens(text: &str, fallback_refresh: &str) -> Result<CodexTokens, AgentError> {
    let v: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| AgentError::Provider(format!("codex token JSON: {e}")))?;
    let access_token = v
        .get("access_token")
        .and_then(|t| t.as_str())
        .ok_or_else(|| AgentError::Provider("codex response had no access_token".into()))?
        .to_string();
    let refresh_token = v
        .get("refresh_token")
        .and_then(|t| t.as_str())
        .unwrap_or(fallback_refresh)
        .to_string();
    let id_token = v
        .get("id_token")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let account_id = codex_account_id_from_id_token(&id_token);
    let expires_in = v.get("expires_in").and_then(|t| t.as_u64()).unwrap_or(0);
    Ok(CodexTokens {
        access_token,
        refresh_token,
        account_id,
        id_token,
        expires_in,
    })
}

/// Exchange a Codex authorization `code` (form-urlencoded, PKCE) for tokens.
#[cfg(feature = "http")]
pub fn codex_exchange(
    http: &crate::http::HttpTransport,
    cfg: &CodexOAuthConfig,
    code: &str,
    verifier: &str,
) -> Result<CodexTokens, AgentError> {
    let (status, text) = http.post_form(
        &cfg.token_url,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &cfg.redirect_uri),
            ("client_id", &cfg.client_id),
            ("code_verifier", verifier),
        ],
    )?;
    if status >= 400 {
        return Err(AgentError::Provider(format!(
            "codex token exchange failed (status {status}): {}",
            text.chars().take(300).collect::<String>()
        )));
    }
    codex_parse_tokens(&text, "")
}

/// Refresh a Codex access token (form-urlencoded).
#[cfg(feature = "http")]
pub fn codex_refresh(
    http: &crate::http::HttpTransport,
    cfg: &CodexOAuthConfig,
    refresh_token: &str,
) -> Result<CodexTokens, AgentError> {
    let (status, text) = http.post_form(
        &cfg.token_url,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", &cfg.client_id),
            ("scope", &cfg.scope),
        ],
    )?;
    if status >= 400 {
        return Err(AgentError::Provider(format!(
            "codex token refresh failed (status {status})"
        )));
    }
    codex_parse_tokens(&text, refresh_token)
}

// ---------------------------------------------------------------------------------------
// Product credential revocation. Endpoints, client identities, token selection and
// deadlines are compiled policy; callers can provide credentials but cannot override the
// reviewed provider contract.
// ---------------------------------------------------------------------------------------

const CLAUDE_REVOKE_TIMEOUT: Duration = Duration::from_secs(5);
const CODEX_REVOKE_TIMEOUT: Duration = Duration::from_secs(10);
const CODEX_REVOKE_URL: &str = "https://auth.openai.com/oauth/revoke";

/// Which credential authority was sent to the provider revoke endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevokeRequestTarget {
    RefreshToken,
    AccessToken,
}

/// What the reviewed provider contract allows the product to claim about revocation scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevokeScopeGuarantee {
    GuaranteedTokenSession,
    ObservedTokenSession,
    Unknown,
    FullGrant,
}

/// Closed public failure codes for one provider revoke attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeFailureCode {
    InvalidCredential,
    ConnectTimedOut,
    NameResolutionFailed,
    TlsFailed,
    ConnectFailed,
    RateLimited,
    ProviderUnavailable,
    Rejected,
}

/// Result of one provider revoke request. No provider body or raw error is retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeOutcome {
    Confirmed {
        request_target: RevokeRequestTarget,
        scope_guarantee: RevokeScopeGuarantee,
    },
    Retryable {
        code: RevokeFailureCode,
    },
    Terminal {
        code: RevokeFailureCode,
    },
}

/// Product credential material accepted by the closed revoke builder.
#[derive(Clone, Default)]
pub struct RevokeCredential {
    pub access_token: Option<crate::Secret>,
    pub refresh_token: Option<crate::Secret>,
}

impl fmt::Debug for RevokeCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RevokeCredential")
            .field("access_token_present", &self.access_token.is_some())
            .field("refresh_token_present", &self.refresh_token.is_some())
            .finish()
    }
}

/// Immutable provider request. Its custom Debug implementation exposes policy metadata only.
pub struct ProductRevokeRequest {
    url: &'static str,
    body: crate::Secret,
    timeout: Duration,
    request_target: RevokeRequestTarget,
    scope_guarantee: RevokeScopeGuarantee,
}

impl ProductRevokeRequest {
    pub fn url(&self) -> &'static str {
        self.url
    }

    pub fn body(&self) -> &crate::Secret {
        &self.body
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

impl fmt::Debug for ProductRevokeRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProductRevokeRequest")
            .field("provider_endpoint", &"[compiled provider endpoint]")
            .field("body", &"[redacted]")
            .field("timeout", &self.timeout)
            .field("request_target", &self.request_target)
            .field("scope_guarantee", &self.scope_guarantee)
            .finish()
    }
}

/// Injected transport used by host code and deterministic unit tests.
pub trait ProductRevokeTransport {
    fn send(
        &self,
        request: &ProductRevokeRequest,
    ) -> Result<u16, crate::http::SecretJsonTransportError>;
}

#[cfg(feature = "http")]
impl ProductRevokeTransport for crate::http::HttpTransport {
    fn send(
        &self,
        request: &ProductRevokeRequest,
    ) -> Result<u16, crate::http::SecretJsonTransportError> {
        self.post_secret_json_no_redirect(request.url(), request.body(), request.timeout())
    }
}

fn secret_text(secret: &crate::Secret) -> Result<&str, RevokeFailureCode> {
    let value =
        std::str::from_utf8(secret.expose()).map_err(|_| RevokeFailureCode::InvalidCredential)?;
    if value.trim().is_empty() {
        return Err(RevokeFailureCode::InvalidCredential);
    }
    Ok(value)
}

fn encode_revoke_body(fields: &[(&str, &str)]) -> Result<crate::Secret, RevokeFailureCode> {
    let object: serde_json::Map<String, serde_json::Value> = fields
        .iter()
        .map(|(name, value)| {
            (
                (*name).to_string(),
                serde_json::Value::String((*value).to_string()),
            )
        })
        .collect();
    serde_json::to_vec(&object)
        .map(crate::Secret::new)
        .map_err(|_| RevokeFailureCode::InvalidCredential)
}

fn build_product_revoke_request(
    provider: crate::ProductProviderId,
    credential: &RevokeCredential,
) -> Result<ProductRevokeRequest, RevokeFailureCode> {
    let claude_cfg = OAuthConfig::default();
    let codex_cfg = CodexOAuthConfig::default();
    match provider {
        crate::ProductProviderId::Claude => {
            let token = credential
                .refresh_token
                .as_ref()
                .ok_or(RevokeFailureCode::InvalidCredential)
                .and_then(secret_text)?;
            let url = match claude_cfg.token_url.as_str() {
                "https://platform.claude.com/v1/oauth/token" => {
                    "https://platform.claude.com/v1/oauth/token/revoke"
                }
                _ => return Err(RevokeFailureCode::InvalidCredential),
            };
            let body = encode_revoke_body(&[
                ("token", token),
                ("token_type_hint", "refresh_token"),
                ("client_id", &claude_cfg.client_id),
            ])?;
            Ok(ProductRevokeRequest {
                url,
                body,
                timeout: CLAUDE_REVOKE_TIMEOUT,
                request_target: RevokeRequestTarget::RefreshToken,
                scope_guarantee: RevokeScopeGuarantee::ObservedTokenSession,
            })
        }
        crate::ProductProviderId::Codex => {
            let (secret, target, hint, include_client) =
                if let Some(refresh) = credential.refresh_token.as_ref() {
                    (
                        secret_text(refresh)?,
                        RevokeRequestTarget::RefreshToken,
                        "refresh_token",
                        true,
                    )
                } else if let Some(access) = credential.access_token.as_ref() {
                    (
                        secret_text(access)?,
                        RevokeRequestTarget::AccessToken,
                        "access_token",
                        false,
                    )
                } else {
                    return Err(RevokeFailureCode::InvalidCredential);
                };
            let mut fields = vec![("token", secret), ("token_type_hint", hint)];
            if include_client {
                fields.push(("client_id", &codex_cfg.client_id));
            }
            Ok(ProductRevokeRequest {
                url: CODEX_REVOKE_URL,
                body: encode_revoke_body(&fields)?,
                timeout: CODEX_REVOKE_TIMEOUT,
                request_target: target,
                scope_guarantee: RevokeScopeGuarantee::ObservedTokenSession,
            })
        }
    }
}

/// Revoke one product credential according to the compiled provider contract.
pub fn revoke_product_credential(
    transport: &dyn ProductRevokeTransport,
    provider: crate::ProductProviderId,
    credential: &RevokeCredential,
) -> RevokeOutcome {
    let request = match build_product_revoke_request(provider, credential) {
        Ok(request) => request,
        Err(code) => return RevokeOutcome::Terminal { code },
    };
    match transport.send(&request) {
        Ok(200..=299) => RevokeOutcome::Confirmed {
            request_target: request.request_target,
            scope_guarantee: request.scope_guarantee,
        },
        Ok(408 | 425 | 429) => RevokeOutcome::Retryable {
            code: RevokeFailureCode::RateLimited,
        },
        Ok(500..=599) => RevokeOutcome::Retryable {
            code: RevokeFailureCode::ProviderUnavailable,
        },
        Ok(_) => RevokeOutcome::Terminal {
            code: RevokeFailureCode::Rejected,
        },
        Err(crate::http::SecretJsonTransportError::ConnectTimedOut) => RevokeOutcome::Retryable {
            code: RevokeFailureCode::ConnectTimedOut,
        },
        Err(crate::http::SecretJsonTransportError::NameResolutionFailed) => {
            RevokeOutcome::Retryable {
                code: RevokeFailureCode::NameResolutionFailed,
            }
        }
        Err(crate::http::SecretJsonTransportError::TlsFailed) => RevokeOutcome::Retryable {
            code: RevokeFailureCode::TlsFailed,
        },
        Err(
            crate::http::SecretJsonTransportError::ConnectFailed
            | crate::http::SecretJsonTransportError::InitializationFailed,
        ) => RevokeOutcome::Retryable {
            code: RevokeFailureCode::ConnectFailed,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    fn cfg() -> OAuthConfig {
        OAuthConfig {
            authorize_url: "https://example.invalid/oauth/authorize".into(),
            token_url: "https://example.invalid/oauth/token".into(),
            client_id: "client-123".into(),
            scopes: vec!["a".into(), "b".into()],
            manual_redirect_url: "https://console.invalid/oauth/code/callback".into(),
        }
    }

    #[test]
    fn pkce_challenge_is_deterministic_for_a_verifier() {
        let (verifier, challenge) = pkce().unwrap();
        // recompute the challenge from the verifier
        let d = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
        assert_eq!(challenge, B64URL.encode(d.as_ref()));
        assert!(!verifier.is_empty() && !challenge.is_empty());
    }

    #[test]
    fn authorize_url_has_pkce_s256_and_encoded_params() {
        let url = build_authorize_url(
            &cfg(),
            "http://127.0.0.1:5000/agent/oauth/callback",
            "CH",
            "ST",
        );
        assert!(url.starts_with("https://example.invalid/oauth/authorize?"));
        assert!(url.contains("code=true")); // required by the Claude Code authorize flow
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=client-123"));
        assert!(url.contains("code_challenge=CH"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=ST"));
        assert!(url.contains("scope=a%20b")); // space-joined + percent-encoded
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A5000%2F")); // encoded
    }

    #[test]
    fn start_then_take_is_single_use() {
        let oauth = AgentOAuth::new();
        let started = oauth
            .start(&cfg(), "http://127.0.0.1:5000/agent/oauth/callback")
            .unwrap();
        assert!(started
            .authorize_url
            .contains(&format!("state={}", pct(&started.state))));
        let (verifier, redirect) = oauth.take(&started.state).expect("pending present");
        assert!(!verifier.is_empty());
        assert_eq!(redirect, "http://127.0.0.1:5000/agent/oauth/callback");
        assert!(oauth.take(&started.state).is_none()); // single-use
    }

    #[test]
    fn parse_pasted_code_splits_code_and_state() {
        let (c, s) = parse_pasted_code("  abc123#st-9  ");
        assert_eq!(c, "abc123");
        assert_eq!(s.as_deref(), Some("st-9"));
        let (c2, s2) = parse_pasted_code("onlycode");
        assert_eq!(c2, "onlycode");
        assert_eq!(s2, None);
    }

    #[test]
    fn codex_authorize_url_retains_official_simplified_flow_and_originator() {
        let cfg = CodexOAuthConfig::default();
        let url = codex_build_authorize_url(&cfg, "challenge", "state");
        let (_, query) = url.split_once('?').expect("authorize URL has query");
        let ordered_keys: Vec<&str> = query
            .split('&')
            .map(|field| field.split_once('=').expect("query field has value").0)
            .collect();
        let fields: std::collections::BTreeMap<&str, &str> = query
            .split('&')
            .map(|field| field.split_once('=').expect("query field has value"))
            .collect();

        assert_eq!(
            fields.len(),
            10,
            "no default-client query field may drift in"
        );
        assert_eq!(
            ordered_keys,
            [
                "response_type",
                "client_id",
                "redirect_uri",
                "scope",
                "code_challenge",
                "code_challenge_method",
                "id_token_add_organizations",
                "codex_cli_simplified_flow",
                "state",
                "originator",
            ]
        );
        assert_eq!(
            fields.get("codex_cli_simplified_flow"),
            Some(&CODEX_OAUTH_SIMPLIFIED_FLOW)
        );
        assert_eq!(fields.get("originator"), Some(&CODEX_OAUTH_ORIGINATOR));
        assert_eq!(fields.get("code_challenge_method"), Some(&"S256"));
        assert_eq!(fields.get("id_token_add_organizations"), Some(&"true"));
        assert_eq!(fields.get("state"), Some(&"state"));
    }

    struct RevokeMock {
        result: Result<u16, crate::http::SecretJsonTransportError>,
        seen: StdMutex<Vec<(String, Duration, serde_json::Value)>>,
    }

    impl RevokeMock {
        fn status(status: u16) -> Self {
            Self {
                result: Ok(status),
                seen: StdMutex::new(Vec::new()),
            }
        }
    }

    impl ProductRevokeTransport for RevokeMock {
        fn send(
            &self,
            request: &ProductRevokeRequest,
        ) -> Result<u16, crate::http::SecretJsonTransportError> {
            let body = serde_json::from_slice(request.body().expose()).expect("valid JSON body");
            self.seen
                .lock()
                .unwrap()
                .push((request.url().to_string(), request.timeout(), body));
            self.result
        }
    }

    fn secret(value: &str) -> crate::Secret {
        crate::Secret::new(value.as_bytes().to_vec())
    }

    #[test]
    fn claude_revoke_uses_refresh_token_hint_client_id_and_token_endpoint_suffix() {
        let transport = RevokeMock::status(204);
        let outcome = revoke_product_credential(
            &transport,
            crate::ProductProviderId::Claude,
            &RevokeCredential {
                access_token: Some(secret("unused-access")),
                refresh_token: Some(secret("claude-refresh-sentinel")),
            },
        );
        assert_eq!(
            outcome,
            RevokeOutcome::Confirmed {
                request_target: RevokeRequestTarget::RefreshToken,
                scope_guarantee: RevokeScopeGuarantee::ObservedTokenSession,
            }
        );
        let seen = transport.seen.lock().unwrap();
        assert_eq!(
            seen[0].0,
            "https://platform.claude.com/v1/oauth/token/revoke"
        );
        assert_eq!(seen[0].1, Duration::from_secs(5));
        assert_eq!(seen[0].2["token"], "claude-refresh-sentinel");
        assert_eq!(seen[0].2["token_type_hint"], "refresh_token");
        assert_eq!(seen[0].2["client_id"], OAuthConfig::default().client_id);
        assert_eq!(seen[0].2.as_object().unwrap().len(), 3);
    }

    #[test]
    fn codex_revoke_prefers_refresh_token_and_includes_client_id() {
        let request = build_product_revoke_request(
            crate::ProductProviderId::Codex,
            &RevokeCredential {
                access_token: Some(secret("access-sentinel")),
                refresh_token: Some(secret("refresh-sentinel")),
            },
        )
        .unwrap();
        let body: serde_json::Value = serde_json::from_slice(request.body().expose()).unwrap();
        assert_eq!(request.url(), CODEX_REVOKE_URL);
        assert_eq!(request.timeout(), Duration::from_secs(10));
        assert_eq!(body["token"], "refresh-sentinel");
        assert_eq!(body["token_type_hint"], "refresh_token");
        assert_eq!(body["client_id"], CodexOAuthConfig::default().client_id);
        assert_eq!(body.as_object().unwrap().len(), 3);
    }

    #[test]
    fn codex_revoke_falls_back_to_access_token_without_client_id() {
        let request = build_product_revoke_request(
            crate::ProductProviderId::Codex,
            &RevokeCredential {
                access_token: Some(secret("access-sentinel")),
                refresh_token: None,
            },
        )
        .unwrap();
        let body: serde_json::Value = serde_json::from_slice(request.body().expose()).unwrap();
        assert_eq!(body["token"], "access-sentinel");
        assert_eq!(body["token_type_hint"], "access_token");
        assert!(body.get("client_id").is_none());
        assert_eq!(body.as_object().unwrap().len(), 2);
    }

    #[test]
    fn revoke_rejects_empty_or_unsupported_credential_shape() {
        for (provider, credential) in [
            (
                crate::ProductProviderId::Claude,
                RevokeCredential::default(),
            ),
            (
                crate::ProductProviderId::Claude,
                RevokeCredential {
                    access_token: Some(secret("access-only")),
                    refresh_token: None,
                },
            ),
            (
                crate::ProductProviderId::Codex,
                RevokeCredential {
                    access_token: Some(secret("  ")),
                    refresh_token: None,
                },
            ),
        ] {
            assert_eq!(
                revoke_product_credential(&RevokeMock::status(204), provider, &credential),
                RevokeOutcome::Terminal {
                    code: RevokeFailureCode::InvalidCredential
                }
            );
        }
    }

    #[test]
    fn revoke_does_not_follow_redirects() {
        let outcome = revoke_product_credential(
            &RevokeMock::status(302),
            crate::ProductProviderId::Codex,
            &RevokeCredential {
                access_token: Some(secret("access")),
                refresh_token: None,
            },
        );
        assert_eq!(
            outcome,
            RevokeOutcome::Terminal {
                code: RevokeFailureCode::Rejected
            }
        );
        let transport_source = include_str!("http.rs");
        assert!(transport_source.contains("redirect(reqwest::redirect::Policy::none())"));
        assert!(transport_source
            .contains("header(reqwest::header::CONTENT_TYPE, \"application/json\")"));
    }

    #[test]
    fn revoke_maps_timeout_dns_tls_429_and_5xx_to_closed_typed_failures() {
        let credential = RevokeCredential {
            access_token: Some(secret("access")),
            refresh_token: None,
        };
        for (result, expected) in [
            (
                Err(crate::http::SecretJsonTransportError::ConnectTimedOut),
                RevokeFailureCode::ConnectTimedOut,
            ),
            (
                Err(crate::http::SecretJsonTransportError::NameResolutionFailed),
                RevokeFailureCode::NameResolutionFailed,
            ),
            (
                Err(crate::http::SecretJsonTransportError::TlsFailed),
                RevokeFailureCode::TlsFailed,
            ),
            (Ok(429), RevokeFailureCode::RateLimited),
            (Ok(503), RevokeFailureCode::ProviderUnavailable),
        ] {
            let transport = RevokeMock {
                result,
                seen: StdMutex::new(Vec::new()),
            };
            assert_eq!(
                revoke_product_credential(&transport, crate::ProductProviderId::Codex, &credential),
                RevokeOutcome::Retryable { code: expected }
            );
        }
    }

    #[test]
    fn revoke_maps_unreviewed_4xx_to_terminal_without_response_body() {
        let outcome = revoke_product_credential(
            &RevokeMock::status(418),
            crate::ProductProviderId::Codex,
            &RevokeCredential {
                access_token: Some(secret("body-must-not-escape")),
                refresh_token: None,
            },
        );
        assert_eq!(
            outcome,
            RevokeOutcome::Terminal {
                code: RevokeFailureCode::Rejected
            }
        );
        assert!(!format!("{outcome:?}").contains("body-must-not-escape"));
    }

    #[test]
    fn revoke_success_exposes_scope_and_kind_but_never_token_or_body() {
        let token = "success-secret-sentinel";
        let credential = RevokeCredential {
            access_token: Some(secret(token)),
            refresh_token: None,
        };
        let outcome = revoke_product_credential(
            &RevokeMock::status(200),
            crate::ProductProviderId::Codex,
            &credential,
        );
        assert_eq!(
            outcome,
            RevokeOutcome::Confirmed {
                request_target: RevokeRequestTarget::AccessToken,
                scope_guarantee: RevokeScopeGuarantee::ObservedTokenSession,
            }
        );
        assert!(!format!("{outcome:?} {credential:?}").contains(token));
    }

    #[test]
    fn refresh_token_request_target_does_not_imply_guaranteed_session_scope() {
        let outcome = revoke_product_credential(
            &RevokeMock::status(200),
            crate::ProductProviderId::Claude,
            &RevokeCredential {
                access_token: None,
                refresh_token: Some(secret("refresh")),
            },
        );
        assert_eq!(
            outcome,
            RevokeOutcome::Confirmed {
                request_target: RevokeRequestTarget::RefreshToken,
                scope_guarantee: RevokeScopeGuarantee::ObservedTokenSession,
            }
        );
    }

    #[test]
    fn product_revoke_specs_reject_endpoint_or_client_override() {
        let source = include_str!("oauth.rs");
        let signature = source
            .split("pub fn revoke_product_credential(")
            .nth(1)
            .unwrap()
            .split(") -> RevokeOutcome")
            .next()
            .unwrap();
        assert!(!signature.contains("url"));
        assert!(!signature.contains("client_id"));
        assert!(!signature.contains("OAuthConfig"));
    }

    #[test]
    fn revoke_debug_and_display_are_secret_free() {
        let secret_value = "debug-secret-sentinel";
        let credential = RevokeCredential {
            access_token: Some(secret(secret_value)),
            refresh_token: None,
        };
        let request =
            build_product_revoke_request(crate::ProductProviderId::Codex, &credential).unwrap();
        let rendered = format!("{credential:?} {request:?}");
        assert!(!rendered.contains(secret_value));
        assert!(!rendered.contains(CODEX_REVOKE_URL));
        assert!(rendered.contains("[redacted]"));
    }
}
