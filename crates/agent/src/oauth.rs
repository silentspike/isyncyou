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
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;

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
        self.pending
            .lock()
            .unwrap()
            .remove(state)
            .map(|p| (p.verifier, p.redirect_uri))
    }

    /// Forget one pending login without exchanging it. The opaque browser-facing state
    /// never leaves the host; callers use this only after matching their own attempt id.
    pub fn cancel(&self, state: &str) -> bool {
        self.pending.lock().unwrap().remove(state).is_some()
    }
}

// Tiny helper so `start` stays readable.
trait PendingInsert {
    fn borrow_insert(&self, state: &str, verifier: String, redirect_uri: &str);
}
impl PendingInsert for Mutex<HashMap<String, PendingLogin>> {
    fn borrow_insert(&self, state: &str, verifier: String, redirect_uri: &str) {
        self.lock().unwrap().insert(
            state.to_string(),
            PendingLogin {
                verifier,
                redirect_uri: redirect_uri.to_string(),
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

/// Build the Codex authorize URL (PKCE S256) the system browser opens. NOTE: we deliberately
/// do **not** send `codex_cli_simplified_flow` — that makes the consent hand the code to the
/// loopback server over a JS `fetch` (CORS) handshake the CLI's own server implements; we use
/// the plain redirect flow instead, so the browser just navigates to `redirect_uri?code=…`.
/// `id_token_add_organizations` is kept so the id_token carries the ChatGPT account id.
pub fn codex_build_authorize_url(cfg: &CodexOAuthConfig, challenge: &str, state: &str) -> String {
    format!(
        "{base}?response_type=code&client_id={cid}&redirect_uri={ru}&scope={sc}\
         &code_challenge={ch}&code_challenge_method=S256&state={st}\
         &id_token_add_organizations=true",
        base = cfg.authorize_url,
        cid = pct(&cfg.client_id),
        ru = pct(&cfg.redirect_uri),
        sc = pct(&cfg.scope),
        ch = pct(challenge),
        st = pct(state),
    )
}

/// A Codex/ChatGPT credential from the token endpoint.
#[cfg(feature = "http")]
pub struct CodexTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
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
    let account_id =
        codex_account_id_from_id_token(v.get("id_token").and_then(|t| t.as_str()).unwrap_or(""));
    let expires_in = v.get("expires_in").and_then(|t| t.as_u64()).unwrap_or(0);
    Ok(CodexTokens {
        access_token,
        refresh_token,
        account_id,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
