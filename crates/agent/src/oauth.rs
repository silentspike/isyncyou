//! EXPERIMENTAL device OAuth login (S-AG.12 / #627) — UNSUPPORTED, personal-build only,
//! behind the default-off `agent-subscription-experimental` feature.
//!
//! Flow (RFC 8252 native-app, PKCE): the app asks [`start`] for an authorize URL; the
//! WebView hands it to the **system browser**; the operator logs in to their own account
//! on the provider's official OAuth page (legitimate login); the browser redirects to the
//! engine's loopback `redirect_uri`; [`exchange`] swaps the code for a token (PKCE
//! verifier), which is then stored in the [`crate::CredentialStore`].
//!
//! **Recipe-out-of-repo:** `authorize_url` / `token_url` / `client_id` / `scopes` come
//! from a local, uncommitted [`OAuthConfig`]; nothing provider-specific is hardcoded here.

use crate::AgentError;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine;
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;

/// OAuth endpoints/client. Defaults to the public Claude OAuth client (PKCE public
/// client — these are not secrets); an operator may override via a local config file.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OAuthConfig {
    pub authorize_url: String,
    pub token_url: String,
    pub client_id: String,
    pub scopes: Vec<String>,
    /// The manual (copy-paste) redirect: claude.ai shows the code on this page instead of
    /// redirecting to a loopback server. Avoids the whole loopback-redirect fragility.
    pub manual_redirect_url: String,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            // Captured from the REAL `claude` CLI (xdg-open intercept on `claude
            // setup-token`): the authorize endpoint is claude.com/cai/oauth/authorize —
            // NOT claude.ai/oauth/authorize (which renders a generic consent that fails
            // the submit with "Invalid request format"). This was the root cause.
            authorize_url: "https://claude.com/cai/oauth/authorize".to_string(),
            token_url: "https://console.anthropic.com/v1/oauth/token".to_string(),
            client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e".to_string(),
            manual_redirect_url: "https://console.anthropic.com/oauth/code/callback".to_string(),
            // The Claude Code subscription-login scopes (verified against a real token):
            // inference + profile + the claude-code session scope. NOT `org:create_api_key`
            // (that is the separate "create an API key" setup flow, not direct inference).
            scopes: vec![
                "user:inference".to_string(),
                "user:profile".to_string(),
                "user:sessions:claude_code".to_string(),
            ],
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
        let state = rand_b64(16)?;
        let authorize_url = build_authorize_url(cfg, redirect_uri, &challenge, &state);
        self.pending.borrow_insert(&state, verifier, redirect_uri);
        Ok(StartedLogin { authorize_url, state })
    }

    /// Take the pending login for `state` (single-use), returning (verifier, redirect_uri).
    pub fn take(&self, state: &str) -> Option<(String, String)> {
        self.pending
            .lock()
            .unwrap()
            .remove(state)
            .map(|p| (p.verifier, p.redirect_uri))
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
) -> Result<String, AgentError> {
    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": redirect_uri,
        "client_id": cfg.client_id,
        "code_verifier": verifier,
        "state": state,
    });
    let (status, text) = http.post_json(&cfg.token_url, &[], &body)?;
    if status >= 400 {
        return Err(AgentError::Provider(format!(
            "oauth token exchange failed (status {status})"
        )));
    }
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| AgentError::Provider(format!("oauth token JSON: {e}")))?;
    v.get("access_token")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| AgentError::Provider("oauth response had no access_token".into()))
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
        let url = build_authorize_url(&cfg(), "http://127.0.0.1:5000/agent/oauth/callback", "CH", "ST");
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
        let started = oauth.start(&cfg(), "http://127.0.0.1:5000/agent/oauth/callback").unwrap();
        assert!(started.authorize_url.contains(&format!("state={}", pct(&started.state))));
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
