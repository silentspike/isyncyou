//! The agent's own blocking HTTP transport — **not** `GraphClient` (which is
//! Graph-specific). The retry/backoff *classification* is pure and unit-tested without
//! the network; the actual `reqwest` call is behind the `http` feature.

#[cfg(any(feature = "http", test))]
use crate::AgentError;
#[cfg(any(feature = "http", test))]
use std::collections::BTreeMap;
#[cfg(any(feature = "http", test))]
use std::io::Read;

#[cfg(any(feature = "http", test))]
const MAX_SSE_LINE_BYTES: usize = 64 * 1024;
#[cfg(any(feature = "http", test))]
const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;
#[cfg(any(feature = "http", test))]
const MAX_PROVIDER_BODY_PREVIEW_BYTES: usize = 4096;

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

/// A provider HTTP response summary. Streaming success bodies are not retained; error
/// responses keep only a short, redacted preview for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(any(feature = "http", test))]
pub struct ProviderHttpResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body_preview: Option<String>,
}

/// One parsed Server-Sent Event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(any(feature = "http", test))]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Debug)]
#[cfg(any(feature = "http", test))]
struct SseDecoder {
    pending: Vec<u8>,
    event: Option<String>,
    data: String,
    max_line_bytes: usize,
    max_event_bytes: usize,
}

#[cfg(any(feature = "http", test))]
impl Default for SseDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(feature = "http", test))]
impl SseDecoder {
    fn new() -> Self {
        Self::with_limits(MAX_SSE_LINE_BYTES, MAX_SSE_EVENT_BYTES)
    }

    fn with_limits(max_line_bytes: usize, max_event_bytes: usize) -> Self {
        Self {
            pending: Vec::new(),
            event: None,
            data: String::new(),
            max_line_bytes,
            max_event_bytes,
        }
    }

    fn push_bytes(
        &mut self,
        chunk: &[u8],
        on_event: &mut dyn FnMut(SseEvent),
    ) -> Result<(), AgentError> {
        self.pending.extend_from_slice(chunk);
        while let Some(pos) = self.pending.iter().position(|b| *b == b'\n') {
            let mut line = self.pending.drain(..=pos).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.process_line(&line, on_event)?;
        }
        if self.pending.len() > self.max_line_bytes {
            return Err(AgentError::Provider("sse: line exceeded limit".into()));
        }
        Ok(())
    }

    fn finish(&mut self, on_event: &mut dyn FnMut(SseEvent)) -> Result<(), AgentError> {
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.process_line(&line, on_event)?;
        }
        self.dispatch(on_event);
        Ok(())
    }

    fn process_line(
        &mut self,
        line: &[u8],
        on_event: &mut dyn FnMut(SseEvent),
    ) -> Result<(), AgentError> {
        if line.len() > self.max_line_bytes {
            return Err(AgentError::Provider("sse: line exceeded limit".into()));
        }
        let line = std::str::from_utf8(line)
            .map_err(|_| AgentError::Provider("sse: malformed utf-8".into()))?;
        if line.is_empty() {
            self.dispatch(on_event);
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => {
                let extra = value.len() + usize::from(!self.data.is_empty());
                if self.data.len().saturating_add(extra) > self.max_event_bytes {
                    return Err(AgentError::Provider(
                        "sse: event data exceeded limit".into(),
                    ));
                }
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(value);
            }
            _ => {}
        }
        Ok(())
    }

    fn dispatch(&mut self, on_event: &mut dyn FnMut(SseEvent)) {
        if self.event.is_none() && self.data.is_empty() {
            return;
        }
        on_event(SseEvent {
            event: self.event.take(),
            data: std::mem::take(&mut self.data),
        });
    }
}

#[cfg(any(feature = "http", test))]
fn read_sse_stream(
    mut reader: impl Read,
    on_event: &mut dyn FnMut(SseEvent),
) -> Result<(), AgentError> {
    let mut decoder = SseDecoder::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| AgentError::Transport(e.to_string()))?;
        if n == 0 {
            break;
        }
        decoder.push_bytes(&buf[..n], on_event)?;
    }
    decoder.finish(on_event)
}

