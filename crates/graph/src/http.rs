//! Live HTTP [`Transport`] backed by reqwest + rustls (feature `http`).
//!
//! Adds the bearer token, parses `Retry-After`, and maps transport errors to a
//! retryable status so the [`crate::run_delta`] orchestration handles them
//! uniformly. The pure orchestration is tested with a mock transport; this is the
//! thin real-network adapter, exercised by the env-gated live test below.

use crate::client::{Response, Transport};
use std::time::Duration;

/// A Microsoft Graph HTTP client carrying a bearer access token.
pub struct GraphClient {
    client: reqwest::blocking::Client,
    token: String,
}

impl GraphClient {
    pub fn new(access_token: impl Into<String>) -> Self {
        GraphClient {
            client: reqwest::blocking::Client::new(),
            token: access_token.into(),
        }
    }

    /// Build with a custom reqwest client (timeouts, proxy, …).
    pub fn with_client(client: reqwest::blocking::Client, access_token: impl Into<String>) -> Self {
        GraphClient {
            client,
            token: access_token.into(),
        }
    }
}

fn parse_retry_after(resp: &reqwest::blocking::Response) -> Option<Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

impl Transport for GraphClient {
    fn get(&mut self, url: &str) -> Response {
        match self.client.get(url).bearer_auth(&self.token).send() {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let retry_after = parse_retry_after(&resp);
                let body = resp.json::<serde_json::Value>().ok();
                Response {
                    status,
                    retry_after,
                    body,
                }
            }
            // Network/transport failure: surface as a retryable 503 so the
            // delta loop's retry budget applies.
            Err(e) => Response {
                status: e.status().map(|s| s.as_u16()).unwrap_or(503),
                retry_after: None,
                body: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_delta;

    /// Live OneDrive delta against the test account. Skips unless
    /// `ISYNCYOU_TEST_TOKEN` (a Files.Read bearer token for the throwaway
    /// account) is set, so CI without credentials passes.
    #[test]
    fn live_onedrive_delta() {
        let token = match std::env::var("ISYNCYOU_TEST_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!("skipping live_onedrive_delta: ISYNCYOU_TEST_TOKEN not set");
                return;
            }
        };
        let mut client = GraphClient::new(token);
        let out = run_delta(
            &mut client,
            "https://graph.microsoft.com/v1.0/me/drive/root/delta",
            None,
            5,
        )
        .expect("live delta walk should succeed");
        assert!(!out.cursor.as_str().is_empty(), "expected a delta cursor");
        eprintln!(
            "live delta: {} items, cursor {} chars",
            out.items.len(),
            out.cursor.as_str().len()
        );
    }
}
