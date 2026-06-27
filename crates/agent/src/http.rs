//! The agent's own blocking HTTP transport — **not** `GraphClient` (which is
//! Graph-specific). The retry/backoff *classification* is pure and unit-tested without
//! the network; the actual `reqwest` call is behind the `http` feature.

/// What to do with a provider HTTP response, decided from its status code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpAction {
    /// 2xx — use the body.
    Ok,
    /// Transient — retry after `after_secs` (honouring `Retry-After` when present).
    Retry { after_secs: Option<u64> },
    /// 401 — refresh the credential and retry once.
    RefreshAuth,
    /// Non-retryable.
    Fatal,
}

/// Pure classifier: map an HTTP status (+ optional `Retry-After`) to an [`HttpAction`].
pub fn classify(status: u16, retry_after: Option<u64>) -> HttpAction {
    match status {
        200..=299 => HttpAction::Ok,
        401 => HttpAction::RefreshAuth,
        408 | 429 => HttpAction::Retry {
            after_secs: retry_after,
        },
        500..=599 => HttpAction::Retry {
            after_secs: retry_after,
        },
        _ => HttpAction::Fatal,
    }
}

/// Pure backoff: `Retry-After` wins; otherwise exponential (1,2,4,…,64 s, capped).
pub fn backoff_secs(attempt: u32, retry_after: Option<u64>) -> u64 {
    if let Some(s) = retry_after {
        return s;
    }
    1u64 << attempt.min(6)
}

#[cfg(feature = "http")]
mod live {
    use crate::AgentError;

    /// A minimal blocking JSON-over-HTTPS client (reqwest + rustls). Providers build
    /// their own request bodies and headers; this just sends and returns status+body.
    pub struct HttpTransport {
        client: reqwest::blocking::Client,
    }

    impl HttpTransport {
        pub fn new() -> Result<Self, AgentError> {
            let client = reqwest::blocking::Client::builder()
                .build()
                .map_err(|e| AgentError::Transport(e.to_string()))?;
            Ok(Self { client })
        }

        /// POST a JSON body with the given headers; returns `(status, body_text)`.
        pub fn post_json(
            &self,
            url: &str,
            headers: &[(String, String)],
            body: &serde_json::Value,
        ) -> Result<(u16, String), AgentError> {
            let mut req = self.client.post(url).json(body);
            for (k, v) in headers {
                req = req.header(k.as_str(), v.as_str());
            }
            let resp = req
                .send()
                .map_err(|e| AgentError::Transport(e.to_string()))?;
            let status = resp.status().as_u16();
            let text = resp
                .text()
                .map_err(|e| AgentError::Transport(e.to_string()))?;
            Ok((status, text))
        }
    }
}

#[cfg(feature = "http")]
pub use live::HttpTransport;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_status_to_action() {
        assert_eq!(classify(200, None), HttpAction::Ok);
        assert_eq!(classify(204, None), HttpAction::Ok);
        assert_eq!(classify(401, None), HttpAction::RefreshAuth);
        assert_eq!(
            classify(429, Some(7)),
            HttpAction::Retry {
                after_secs: Some(7)
            }
        );
        assert_eq!(classify(408, None), HttpAction::Retry { after_secs: None });
        assert_eq!(classify(503, None), HttpAction::Retry { after_secs: None });
        assert_eq!(classify(400, None), HttpAction::Fatal);
        assert_eq!(classify(404, None), HttpAction::Fatal);
    }

    #[test]
    fn backoff_honours_retry_after_then_exponential() {
        assert_eq!(backoff_secs(0, Some(30)), 30); // Retry-After wins
        assert_eq!(backoff_secs(0, None), 1);
        assert_eq!(backoff_secs(1, None), 2);
        assert_eq!(backoff_secs(3, None), 8);
        assert_eq!(backoff_secs(10, None), 64); // capped
    }
}
