//! OAuth for personal Microsoft accounts: a persisted token cache (always
//! available, pure) and the device-code / refresh network flow (feature `http`).
//!
//! Personal accounts use the `consumers` authority and a public client (no
//! secret). The interactive device-code login needs a human once; afterwards the
//! cached refresh token renews access silently.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// `consumers` OAuth 2.0 v2 endpoint base.
pub const AUTHORITY: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0";

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
        serde_json::from_slice(&bytes).map_err(std::io::Error::other)
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, bytes)
    }
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
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn deserializes_real_shaped_response() {
        let v =
            r#"{"access_token":"x","refresh_token":"y","expires_in":3599,"token_type":"Bearer"}"#;
        let t: TokenResponse = serde_json::from_str(v).unwrap();
        assert_eq!(t.access_token, "x");
        assert_eq!(t.expires_in, 3599);
    }
}
