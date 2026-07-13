//! The agent's own blocking HTTP transport — **not** `GraphClient` (which is
//! Graph-specific). The retry/backoff *classification* is pure and unit-tested without
//! the network; the actual `reqwest` call is behind the `http` feature.

#[cfg(any(feature = "http", test))]
use crate::AgentError;
#[cfg(any(feature = "http", test))]
use std::collections::BTreeMap;
#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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
        filter_provider_header_pairs, ProviderHttpResponse, SseDecoder, SseEvent,
        MAX_PROVIDER_BODY_PREVIEW_BYTES,
    };
    use crate::connectivity::{ProbeObservation, ProbeTarget};
    use crate::AgentError;
    use futures_util::StreamExt;
    use std::sync::{Arc, OnceLock};
    use std::time::Duration;

    /// Compiled policy for product provider traffic. Callers cannot override these values.
    pub const PROVIDER_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    pub const PROVIDER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);
    pub const PROVIDER_SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
    pub const PROVIDER_TURN_TIMEOUT: Duration = Duration::from_secs(20 * 60);
    pub const PREFLIGHT_TOTAL_TIMEOUT: Duration = Duration::from_secs(12);

    /// A minimal blocking JSON-over-HTTPS client (reqwest + rustls). Providers build
    /// their own request bodies and headers; this just sends and returns status+body.
    pub struct HttpTransport {
        client: reqwest::blocking::Client,
        probe_client: reqwest::blocking::Client,
        sse_client: reqwest::Client,
        sse_runtime: tokio::runtime::Runtime,
    }

    impl HttpTransport {
        pub fn new() -> Result<Self, AgentError> {
            let client = reqwest::blocking::Client::builder()
                .connect_timeout(PROVIDER_CONNECT_TIMEOUT)
                .timeout(PROVIDER_TURN_TIMEOUT)
                .build()
                .map_err(|_| {
                    AgentError::Transport("provider_transport_initialization_failed".into())
                })?;
            let probe_client = reqwest::blocking::Client::builder()
                .connect_timeout(PROVIDER_CONNECT_TIMEOUT)
                .timeout(PREFLIGHT_TOTAL_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| {
                    AgentError::Transport("provider_transport_initialization_failed".into())
                })?;
            let sse_client = async_sse_client_builder().build().map_err(|_| {
                AgentError::Transport("provider_transport_initialization_failed".into())
            })?;
            let sse_runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|_| {
                    AgentError::Transport("provider_transport_initialization_failed".into())
                })?;
            Ok(Self {
                client,
                probe_client,
                sse_client,
                sse_runtime,
            })
        }

        /// Reuse one transport for the lifetime of the app process. Provider requests
        /// remain logically independent, while DNS, TLS sessions, connection pools, and
        /// the async runtime stay warm between turns and credential refreshes.
        pub fn shared() -> Result<Arc<Self>, AgentError> {
            static SHARED: OnceLock<Arc<HttpTransport>> = OnceLock::new();
            if let Some(transport) = SHARED.get() {
                return Ok(Arc::clone(transport));
            }
            let transport = Arc::new(Self::new()?);
            Ok(Arc::clone(SHARED.get_or_init(|| transport)))
        }

        fn ensure_test_network_allowed() -> Result<(), AgentError> {
            #[cfg(test)]
            if std::env::var_os("ISYNCYOU_AGENT_TEST_BLOCK_NETWORK").is_some() {
                return Err(AgentError::Transport(
                    "unexpected provider network access blocked by test guard".into(),
                ));
            }
            Ok(())
        }

        /// Run one unauthenticated, no-redirect provider reachability probe. The target
        /// table is closed and neither the URL nor raw reqwest error leaves this module.
        pub fn probe(&self, target: ProbeTarget) -> Result<ProbeObservation, AgentError> {
            Self::ensure_test_network_allowed()?;
            let mut request = self.probe_client.get(probe_target_url(target));
            for (name, value) in probe_request_headers() {
                request = request.header(name, value);
            }
            let result = request.send();
            match result {
                Ok(response) => Ok(ProbeObservation::HttpStatus(response.status().as_u16())),
                Err(error) => Ok(probe_observation_for_reqwest_error(&error)),
            }
        }

        /// POST a JSON body with the given headers; returns `(status, body_text)`.
        pub fn post_json(
            &self,
            url: &str,
            headers: &[(String, String)],
            body: &serde_json::Value,
        ) -> Result<(u16, String), AgentError> {
            Self::ensure_test_network_allowed()?;
            let mut req = self
                .client
                .post(url)
                .timeout(PROVIDER_RESPONSE_TIMEOUT)
                .json(body);
            for (k, v) in headers {
                req = req.header(k.as_str(), v.as_str());
            }
            let resp = req.send().map_err(safe_reqwest_transport_error)?;
            let status = resp.status().as_u16();
            let text = resp
                .text()
                .map_err(|_| AgentError::Transport("provider_response_read_failed".into()))?;
            Ok((status, text))
        }

        /// POST a JSON body and parse the response as Server-Sent Events. Successful
        /// response bodies are streamed to `on_event` and not retained.
        pub fn post_json_sse(
            &self,
            url: &str,
            headers: &[(String, String)],
            body: &serde_json::Value,
            on_event: &mut dyn FnMut(SseEvent) -> bool,
        ) -> Result<ProviderHttpResponse, AgentError> {
            Self::ensure_test_network_allowed()?;
            let mut req = self.sse_client.post(url).json(body);
            for (k, v) in headers {
                req = req.header(k.as_str(), v.as_str());
            }
            self.sse_runtime.block_on(async {
                let started_at = tokio::time::Instant::now();
                let deadline = started_at + PROVIDER_TURN_TIMEOUT;
                let first_event_deadline = started_at + PROVIDER_RESPONSE_TIMEOUT;
                let response =
                    within_deadline(first_event_deadline, PROVIDER_RESPONSE_TIMEOUT, req.send())
                        .await
                        .map_err(|_| provider_timeout("provider_response_timed_out"))?
                        .map_err(safe_reqwest_transport_error)?;
                let status = response.status().as_u16();
                let headers = filter_provider_header_pairs(
                    response
                        .headers()
                        .iter()
                        .filter_map(|(k, v)| v.to_str().ok().map(|value| (k.as_str(), value))),
                );
                if !(200..=299).contains(&status) {
                    let body_preview = read_body_preview_async(response, deadline).await?;
                    return Ok(ProviderHttpResponse {
                        status,
                        headers,
                        body_preview,
                    });
                }
                let mut stream = response.bytes_stream();
                let mut decoder = SseDecoder::new();
                let mut saw_any_event = false;
                let mut saw_progress = false;
                let mut event_deadline = first_event_deadline;
                loop {
                    let phase_deadline = deadline.min(event_deadline);
                    let (phase_timeout, timeout_code) = provider_stream_wait_policy(saw_progress);
                    let next = within_deadline(phase_deadline, phase_timeout, stream.next())
                        .await
                        .map_err(|_| provider_timeout(timeout_code))?;
                    let Some(chunk) = next else {
                        break;
                    };
                    let chunk = chunk
                        .map_err(|_| AgentError::Transport("provider_stream_read_failed".into()))?;
                    let mut chunk_had_progress = false;
                    decoder.push_bytes(&chunk, &mut |event| {
                        saw_any_event = true;
                        chunk_had_progress |= on_event(event);
                    })?;
                    if chunk_had_progress {
                        saw_progress = true;
                        event_deadline = tokio::time::Instant::now() + PROVIDER_SSE_IDLE_TIMEOUT;
                    }
                }
                decoder.finish(&mut |event| {
                    saw_any_event = true;
                    let _ = on_event(event);
                })?;
                if !saw_any_event {
                    return Err(AgentError::Transport(
                        "provider_stream_ended_without_event".into(),
                    ));
                }
                Ok(ProviderHttpResponse {
                    status,
                    headers,
                    body_preview: None,
                })
            })
        }

        /// POST an `application/x-www-form-urlencoded` body (the OpenAI/ChatGPT OAuth token
        /// endpoint wants form encoding, not JSON); returns `(status, body_text)`.
        pub fn post_form(
            &self,
            url: &str,
            form: &[(&str, &str)],
        ) -> Result<(u16, String), AgentError> {
            Self::ensure_test_network_allowed()?;
            let resp = self
                .client
                .post(url)
                .timeout(PROVIDER_RESPONSE_TIMEOUT)
                .form(form)
                .send()
                .map_err(safe_reqwest_transport_error)?;
            let status = resp.status().as_u16();
            let text = resp
                .text()
                .map_err(|_| AgentError::Transport("provider_response_read_failed".into()))?;
            Ok((status, text))
        }
    }

    fn probe_target_url(target: ProbeTarget) -> &'static str {
        match target {
            ProbeTarget::ClaudeOAuth => "https://claude.com/cai/oauth/authorize",
            ProbeTarget::ClaudeInference => "https://api.anthropic.com/v1/messages",
            ProbeTarget::CodexOAuth => "https://auth.openai.com/",
            ProbeTarget::CodexInference => "https://chatgpt.com/backend-api/codex/responses",
        }
    }

    pub(super) fn probe_request_headers() -> [(&'static str, &'static str); 1] {
        [("accept", "application/json")]
    }

    fn async_sse_client_builder() -> reqwest::ClientBuilder {
        reqwest::Client::builder().connect_timeout(PROVIDER_CONNECT_TIMEOUT)
    }

    fn error_source_is_tls(error: &(dyn std::error::Error + 'static)) -> bool {
        if error.is::<rustls::Error>() {
            return true;
        }
        if let Some(io) = error.downcast_ref::<std::io::Error>() {
            if io
                .get_ref()
                .is_some_and(|inner| inner.is::<rustls::Error>())
            {
                return true;
            }
        }
        error.source().is_some_and(error_source_is_tls)
    }

    fn error_source_is_dns(error: &(dyn std::error::Error + 'static)) -> bool {
        if let Some(io) = error.downcast_ref::<std::io::Error>() {
            if matches!(
                io.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::AddrNotAvailable
            ) {
                return true;
            }
        }
        error.source().is_some_and(error_source_is_dns)
    }

    pub(super) fn probe_observation_for_error_source(
        timed_out: bool,
        error: &(dyn std::error::Error + 'static),
    ) -> ProbeObservation {
        if timed_out {
            ProbeObservation::ConnectTimedOut
        } else if error_source_is_tls(error) {
            ProbeObservation::TlsFailed
        } else if error_source_is_dns(error) {
            ProbeObservation::NameResolutionFailed
        } else {
            ProbeObservation::ConnectFailed
        }
    }

    fn probe_observation_for_reqwest_error(error: &reqwest::Error) -> ProbeObservation {
        probe_observation_for_error_source(error.is_timeout(), error)
    }

    fn safe_reqwest_transport_error(error: reqwest::Error) -> AgentError {
        let code = match probe_observation_for_reqwest_error(&error) {
            ProbeObservation::ConnectTimedOut => "provider_connect_timed_out",
            ProbeObservation::TlsFailed => "provider_tls_failed",
            ProbeObservation::NameResolutionFailed => "provider_name_resolution_failed",
            _ => "provider_connect_failed",
        };
        AgentError::Transport(code.into())
    }

    pub(super) async fn within_deadline<T>(
        deadline: tokio::time::Instant,
        limit: Duration,
        future: impl std::future::Future<Output = T>,
    ) -> Result<T, ()> {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(());
        }
        let remaining = deadline.duration_since(now);
        let timeout = remaining.min(limit);
        tokio::time::timeout(timeout, future).await.map_err(|_| ())
    }

    fn provider_timeout(code: &'static str) -> AgentError {
        AgentError::Transport(code.into())
    }

    pub(super) fn provider_stream_wait_policy(saw_event: bool) -> (Duration, &'static str) {
        if saw_event {
            (PROVIDER_SSE_IDLE_TIMEOUT, "provider_stream_idle_timed_out")
        } else {
            (PROVIDER_RESPONSE_TIMEOUT, "provider_response_timed_out")
        }
    }

    async fn read_body_preview_async(
        response: reqwest::Response,
        deadline: tokio::time::Instant,
    ) -> Result<Option<String>, AgentError> {
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::with_capacity(MAX_PROVIDER_BODY_PREVIEW_BYTES);
        let mut truncated = false;
        loop {
            let next = within_deadline(deadline, PROVIDER_SSE_IDLE_TIMEOUT, stream.next())
                .await
                .map_err(|_| provider_timeout("provider_stream_idle_timed_out"))?;
            let Some(chunk) = next else {
                break;
            };
            let chunk =
                chunk.map_err(|_| AgentError::Transport("provider_stream_read_failed".into()))?;
            let remaining = MAX_PROVIDER_BODY_PREVIEW_BYTES.saturating_sub(bytes.len());
            if chunk.len() > remaining {
                bytes.extend_from_slice(&chunk[..remaining]);
                truncated = true;
                break;
            }
            bytes.extend_from_slice(&chunk);
            if bytes.len() == MAX_PROVIDER_BODY_PREVIEW_BYTES {
                truncated = true;
                break;
            }
        }
        Ok(super::body_preview_from_bytes(&bytes, truncated))
    }
}

#[cfg(feature = "http")]
pub use live::HttpTransport;

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[cfg(feature = "http")]
    static HTTP_TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(feature = "http")]
    struct EnvRestore {
        key: &'static str,
        value: Option<std::ffi::OsString>,
    }

    #[cfg(feature = "http")]
    impl EnvRestore {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, value: old }
        }
    }

    #[cfg(feature = "http")]
    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match &self.value {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

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
    fn provider_sse_keepalive_comments_do_not_count_as_events() {
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        decoder
            .push_bytes(b": keepalive\n\n: still-alive\n\n", &mut |event| {
                events.push(event)
            })
            .unwrap();
        decoder.finish(&mut |event| events.push(event)).unwrap();
        assert!(events.is_empty());
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

    #[cfg(feature = "http")]
    #[test]
    fn provider_transport_timeout_policy_has_distinct_response_idle_and_turn_bounds() {
        assert_eq!(live::PROVIDER_CONNECT_TIMEOUT.as_secs(), 10);
        assert_eq!(live::PROVIDER_RESPONSE_TIMEOUT.as_secs(), 60);
        assert_eq!(live::PROVIDER_SSE_IDLE_TIMEOUT.as_secs(), 120);
        assert_eq!(live::PROVIDER_TURN_TIMEOUT.as_secs(), 20 * 60);
        assert!(live::PROVIDER_RESPONSE_TIMEOUT < live::PROVIDER_SSE_IDLE_TIMEOUT);
        assert!(live::PROVIDER_SSE_IDLE_TIMEOUT < live::PROVIDER_TURN_TIMEOUT);
    }

    #[cfg(feature = "http")]
    #[test]
    fn provider_transport_is_hot_for_process_lifetime() {
        let first = HttpTransport::shared().unwrap();
        let second = HttpTransport::shared().unwrap();
        assert!(std::sync::Arc::ptr_eq(&first, &second));
    }

    #[cfg(feature = "http")]
    #[test]
    fn provider_transport_hot_runtime_drives_local_sse_io() {
        use std::io::{Read, Write};

        let _guard = HTTP_TEST_ENV_LOCK.lock().unwrap();
        assert!(std::env::var_os("ISYNCYOU_AGENT_TEST_BLOCK_NETWORK").is_none());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                      connection: close\r\n\r\ndata: {\"type\":\"response.completed\"}\n\n",
                )
                .unwrap();
        });

        let transport = HttpTransport::shared().unwrap();
        let mut events = Vec::new();
        let response = transport
            .post_json_sse(
                &format!("http://{address}/responses"),
                &[],
                &serde_json::json!({}),
                &mut |event| {
                    events.push(event.data);
                    true
                },
            )
            .unwrap();
        server.join().unwrap();

        assert_eq!(response.status, 200);
        assert_eq!(events, [r#"{"type":"response.completed"}"#]);
    }

    #[cfg(feature = "http")]
    #[test]
    fn provider_stream_first_event_uses_response_deadline_not_idle_deadline() {
        assert_eq!(
            live::provider_stream_wait_policy(false),
            (
                live::PROVIDER_RESPONSE_TIMEOUT,
                "provider_response_timed_out"
            )
        );
        assert_eq!(
            live::provider_stream_wait_policy(true),
            (
                live::PROVIDER_SSE_IDLE_TIMEOUT,
                "provider_stream_idle_timed_out"
            )
        );
    }

    #[cfg(feature = "http")]
    #[test]
    fn provider_transport_expired_deadline_rejects_already_ready_chunk() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let expired = tokio::time::Instant::now();
            let result = live::within_deadline(
                expired,
                std::time::Duration::from_secs(120),
                std::future::ready(42),
            )
            .await;
            assert_eq!(result, Err(()));
        });
    }

    #[cfg(feature = "http")]
    #[test]
    fn preflight_transport_never_sends_authorization_or_cookie() {
        for (name, _) in live::probe_request_headers() {
            let name = name.to_ascii_lowercase();
            assert_ne!(name, "authorization");
            assert_ne!(name, "cookie");
        }
    }

    #[cfg(feature = "http")]
    #[test]
    fn connectivity_preflight_classifies_name_resolution_failure_without_error_text() {
        let error = std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "private resolver details must not be parsed",
        );
        assert_eq!(
            live::probe_observation_for_error_source(false, &error),
            crate::connectivity::ProbeObservation::NameResolutionFailed
        );
    }

    #[cfg(feature = "http")]
    #[test]
    fn connectivity_preflight_classifies_tls_failure_without_error_text() {
        let error = rustls::Error::General("private certificate details".into());
        assert_eq!(
            live::probe_observation_for_error_source(false, &error),
            crate::connectivity::ProbeObservation::TlsFailed
        );
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

    #[cfg(feature = "http")]
    #[test]
    fn non_live_provider_tests_fail_on_unexpected_network() {
        let _guard = HTTP_TEST_ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::set("ISYNCYOU_AGENT_TEST_BLOCK_NETWORK", "1");
        let http = HttpTransport::new().unwrap();
        let err = http
            .post_json(
                "https://example.invalid/provider",
                &[],
                &serde_json::json!({}),
            )
            .expect_err("network guard must fail before opening a request");
        assert!(err
            .to_string()
            .contains("unexpected provider network access"));
    }
}
