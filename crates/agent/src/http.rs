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

    /// Resolve `host`'s A records via **DNS-over-HTTPS to `1.1.1.1`**. Android forbids apps
    /// from doing raw DNS (UDP → `EPERM`) and its platform resolver intermittently `EAI_NODATA`s
    /// for Cloudflare-fronted hosts (`auth.openai.com`) from app threads; a plain HTTPS call to
    /// the literal IP `1.1.1.1` (no DNS needed, cert valid for `1.1.1.1`) works within those
    /// rules. Returns the resolved IPs (empty on any failure → caller falls back to platform DNS).
    pub fn doh_resolve(host: &str) -> Result<Vec<std::net::IpAddr>, String> {
        use std::error::Error;
        use std::net::{IpAddr, SocketAddr};
        // Google's public DoH, reached by PINNING `dns.google` to its stable well-known IPs
        // (8.8.8.8 / 8.8.4.4). This needs neither the platform resolver (which app-side can't
        // look up AAAA-bearing hosts like auth.openai.com / dns.google) nor raw UDP DNS
        // (EPERM on Android). SNI/Host stay `dns.google`, so the cert validates normally.
        // Some networks block app access to public DNS *server* IPs (8.8.8.8 / 1.1.1.1),
        // but not Cloudflare's general web range — so reach Cloudflare's DoH by pinning
        // `cloudflare-dns.com` to its web IPs (reachable wherever the site itself is).
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .resolve("cloudflare-dns.com", SocketAddr::new(IpAddr::from([104, 16, 248, 249]), 443))
            .resolve("cloudflare-dns.com", SocketAddr::new(IpAddr::from([104, 16, 249, 249]), 443))
            .build()
            .map_err(|e| e.to_string())?;
        let text = client
            .get(format!("https://cloudflare-dns.com/dns-query?name={host}&type=A"))
            .header("accept", "application/dns-json")
            .send()
            .map_err(|e| {
                let mut m = e.to_string();
                let mut s = e.source();
                while let Some(x) = s {
                    m.push_str(&format!(" | {x}"));
                    s = x.source();
                }
                m
            })?
            .text()
            .map_err(|e| e.to_string())?;
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        Ok(v.get("Answer")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    // type 1 = A record
                    .filter(|e| e.get("type").and_then(|t| t.as_u64()) == Some(1))
                    .filter_map(|e| {
                        e.get("data")
                            .and_then(|d| d.as_str())
                            .and_then(|s| s.parse().ok())
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

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

        /// Like [`new`], but pins `host` to the given IPs (from [`doh_resolve`]) so the
        /// platform resolver is bypassed. The TLS SNI/Host stays `host`, so the cert still
        /// validates. Use for hosts Android's app-thread resolver can't look up
        /// (`auth.openai.com`). Empty `ips` → behaves like [`new`] (platform resolver).
        pub fn new_resolving(host: &str, ips: &[std::net::IpAddr]) -> Result<Self, AgentError> {
            use std::net::SocketAddr;
            use std::time::Duration;
            let addrs: Vec<SocketAddr> = ips
                .iter()
                .copied()
                .filter(|ip| ip.is_ipv4())
                .map(|ip| SocketAddr::new(ip, 443))
                .collect();
            let client = reqwest::blocking::Client::builder()
                .no_proxy()
                .connect_timeout(Duration::from_secs(6))
                .timeout(Duration::from_secs(30))
                .resolve_to_addrs(host, &addrs)
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

        /// POST an `application/x-www-form-urlencoded` body (the OpenAI/ChatGPT OAuth token
        /// endpoint wants form encoding, not JSON); returns `(status, body_text)`.
        pub fn post_form(
            &self,
            url: &str,
            form: &[(&str, &str)],
        ) -> Result<(u16, String), AgentError> {
            let resp = self
                .client
                .post(url)
                .form(form)
                .send()
                .map_err(|e| {
                    use std::error::Error;
                    let mut msg = e.to_string();
                    let mut src = e.source();
                    while let Some(s) = src {
                        msg.push_str(&format!(" | {s}"));
                        src = s.source();
                    }
                    AgentError::Transport(msg)
                })?;
            let status = resp.status().as_u16();
            let text = resp
                .text()
                .map_err(|e| AgentError::Transport(e.to_string()))?;
            Ok((status, text))
        }
    }
}

#[cfg(feature = "http")]
pub use live::{doh_resolve, HttpTransport};

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