#[cfg(any(feature = "http", test))]
fn provider_header_allowed(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "retry-after"
        || lower == "request-id"
        || lower == "x-request-id"
        || lower == "openai-processing-ms"
        || lower.starts_with("x-ratelimit-")
        || lower.starts_with("anthropic-ratelimit-")
}

#[cfg(any(feature = "http", test))]
fn filter_provider_header_pairs<'a>(
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (name, value) in headers {
        if provider_header_allowed(name) {
            out.insert(name.to_ascii_lowercase(), value.to_string());
        }
    }
    out
}

#[cfg(any(feature = "http", test))]
fn redact_provider_preview(text: &str) -> String {
    text.lines()
        .map(|line| {
            let lower = line.to_ascii_lowercase();
            if lower.contains("authorization:")
                || lower.contains("x-api-key")
                || lower.contains("api-key")
            {
                "[redacted provider credential line]"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(any(feature = "http", test))]
fn body_preview_from_bytes(bytes: &[u8], truncated: bool) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    let mut preview = redact_provider_preview(&String::from_utf8_lossy(bytes));
    if truncated {
        preview.push_str("\n[truncated]");
    }
    Some(preview)
}

#[cfg(any(feature = "http", test))]
fn read_body_preview(mut reader: impl Read) -> Result<Option<String>, AgentError> {
    let mut buf = Vec::with_capacity(MAX_PROVIDER_BODY_PREVIEW_BYTES);
    let mut chunk = [0u8; 1024];
    let mut truncated = false;
    while buf.len() <= MAX_PROVIDER_BODY_PREVIEW_BYTES {
        let n = reader
            .read(&mut chunk)
            .map_err(|e| AgentError::Transport(e.to_string()))?;
        if n == 0 {
            return Ok(body_preview_from_bytes(&buf, truncated));
        }
        let remaining = MAX_PROVIDER_BODY_PREVIEW_BYTES.saturating_sub(buf.len());
        if n > remaining {
            buf.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(body_preview_from_bytes(&buf, truncated))
}

#[cfg(feature = "http")]
mod live {
    use super::{
        filter_provider_header_pairs, read_body_preview, read_sse_stream, ProviderHttpResponse,
        SseEvent,
    };
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
            .resolve(
                "cloudflare-dns.com",
                SocketAddr::new(IpAddr::from([104, 16, 248, 249]), 443),
            )
            .resolve(
                "cloudflare-dns.com",
                SocketAddr::new(IpAddr::from([104, 16, 249, 249]), 443),
            )
            .build()
            .map_err(|e| e.to_string())?;
        let text = client
            .get(format!(
                "https://cloudflare-dns.com/dns-query?name={host}&type=A"
            ))
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

        /// POST a JSON body and parse the response as Server-Sent Events. Successful
        /// response bodies are streamed to `on_event` and not retained.
        pub fn post_json_sse(
            &self,
            url: &str,
            headers: &[(String, String)],
            body: &serde_json::Value,
            on_event: &mut dyn FnMut(SseEvent),
        ) -> Result<ProviderHttpResponse, AgentError> {
            let mut req = self.client.post(url).json(body);
            for (k, v) in headers {
                req = req.header(k.as_str(), v.as_str());
            }
            let mut resp = req
                .send()
                .map_err(|e| AgentError::Transport(e.to_string()))?;
            let status = resp.status().as_u16();
            let headers = filter_provider_header_pairs(
                resp.headers()
                    .iter()
                    .filter_map(|(k, v)| v.to_str().ok().map(|value| (k.as_str(), value))),
            );
            if !(200..=299).contains(&status) {
                let body_preview = read_body_preview(&mut resp)?;
                return Ok(ProviderHttpResponse {
                    status,
                    headers,
                    body_preview,
                });
            }
            read_sse_stream(&mut resp, on_event)?;
            Ok(ProviderHttpResponse {
                status,
                headers,
                body_preview: None,
            })
        }

        /// POST an `application/x-www-form-urlencoded` body (the OpenAI/ChatGPT OAuth token
        /// endpoint wants form encoding, not JSON); returns `(status, body_text)`.
        pub fn post_form(
            &self,
            url: &str,
            form: &[(&str, &str)],
        ) -> Result<(u16, String), AgentError> {
            let resp = self.client.post(url).form(form).send().map_err(|e| {
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
    use std::cell::Cell;
    use std::rc::Rc;

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

    #[test]
    fn sse_parser_handles_event_data_comments_and_blank_lines() {
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        decoder
            .push_bytes(
                b": keepalive\nevent: delta\ndata: hello\ndata: world\n\n\
                  data: [DONE]\n\n",
                &mut |ev| events.push(ev),
            )
            .unwrap();
        decoder.finish(&mut |ev| events.push(ev)).unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("delta"));
        assert_eq!(events[0].data, "hello\nworld");
        assert_eq!(events[1].event, None);
        assert_eq!(events[1].data, "[DONE]");
    }

    #[test]
    fn sse_parser_rejects_oversized_event() {
        let mut decoder = SseDecoder::with_limits(128, 5);
        let err = decoder
            .push_bytes(b"data: 123456\n\n", &mut |_| {})
            .unwrap_err();
        assert!(err.to_string().contains("event data exceeded limit"));
    }

    #[test]
    fn sse_parser_rejects_malformed_utf8() {
        let mut decoder = SseDecoder::new();
        let err = decoder
            .push_bytes(
                &[b'd', b'a', b't', b'a', b':', b' ', 0xff, b'\n'],
                &mut |_| {},
            )
            .unwrap_err();
        assert!(err.to_string().contains("malformed utf-8"));
    }

    #[test]
    fn http_transport_exposes_allowlisted_provider_headers() {
        let got = filter_provider_header_pairs([
            ("Authorization", "Bearer secret"),
            ("content-type", "text/event-stream"),
            ("x-request-id", "req-123"),
            ("retry-after", "7"),
            ("x-ratelimit-remaining-requests", "42"),
            ("anthropic-ratelimit-tokens-remaining", "99"),
            ("set-cookie", "sid=secret"),
        ]);

        assert_eq!(got.get("x-request-id").map(String::as_str), Some("req-123"));
        assert_eq!(got.get("retry-after").map(String::as_str), Some("7"));
        assert_eq!(
            got.get("x-ratelimit-remaining-requests")
                .map(String::as_str),
            Some("42")
        );
        assert_eq!(
            got.get("anthropic-ratelimit-tokens-remaining")
                .map(String::as_str),
            Some("99")
        );
        assert!(!got.contains_key("authorization"));
        assert!(!got.contains_key("set-cookie"));
        assert!(!got.contains_key("content-type"));
    }

    #[test]
    fn provider_http_error_redacts_authorization() {
        let preview = body_preview_from_bytes(
            b"Authorization: Bearer provider-secret\n{\"error\":\"bad\"}",
            false,
        )
        .unwrap();

        assert!(!preview.contains("provider-secret"));
        assert!(preview.contains("[redacted provider credential line]"));
        assert!(preview.contains("\"error\":\"bad\""));
    }

    #[test]
    fn provider_body_preview_is_bounded_and_marked_truncated() {
        let bytes = vec![b'a'; MAX_PROVIDER_BODY_PREVIEW_BYTES + 10];
        let mut reader = std::io::Cursor::new(bytes);
        let preview = read_body_preview(&mut reader).unwrap().unwrap();

        assert!(preview.len() <= MAX_PROVIDER_BODY_PREVIEW_BYTES + "\n[truncated]".len());
        assert!(preview.ends_with("[truncated]"));
    }

    struct ChunkReader {
        chunks: Vec<&'static [u8]>,
        pos: usize,
        reads: Rc<Cell<usize>>,
    }

    impl std::io::Read for ChunkReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.chunks.len() {
                return Ok(0);
            }
            let chunk = self.chunks[self.pos];
            self.pos += 1;
            self.reads.set(self.pos);
            buf[..chunk.len()].copy_from_slice(chunk);
            Ok(chunk.len())
        }
    }

    #[test]
    fn provider_stream_does_not_buffer_until_done() {
        let reads = Rc::new(Cell::new(0));
        let mut reader = ChunkReader {
            chunks: vec![b"data: first\n\n", b"data: second\n\n"],
            pos: 0,
            reads: Rc::clone(&reads),
        };
        let mut emitted = Vec::new();

        read_sse_stream(&mut reader, &mut |ev| {
            emitted.push((reads.get(), ev.data));
        })
        .unwrap();

        assert_eq!(emitted[0], (1, "first".to_string()));
        assert_eq!(emitted[1], (2, "second".to_string()));
    }
}
