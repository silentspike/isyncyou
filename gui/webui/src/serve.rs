//! Minimal localhost HTTP/1.1 adapter for [`Router`] (plan §25).
//!
//! Hand-rolled over `std::net` — no web framework — because the surface is tiny:
//! GET requests plus a few capability-token-guarded POSTs, mostly one response
//! per connection (`Connection: close`).
//! The accept loop is **thread-per-connection** (capped at [`MAX_CONNS`]): a
//! personal localhost UI serves one user, but the SSE `/api/v1/events` stream
//! ([`handle_sse`]) holds its connection open, so normal requests must keep being
//! served concurrently. Each per-request [`Store`] open still holds the
//! single-instance lock only momentarily; the daemon serializes store access via
//! the router gate.
//!
//! Two transports share one [`handle`]: TCP loopback ([`serve`]) for the browser
//! UI, and a **Unix-domain socket** ([`serve_unix`]) for owner-only local access
//! where filesystem permissions (mode 0600) are the access control.

use crate::{ApiRequest, ApiResponse, EventBus, Router};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Hard cap on concurrent connection threads (safety against runaway opens on a
/// loopback-only server). SSE streams count against this, so it is generous.
const MAX_CONNS: usize = 128;
static ACTIVE_CONNS: AtomicUsize = AtomicUsize::new(0);

/// Upper bound on an in-memory request body (#0A). Generous enough for a document
/// upload yet bounded so a bogus `Content-Length` can't make the server allocate without
/// limit; an oversized body is refused (413) rather than buffered.
const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;

/// Decrements the live-connection counter when a connection thread ends.
struct ConnGuard;
impl Drop for ConnGuard {
    fn drop(&mut self) {
        ACTIVE_CONNS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A connection we can both read the request from and write the response to.
/// We need a second read handle (for the `BufReader`) while still writing to the
/// stream, so the trait yields a cloned reader. Implemented for TCP + Unix.
trait Conn: Read + Write {
    fn clone_reader(&self) -> std::io::Result<Box<dyn Read>>;
    /// Bound how long a persistent (keep-alive) connection waits for the next request
    /// before it's closed, so an idle connection releases its slot instead of lingering.
    fn set_read_timeout(&self, dur: Option<Duration>) -> std::io::Result<()>;
}
impl Conn for TcpStream {
    fn clone_reader(&self) -> std::io::Result<Box<dyn Read>> {
        Ok(Box::new(self.try_clone()?))
    }
    fn set_read_timeout(&self, dur: Option<Duration>) -> std::io::Result<()> {
        TcpStream::set_read_timeout(self, dur)
    }
}
#[cfg(unix)]
impl Conn for std::os::unix::net::UnixStream {
    fn clone_reader(&self) -> std::io::Result<Box<dyn Read>> {
        Ok(Box::new(self.try_clone()?))
    }
    fn set_read_timeout(&self, dur: Option<Duration>) -> std::io::Result<()> {
        std::os::unix::net::UnixStream::set_read_timeout(self, dur)
    }
}

/// Which local transport accepted a request. TCP needs Host/Origin checks because
/// any local browser can reach it; Unix sockets rely on filesystem permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccessPolicy {
    TcpLoopback,
    UnixSocket,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct RequestHeaders {
    cap_token: Option<String>,
    session_token: Option<String>,
    cookie: Option<String>,
    host: Option<String>,
    origin: Option<String>,
    body_encoding: Option<String>,
    content_type: Option<String>,
}

const AGENT_STRICT_JSON_MAX_BYTES: usize = 8 * 1024;

fn strict_json_body_limit(method: &str, target: &str) -> Option<usize> {
    if method != "POST" {
        return None;
    }
    let path = target
        .split_once('?')
        .map(|(path, _)| path)
        .unwrap_or(target);
    matches!(
        path,
        "/api/v1/agent/connectivity/preflight"
            | "/api/v1/agent/credential/refresh"
            | "/api/v1/agent/oauth/cancel"
    )
    .then_some(AGENT_STRICT_JSON_MAX_BYTES)
}

/// Decode a request body per the `X-Body-Encoding` header (#657 in-app upload/replace):
/// `base64` → the raw bytes, bounded by `max`; any other/absent value passes the bytes
/// through. Uploads ride base64 over the JSON bridge (which can't carry raw bytes) and,
/// uniformly, over HTTP. Returns `(status, message)` on a 400 (bad base64) / 413 (oversize).
fn decode_body(
    encoding: Option<&str>,
    raw: Vec<u8>,
    max: usize,
) -> Result<Vec<u8>, (u16, &'static str)> {
    match encoding {
        Some(e) if e.eq_ignore_ascii_case("base64") => {
            use base64::Engine as _;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(&raw)
                .map_err(|_| (400u16, "invalid base64 request body"))?;
            if decoded.len() > max {
                return Err((413, "request body too large"));
            }
            Ok(decoded)
        }
        _ => Ok(raw),
    }
}

/// Extract a cookie value by name from a raw `Cookie:` header (`a=1; b=2`).
fn cookie_value(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|pair| {
        let (k, v) = pair.trim().split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

/// Parse an HTTP request line (`"GET /path?x=1 HTTP/1.1"`) into `(method, target)`.
pub fn parse_request_line(line: &str) -> Option<(String, String)> {
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    Some((method, target))
}

/// Build the routed [`ApiRequest`] from a transport-agnostic set of inputs: resolve the
/// effective session token (explicit `X-Session-Token`, else the `isy_session` loopback
/// cookie) and attach the cap-token + body. Shared by the HTTP [`handle`] loop and the
/// in-process [`dispatch_message`] bridge so both transports route **identically** (#0A).
fn build_request(
    method: &str,
    target: &str,
    cap_token: Option<String>,
    session_token: Option<String>,
    cookie: Option<String>,
    content_type: Option<String>,
    body: Vec<u8>,
) -> ApiRequest {
    let session_token = session_token.or_else(|| {
        cookie
            .as_deref()
            .and_then(|c| cookie_value(c, "isy_session"))
    });
    ApiRequest::new(method, target)
        .with_cap_token(cap_token)
        .with_session_token(session_token)
        .with_content_type(content_type)
        .with_body(body)
}

/// Dispatch one request that arrived over the Android in-process `WebMessage` bridge
/// (#0A) — **no TCP port is involved**. Applies the same session-token resolution and
/// routing as the HTTP path; host/origin checks don't apply because the bridge is bound
/// to the app origin natively by `WebMessageListener`'s `allowedOriginRules`. SSE routes
/// (`/api/v1/events`, `/api/v1/agent/stream`) are NOT served here — the bridge carries
/// those streams over its own native push channel. Returns the response for the native
/// side to post back on the message port.
pub struct BridgeDispatchRequest<'a> {
    pub method: &'a str,
    pub target: &'a str,
    pub cap_token: Option<String>,
    pub session_token: Option<String>,
    pub cookie: Option<String>,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

pub fn dispatch_message(router: &Router, request: BridgeDispatchRequest<'_>) -> ApiResponse {
    router.route(&build_request(
        request.method,
        request.target,
        request.cap_token,
        request.session_token,
        request.cookie,
        request.content_type,
        request.body,
    ))
}

/// Handle one JSON-framed unary request from the Android in-process bridge (#0A) and
/// return the **complete reply message** the native side posts back verbatim — the Kotlin
/// side is a truly dumb forwarder, so all parsing and framing live here (host-testable).
/// Request shape: `{"t":"req","id":<str>,"method","path","headers":{..},"body":<str|null>}`.
/// Reply shape: `{"t":"res","id":<str>,"status":<u16>,"body":<string>}` (the response bytes
/// as UTF-8; today's API is JSON/text — binary GET subresources use the asset path). SSE
/// routes are not handled here — the bridge streams them over its own push channel. Header
/// lookup is case-insensitive; `id` is echoed so the JS promise resolves.
pub fn handle_bridge_request(router: &Router, request_json: &str) -> String {
    use serde_json::Value;
    let v: Value = match serde_json::from_str(request_json) {
        Ok(v) => v,
        Err(_) => return bridge_error_envelope(None, 400, "bad bridge request"),
    };
    let id = v.get("id").and_then(Value::as_str);
    let path = match v.get("path").and_then(Value::as_str) {
        Some(p) => p,
        None => return bridge_error_envelope(id, 400, "missing path"),
    };
    let method = v.get("method").and_then(Value::as_str).unwrap_or("GET");
    let headers = v.get("headers").and_then(Value::as_object);
    let header = |name: &str| {
        headers.and_then(|obj| {
            obj.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .and_then(|(_, val)| val.as_str())
                .map(str::to_string)
        })
    };
    let raw = v
        .get("body")
        .and_then(Value::as_str)
        .map(|s| s.as_bytes().to_vec())
        .unwrap_or_default();
    // #657: a binary upload rides base64 over this JSON bridge; decode it (+ size-cap).
    let body = match decode_body(header("X-Body-Encoding").as_deref(), raw, MAX_REQUEST_BODY) {
        Ok(b) => b,
        Err((status, message)) => return bridge_error_envelope(id, status, message),
    };
    let resp = dispatch_message(
        router,
        BridgeDispatchRequest {
            method,
            target: path,
            cap_token: header("X-Capability-Token"),
            session_token: header("X-Session-Token"),
            cookie: header("Cookie"),
            content_type: header("Content-Type"),
            body,
        },
    );
    serde_json::json!({
        "t": "res",
        "id": id,
        "status": resp.status,
        "body": String::from_utf8_lossy(&resp.body),
    })
    .to_string()
}

/// A bridge reply carrying an error, echoing `id` so the JS promise still resolves.
fn bridge_error_envelope(id: Option<&str>, status: u16, message: &str) -> String {
    serde_json::json!({
        "t": "res",
        "id": id,
        "status": status,
        "body": serde_json::json!({ "error": message }).to_string(),
    })
    .to_string()
}

/// Serialize a response as an HTTP/1.1 message with `Connection: close`.
pub fn format_http(resp: &ApiResponse) -> Vec<u8> {
    format_http_conn(resp, false)
}

/// Serialize a response, choosing the `Connection` header. `keep_alive` keeps the
/// socket open for the next request on the same connection (HTTP/1.1 persistent) — the
/// WebView then reuses a handful of connections instead of opening a fresh TCP socket
/// per `fetch()`, which stops a burst of requests from churning/exhausting the browser's
/// small per-origin connection pool. `Content-Length` frames every body, so the client
/// knows where each response ends. Error/handshake paths pass `false` and close.
fn format_http_conn(resp: &ApiResponse, keep_alive: bool) -> Vec<u8> {
    let reason = match resp.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let conn = if keep_alive { "keep-alive" } else { "close" };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\nX-Content-Type-Options: nosniff\r\n",
        resp.status,
        reason,
        resp.content_type,
        resp.body.len(),
        conn
    );
    for (k, v) in &resp.headers {
        // header values are crafted in-process (constants); guard against any
        // accidental CRLF so a value can never inject extra headers.
        let v = v.replace(['\r', '\n'], " ");
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(&resp.body);
    out
}

fn is_loopback_host(host: &str) -> bool {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    matches!(h.as_str(), "localhost" | "127.0.0.1" | "[::1]" | "::1")
        || h.starts_with("localhost:")
        || h.starts_with("127.0.0.1:")
        || h.starts_with("[::1]:")
}

fn bind_host(addr: &str) -> Option<&str> {
    if let Ok(sock) = addr.parse::<SocketAddr>() {
        return Some(match sock {
            SocketAddr::V4(_) => addr.rsplit_once(':')?.0,
            SocketAddr::V6(_) => addr.strip_prefix('[')?.split_once(']')?.0,
        });
    }
    if let Some(rest) = addr.strip_prefix('[') {
        let (host, suffix) = rest.split_once(']')?;
        return suffix.starts_with(':').then_some(host);
    }
    let (host, port) = addr.rsplit_once(':')?;
    if host.contains(':') || host.is_empty() || port.is_empty() {
        return None;
    }
    Some(host)
}

fn is_loopback_bind_addr(addr: &str) -> bool {
    if let Ok(sock) = addr.parse::<SocketAddr>() {
        return sock.ip().is_loopback();
    }
    let Some(host) = bind_host(addr) else {
        return false;
    };
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    matches!(h.as_str(), "localhost" | "127.0.0.1" | "::1")
}

fn origin_host(origin: &str) -> Option<&str> {
    let (_, rest) = origin.trim().split_once("://")?;
    Some(rest.split('/').next().unwrap_or(rest))
}

fn is_local_origin(origin: &str) -> bool {
    origin_host(origin).is_some_and(is_loopback_host)
}

fn forbidden(message: &str) -> ApiResponse {
    ApiResponse {
        status: 403,
        content_type: "application/json".into(),
        body: format!(r#"{{"error":"{message}"}}"#).into_bytes(),
        headers: Vec::new(),
    }
}

fn validate_request_headers(
    policy: AccessPolicy,
    method: &str,
    headers: &RequestHeaders,
) -> Option<ApiResponse> {
    if policy == AccessPolicy::TcpLoopback {
        match headers.host.as_deref() {
            Some(h) if is_loopback_host(h) => {}
            Some(_) => return Some(forbidden("invalid host header")),
            None if method == "GET" || method == "POST" => {
                return Some(forbidden("missing host header"));
            }
            None => {}
        }
    }
    if let Some(origin) = headers.origin.as_deref() {
        if !is_local_origin(origin) {
            return Some(forbidden("invalid origin header"));
        }
    }
    None
}

fn handle<S: Conn>(stream: &mut S, router: &Router, policy: AccessPolicy) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.clone_reader()?);
    // Keep-alive: an idle persistent connection closes after this timeout so it releases
    // its slot instead of pinning one of the WebView's few per-origin connections.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(20)));
    // One iteration per request; the connection is reused (HTTP/1.1 persistent) until the
    // client closes it, it idles out, or a request can't be served cleanly.
    loop {
        let mut request_line = String::new();
        match reader.read_line(&mut request_line) {
            Ok(0) => return Ok(()), // client closed the connection
            Ok(_) => {}
            // idle keep-alive timeout / reset: close quietly, don't surface as an error
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                return Ok(())
            }
            Err(e) => return Err(e),
        }
        let (method, target) = match parse_request_line(request_line.trim_end()) {
            Some(mt) => mt,
            None => {
                let resp = ApiResponse {
                    status: 400,
                    content_type: "text/plain".into(),
                    body: b"bad request line".to_vec(),
                    headers: Vec::new(),
                };
                stream.write_all(&format_http(&resp))?;
                stream.flush()?;
                return Ok(());
            }
        };
        let strict_json_limit = strict_json_body_limit(&method, &target);
        // Read headers up to the blank line; capture the small set the local security
        // policy needs plus the body length. Unknown headers are ignored.
        let mut headers = RequestHeaders::default();
        let mut content_length = None;
        let mut malformed_framing = false;
        loop {
            let mut h = String::new();
            let n = reader.read_line(&mut h)?;
            if n == 0 || h == "\r\n" || h == "\n" {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                let k = k.trim();
                let v = v.trim().to_string();
                if k.eq_ignore_ascii_case("x-capability-token") {
                    headers.cap_token = Some(v);
                } else if k.eq_ignore_ascii_case("x-session-token") {
                    headers.session_token = Some(v);
                } else if k.eq_ignore_ascii_case("cookie") {
                    headers.cookie = Some(v);
                } else if k.eq_ignore_ascii_case("host") {
                    headers.host = Some(v);
                } else if k.eq_ignore_ascii_case("origin") {
                    headers.origin = Some(v);
                } else if k.eq_ignore_ascii_case("content-length") {
                    if strict_json_limit.is_some() {
                        if content_length.is_some() {
                            malformed_framing = true;
                        } else {
                            content_length = v.parse::<usize>().ok();
                            if content_length.is_none() {
                                malformed_framing = true;
                            }
                        }
                    } else {
                        // Preserve legacy framing behavior outside the three strict JSON
                        // endpoints. Their later hardening belongs to #628.
                        content_length = Some(v.parse::<usize>().unwrap_or(0));
                    }
                } else if k.eq_ignore_ascii_case("x-body-encoding") {
                    headers.body_encoding = Some(v);
                } else if k.eq_ignore_ascii_case("content-type") {
                    headers.content_type = Some(v);
                } else if k.eq_ignore_ascii_case("transfer-encoding") && strict_json_limit.is_some()
                {
                    malformed_framing = true;
                }
            }
        }
        if malformed_framing || (strict_json_limit.is_some() && headers.body_encoding.is_some()) {
            let resp = ApiResponse::error(400, "invalid request framing");
            stream.write_all(&format_http(&resp))?;
            stream.flush()?;
            return Ok(());
        }
        // Local-access policy (loopback host / local origin) runs first, for every path.
        if let Some(resp) = validate_request_headers(policy, &method, &headers) {
            stream.write_all(&format_http(&resp))?;
            stream.flush()?;
            return Ok(()); // rejected → close
        }
        // Read any request body into memory (bounded) so a body-bearing request works
        // over HTTP too (#0A); the query-string GETs that dominate today carry none. An
        // oversized body is refused (413) rather than buffered — it can't be reframed
        // safely on a keep-alive connection, so that path also closes.
        let body_limit = strict_json_limit.unwrap_or(MAX_REQUEST_BODY);
        let content_length = content_length.unwrap_or(0);
        let body = if content_length > body_limit {
            let resp = ApiResponse::error(413, "request body too large");
            stream.write_all(&format_http(&resp))?;
            stream.flush()?;
            return Ok(());
        } else if content_length > 0 {
            let mut buf = vec![0u8; content_length];
            match reader.read_exact(&mut buf) {
                Ok(()) => buf,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e),
            }
        } else {
            Vec::new()
        };
        // #657: an upload may ride base64 (uniform with the JSON bridge); decode it here.
        let body = match decode_body(headers.body_encoding.as_deref(), body, body_limit) {
            Ok(b) => b,
            Err((status, message)) => {
                let resp = ApiResponse::error(status, message);
                stream.write_all(&format_http(&resp))?;
                stream.flush()?;
                return Ok(());
            }
        };
        // Build the routed request from the same transport-agnostic path the in-process
        // bridge uses (#0A): explicit `X-Session-Token`, else the `isy_session` loopback
        // cookie that auto-rides iframe/img/EventSource subresource requests.
        let req = build_request(
            &method,
            &target,
            headers.cap_token.clone(),
            headers.session_token.clone(),
            headers.cookie.clone(),
            headers.content_type.clone(),
            body,
        );
        // SSE change stream: a long-lived connection that bypasses the one-shot
        // response model. Reached only after header validation, so the same
        // loopback/origin rules apply; needs the injected EventBus (daemon only).
        if method == "GET" && req.path == "/api/v1/events" {
            // Mobile profile (#89): SSE is a data route, so it is session-gated too.
            // EventSource can't send headers, so the token rides the `_st` query param
            // (the header is honored as well). No-op on the desktop daemon.
            let st_query = req
                .query
                .iter()
                .find(|(k, _)| k == "_st")
                .map(|(_, v)| v.as_str());
            let provided = req.session_token.as_deref().or(st_query);
            if !router.session_authorized(provided) {
                let resp = ApiResponse::error(401, "missing or invalid session token");
                stream.write_all(&format_http(&resp))?;
                stream.flush()?;
                return Ok(());
            }
            if let Some(bus) = router.events_bus() {
                // A read timeout on the socket would abort the SSE heartbeat loop; clear
                // it before handing the connection over to the long-lived stream.
                let _ = stream.set_read_timeout(None);
                return handle_sse(stream, bus);
            }
        }
        // Agent token stream (S-AG.6/#621): a long-lived per-turn SSE driven by the agent
        // handler's `Receiver<String>` (pre-serialized JSON data lines). Same session gate;
        // the turn id rides the `turn` query param (EventSource can't set headers).
        if method == "GET" && req.path == "/api/v1/agent/stream" {
            let st_query = req
                .query
                .iter()
                .find(|(k, _)| k == "_st")
                .map(|(_, v)| v.as_str());
            let provided = req.session_token.as_deref().or(st_query);
            if !router.session_authorized(provided) {
                let resp = ApiResponse::error(401, "missing or invalid session token");
                stream.write_all(&format_http(&resp))?;
                stream.flush()?;
                return Ok(());
            }
            let turn = req
                .query
                .iter()
                .find(|(k, _)| k == "turn")
                .map(|(_, v)| v.as_str());
            let rx = match (router.agent_handler(), turn) {
                (Some(handler), Some(turn)) if !turn.is_empty() => handler.open_stream(turn),
                _ => None,
            };
            match rx {
                Some(rx) => {
                    let _ = stream.set_read_timeout(None);
                    return handle_agent_sse(stream, rx);
                }
                None => {
                    let resp = ApiResponse::error(404, "unknown or missing turn");
                    stream.write_all(&format_http(&resp))?;
                    stream.flush()?;
                    return Ok(());
                }
            }
        }
        let resp = router.route(&req);
        stream.write_all(&format_http_conn(&resp, true))?;
        stream.flush()?;
        // loop: reuse this connection for the client's next request
    }
}

/// Stream Server-Sent Events until the client disconnects. Writes the event-stream
/// headers, an initial comment, then a `change` frame whenever [`EventBus`] is
/// notified, with a heartbeat comment every 15 s so dead peers are detected. The
/// `flush` error on a closed peer ends the loop (and the connection thread).
fn handle_sse<S: Conn>(stream: &mut S, bus: &EventBus) -> std::io::Result<()> {
    let head = "HTTP/1.1 200 OK\r\n\
        Content-Type: text/event-stream\r\n\
        Cache-Control: no-store\r\n\
        Connection: keep-alive\r\n\
        X-Content-Type-Options: nosniff\r\n\r\n";
    stream.write_all(head.as_bytes())?;
    stream.write_all(b": connected\n\n")?;
    stream.flush()?;
    let mut last = bus.generation();
    loop {
        let g = bus.wait_change(last, std::time::Duration::from_secs(15));
        if g != last {
            last = g;
            stream
                .write_all(format!("event: change\ndata: {{\"generation\":{g}}}\n\n").as_bytes())?;
        } else {
            stream.write_all(b": keep-alive\n\n")?; // heartbeat
        }
        stream.flush()?; // Err when the peer closed -> end the stream
    }
}

/// Stream one agent turn's pre-serialized events as SSE until the turn ends or the peer
/// disconnects. Each `Receiver<String>` item is a single-line JSON `data:` payload; a
/// 15 s timeout emits a heartbeat; `Disconnected` (the turn closed its sender) ends the
/// stream cleanly with a `done` event.
fn handle_agent_sse<S: Conn>(
    stream: &mut S,
    rx: std::sync::mpsc::Receiver<String>,
) -> std::io::Result<()> {
    use std::sync::mpsc::RecvTimeoutError;
    let head = "HTTP/1.1 200 OK\r\n\
        Content-Type: text/event-stream\r\n\
        Cache-Control: no-store\r\n\
        Connection: keep-alive\r\n\
        X-Content-Type-Options: nosniff\r\n\r\n";
    stream.write_all(head.as_bytes())?;
    stream.write_all(b": connected\n\n")?;
    stream.flush()?;
    loop {
        match rx.recv_timeout(std::time::Duration::from_secs(15)) {
            Ok(data) => stream.write_all(format!("data: {data}\n\n").as_bytes())?,
            Err(RecvTimeoutError::Timeout) => stream.write_all(b": keep-alive\n\n")?,
            Err(RecvTimeoutError::Disconnected) => {
                stream.write_all(b"event: done\ndata: {}\n\n")?;
                return stream.flush();
            }
        }
        stream.flush()?; // Err when the peer closed -> end the stream
    }
}

/// Bind a **loopback** TCP address, refusing any non-loopback host. Use `:0` for an
/// OS-assigned free port and read it from `local_addr()` (the standalone mobile
/// client does this, then hands the port to its WebView, #89).
pub fn bind_loopback(addr: &str) -> std::io::Result<TcpListener> {
    if !is_loopback_bind_addr(addr) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing non-loopback TCP bind address; use --socket for owner-only local access",
        ));
    }
    TcpListener::bind(addr)
}

/// Serve a **shared** `router` forever on an already-bound loopback `listener`. The
/// caller keeps its own `Arc<Router>` clone — the standalone mobile client needs one to
/// answer in-process bridge requests ([`dispatch_message`]) against the same router that
/// serves loopback (#0A). Returns only on a fatal accept error.
pub fn serve_listener_shared(listener: TcpListener, router: Arc<Router>) -> std::io::Result<()> {
    for stream in listener.incoming() {
        spawn_conn(stream?, Arc::clone(&router), AccessPolicy::TcpLoopback);
    }
    Ok(())
}

/// Serve `router` forever on an already-bound loopback `listener`. Returns only on a
/// fatal accept error. Lets a caller read the bound port before serving (mobile).
pub fn serve_listener(listener: TcpListener, router: Router) -> std::io::Result<()> {
    serve_listener_shared(listener, Arc::new(router))
}

/// Bind `addr` and serve `router` forever. Returns only on a fatal bind/accept error.
pub fn serve(addr: &str, router: Router) -> std::io::Result<()> {
    let listener = bind_loopback(addr)?;
    let local = listener.local_addr()?;
    eprintln!("iSyncYou web UI listening on http://{local}/");
    serve_listener(listener, router)
}

/// Handle one accepted connection on its own thread (so a long-lived SSE stream
/// never blocks other requests), capped at [`MAX_CONNS`] concurrent threads.
fn spawn_conn<S: Conn + Send + 'static>(mut stream: S, router: Arc<Router>, policy: AccessPolicy) {
    // fetch_add returns the previous count; refuse past the cap.
    if ACTIVE_CONNS.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
        ACTIVE_CONNS.fetch_sub(1, Ordering::SeqCst);
        return; // too many connections; drop this one (the stream closes)
    }
    std::thread::spawn(move || {
        let _guard = ConnGuard; // decrements the live count when this thread ends
        if let Err(e) = handle(&mut stream, &router, policy) {
            eprintln!("connection error: {e}");
        }
        // dropping `stream` here sends FIN so the client sees a clean EOF; a
        // zero-length drain read could block and delay that close.
    });
}

#[cfg(unix)]
pub fn default_unix_socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("isyncyou.sock"),
        _ => {
            let user = std::env::var("USER").unwrap_or_else(|_| std::process::id().to_string());
            std::env::temp_dir().join(format!("isyncyou-{user}.sock"))
        }
    }
}

/// Bind a **Unix-domain socket** at `path` and serve `router` forever. A stale
/// socket file is removed first; the socket is created with mode 0600 so only
/// the owner can talk to the engine — the access control for this local API
/// transport. Returns only on a fatal bind/accept error.
#[cfg(unix)]
pub fn serve_unix(path: &std::path::Path, router: Router) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    // A leftover socket from a previous run would make bind() fail with EADDRINUSE.
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    eprintln!("iSyncYou web UI listening on unix:{}", path.display());
    let router = Arc::new(router);
    for stream in listener.incoming() {
        spawn_conn(stream?, Arc::clone(&router), AccessPolicy::UnixSocket);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_line() {
        assert_eq!(
            parse_request_line("GET /api/v1/items?account=a HTTP/1.1"),
            Some(("GET".into(), "/api/v1/items?account=a".into()))
        );
        assert_eq!(
            parse_request_line("POST / HTTP/1.0"),
            Some(("POST".into(), "/".into()))
        );
        assert_eq!(parse_request_line(""), None);
        assert_eq!(parse_request_line("GET"), None);
    }

    #[test]
    fn strict_agent_json_routes_have_small_preallocation_limits() {
        assert_eq!(
            strict_json_body_limit("POST", "/api/v1/agent/connectivity/preflight"),
            Some(AGENT_STRICT_JSON_MAX_BYTES)
        );
        assert_eq!(
            strict_json_body_limit("POST", "/api/v1/agent/credential/refresh"),
            Some(AGENT_STRICT_JSON_MAX_BYTES)
        );
        assert_eq!(
            strict_json_body_limit("POST", "/api/v1/agent/oauth/cancel"),
            Some(AGENT_STRICT_JSON_MAX_BYTES)
        );
        assert_eq!(strict_json_body_limit("POST", "/api/v1/agent/turn"), None);
        assert_eq!(
            strict_json_body_limit("GET", "/api/v1/agent/connectivity/preflight"),
            None
        );
    }

    #[test]
    fn decode_body_base64_roundtrips_and_bounds() {
        let m = MAX_REQUEST_BODY;
        // base64 (case-insensitive header value) -> raw bytes
        assert_eq!(
            decode_body(Some("base64"), b"aGVsbG8=".to_vec(), m).unwrap(),
            b"hello".to_vec()
        );
        assert_eq!(
            decode_body(Some("BASE64"), b"aGk=".to_vec(), m).unwrap(),
            b"hi".to_vec()
        );
        // absent / other encoding -> passthrough
        assert_eq!(
            decode_body(None, b"raw".to_vec(), m).unwrap(),
            b"raw".to_vec()
        );
        assert_eq!(
            decode_body(Some("identity"), b"raw".to_vec(), m).unwrap(),
            b"raw".to_vec()
        );
        // bad base64 -> 400
        assert_eq!(
            decode_body(Some("base64"), b"@@@".to_vec(), m)
                .unwrap_err()
                .0,
            400
        );
        // decoded "hello" (5 bytes) over a max of 4 -> 413
        assert_eq!(
            decode_body(Some("base64"), b"aGVsbG8=".to_vec(), 4)
                .unwrap_err()
                .0,
            413
        );
    }

    #[test]
    fn formats_http_message() {
        let resp = ApiResponse {
            status: 404,
            content_type: "application/json".into(),
            body: b"{\"error\":\"not found\"}".to_vec(),
            headers: vec![(
                "Content-Security-Policy".into(),
                "default-src 'none'".into(),
            )],
        };
        let bytes = format_http(&resp);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("HTTP/1.1 404 Not Found\r\n"));
        assert!(text.contains("Content-Type: application/json\r\n"));
        assert!(text.contains("Content-Length: 21\r\n"));
        assert!(text.contains("Connection: close\r\n"));
        // extra headers are emitted before the blank line
        assert!(text.contains("Content-Security-Policy: default-src 'none'\r\n"));
        assert!(text.ends_with("{\"error\":\"not found\"}"));
    }

    #[test]
    fn loopback_host_and_origin_validation() {
        for host in [
            "localhost",
            "localhost:8765",
            "127.0.0.1",
            "127.0.0.1:8765",
            "[::1]",
            "[::1]:8765",
        ] {
            assert!(is_loopback_host(host), "{host} should be allowed");
        }
        for host in ["example.com", "127.0.0.2", "localhost.evil.test", ""] {
            assert!(!is_loopback_host(host), "{host} should be rejected");
        }
        assert!(is_local_origin("http://localhost:8765"));
        assert!(is_local_origin("https://127.0.0.1"));
        assert!(!is_local_origin("https://example.com"));
    }

    #[test]
    fn tcp_bind_address_must_be_loopback() {
        for addr in ["127.0.0.1:0", "localhost:0", "[::1]:0"] {
            assert!(is_loopback_bind_addr(addr), "{addr} should be allowed");
        }
        for addr in [
            "0.0.0.0:8765",
            "[::]:8765",
            "192.168.1.10:8765",
            "example.com:8765",
            "localhost.evil.test:8765",
            "8765",
            "",
        ] {
            assert!(!is_loopback_bind_addr(addr), "{addr} should be rejected");
        }
    }

    #[test]
    fn serve_refuses_non_loopback_bind_address() {
        use isyncyou_core::Config;
        let err = serve("0.0.0.0:0", Router::new(Config::default())).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            err.to_string().contains("refusing non-loopback TCP bind"),
            "got: {err}"
        );
    }

    /// Read exactly one HTTP/1.1 response (status line + headers + Content-Length body)
    /// without waiting for EOF — responses are keep-alive now, so the socket stays open.
    fn read_http_response<R: std::io::Read>(r: &mut R) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        let hdr_end = loop {
            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break p + 4;
            }
            match r.read(&mut tmp).unwrap() {
                0 => return String::from_utf8_lossy(&buf).to_string(),
                n => buf.extend_from_slice(&tmp[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..hdr_end]).to_ascii_lowercase();
        let clen = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while buf.len() < hdr_end + clen {
            match r.read(&mut tmp).unwrap() {
                0 => break,
                n => buf.extend_from_slice(&tmp[..n]),
            }
        }
        String::from_utf8_lossy(&buf).to_string()
    }

    fn one_tcp_response(request: &[u8]) -> String {
        use isyncyou_core::Config;
        let router = Router::new(Config::default());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = handle(&mut stream, &router, AccessPolicy::TcpLoopback);
            }
        });
        let mut conn = TcpStream::connect(addr).unwrap();
        conn.write_all(request).unwrap();
        read_http_response(&mut conn)
    }

    #[test]
    fn serve_responds_over_a_real_socket() {
        let buf = one_tcp_response(b"GET /api/v1/accounts HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert!(buf.starts_with("HTTP/1.1 200 OK\r\n"), "got: {buf}");
        assert!(buf.contains("\"accounts\""), "body: {buf}");
    }

    #[test]
    fn keep_alive_reuses_one_connection_for_several_requests() {
        use isyncyou_core::Config;
        let router = Arc::new(Router::new(Config::default()));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                spawn_conn(
                    stream.unwrap(),
                    Arc::clone(&router),
                    AccessPolicy::TcpLoopback,
                );
            }
        });
        // A single TCP connection serves three sequential requests — proof the server
        // does not close after each response (persistent HTTP/1.1).
        let mut c = TcpStream::connect(addr).unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        for i in 0..3 {
            c.write_all(b"GET /api/v1/accounts HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .unwrap();
            let r = read_http_response(&mut c);
            assert!(
                r.starts_with("HTTP/1.1 200 OK\r\n"),
                "request {i} on the reused connection failed: {r}"
            );
            assert!(
                r.to_ascii_lowercase().contains("connection: keep-alive"),
                "request {i} must advertise keep-alive: {r}"
            );
        }
    }

    #[test]
    fn tcp_rejects_non_loopback_host_and_origin() {
        let bad_host =
            one_tcp_response(b"GET /api/v1/accounts HTTP/1.1\r\nHost: example.com\r\n\r\n");
        assert!(
            bad_host.starts_with("HTTP/1.1 403 Forbidden\r\n"),
            "got: {bad_host}"
        );
        assert!(bad_host.contains("invalid host header"));

        let bad_origin = one_tcp_response(
            b"GET /api/v1/accounts HTTP/1.1\r\nHost: localhost\r\nOrigin: https://example.com\r\n\r\n",
        );
        assert!(
            bad_origin.starts_with("HTTP/1.1 403 Forbidden\r\n"),
            "got: {bad_origin}"
        );
        assert!(bad_origin.contains("invalid origin header"));
    }

    #[cfg(unix)]
    #[test]
    fn serve_unix_responds_and_is_owner_only() {
        use isyncyou_core::Config;
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::{UnixListener, UnixStream};
        let router = Router::new(Config::default());
        let dir = std::env::temp_dir().join(format!("isyncyou-uds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("api.sock");
        // mirror serve_unix()'s bind + 0600 + single-connection handler
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)).unwrap();
        // the socket is owner-only (no group/other access) — the access control
        let mode = std::fs::metadata(&sock).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "socket must not be group/other accessible");
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = handle(&mut stream, &router, AccessPolicy::UnixSocket);
            }
        });
        let mut conn = UnixStream::connect(&sock).unwrap();
        // Unix transport is scoped by socket file permissions, so an arbitrary
        // browser-style Host header is not the security boundary here.
        conn.write_all(b"GET /api/v1/accounts HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let buf = read_http_response(&mut conn);
        assert!(buf.starts_with("HTTP/1.1 200 OK\r\n"), "got: {buf}");
        assert!(buf.contains("\"accounts\""), "body: {buf}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_token_gates_data_routes_over_http() {
        // #89: real HTTP roundtrip (curl-equivalent) proving the mobile session-token
        // gate. A native client setting Host: 127.0.0.1 (the desktop loopback policy)
        // still cannot read the data API without the per-process token.
        use isyncyou_core::Config;
        let router =
            Arc::new(Router::new(Config::default()).with_session_token("sess-http-tok".into()));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                spawn_conn(
                    stream.unwrap(),
                    Arc::clone(&router),
                    AccessPolicy::TcpLoopback,
                );
            }
        });
        let req = |raw: &str| {
            let mut c = TcpStream::connect(addr).unwrap();
            c.write_all(raw.as_bytes()).unwrap();
            read_http_response(&mut c)
        };
        // No session token → 401 (the key Android-loopback exposure fix).
        let no_tok = req("GET /api/v1/status HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert!(
            no_tok.starts_with("HTTP/1.1 401"),
            "no token must 401: {no_tok}"
        );
        // Valid token header → passes the gate (no 401).
        let with_tok = req(
            "GET /api/v1/status HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Session-Token: sess-http-tok\r\n\r\n",
        );
        assert!(
            !with_tok.starts_with("HTTP/1.1 401"),
            "valid token must pass: {with_tok}"
        );
        // Valid token via the `_st` query (iframe/img/EventSource path) → passes.
        let with_q =
            req("GET /api/v1/status?_st=sess-http-tok HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert!(
            !with_q.starts_with("HTTP/1.1 401"),
            "valid _st query must pass: {with_q}"
        );
        // Valid token via the loopback cookie (auto-rides subresources on Android).
        let with_cookie = req(
            "GET /api/v1/status HTTP/1.1\r\nHost: 127.0.0.1\r\nCookie: isy_session=sess-http-tok\r\n\r\n",
        );
        assert!(
            !with_cookie.starts_with("HTTP/1.1 401"),
            "valid cookie must pass: {with_cookie}"
        );
        // A wrong cookie value is still rejected.
        let bad_cookie = req(
            "GET /api/v1/status HTTP/1.1\r\nHost: 127.0.0.1\r\nCookie: isy_session=nope\r\n\r\n",
        );
        assert!(
            bad_cookie.starts_with("HTTP/1.1 401"),
            "wrong cookie must 401: {bad_cookie}"
        );
        // Static shell stays open without a token (bootstrap).
        let shell = req("GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert!(
            shell.starts_with("HTTP/1.1 200"),
            "/ must stay open: {shell}"
        );
    }

    #[test]
    fn agent_stream_sse_requires_session_token_on_mobile() {
        use isyncyou_core::Config;

        struct StreamAgent;
        impl crate::AgentHandler for StreamAgent {
            fn start_turn(&self, _account: &str, _prompt: &str) -> Result<String, String> {
                Ok("turn-123".into())
            }

            fn confirm(
                &self,
                _pending_id: &str,
                _token: &str,
                _action_hash: &str,
            ) -> Result<String, String> {
                Ok("{}".into())
            }

            fn cancel(&self, _turn_id: &str) {}

            fn open_stream(&self, turn_id: &str) -> Option<std::sync::mpsc::Receiver<String>> {
                if turn_id != "turn-123" {
                    return None;
                }
                let (tx, rx) = std::sync::mpsc::channel();
                tx.send("{\"event\":\"token\",\"text\":\"hi\"}".to_string())
                    .unwrap();
                Some(rx)
            }
        }

        let router = Arc::new(
            Router::new(Config::default())
                .with_session_token("sess-http-tok".into())
                .with_agent(Arc::new(StreamAgent), "agentsecret".into()),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                spawn_conn(
                    stream.unwrap(),
                    Arc::clone(&router),
                    AccessPolicy::TcpLoopback,
                );
            }
        });
        let req = |raw: &str| {
            let mut c = TcpStream::connect(addr).unwrap();
            c.write_all(raw.as_bytes()).unwrap();
            read_http_response(&mut c)
        };

        let no_token =
            req("GET /api/v1/agent/stream?turn=turn-123 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert!(
            no_token.starts_with("HTTP/1.1 401"),
            "agent stream without session token must 401: {no_token}"
        );

        let mut sse = TcpStream::connect(addr).unwrap();
        sse.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        sse.write_all(
            b"GET /api/v1/agent/stream?turn=turn-123&_st=sess-http-tok HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        )
        .unwrap();
        let mut raw = Vec::new();
        let mut tmp = [0u8; 512];
        for _ in 0..8 {
            match sse.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    raw.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&raw);
                    if text.contains("data: {\"event\":\"token\",\"text\":\"hi\"}") {
                        break;
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(e) => panic!("reading agent SSE: {e}"),
            }
        }
        let with_token = String::from_utf8_lossy(&raw);
        assert!(
            with_token.starts_with("HTTP/1.1 200 OK"),
            "agent stream with session token must connect: {with_token}"
        );
        assert!(with_token.contains("Content-Type: text/event-stream"));
        assert!(with_token.contains("data: {\"event\":\"token\",\"text\":\"hi\"}"));
    }

    #[test]
    fn sse_streams_change_frame_and_serves_concurrently() {
        use isyncyou_core::Config;
        use std::time::Duration;
        let bus = Arc::new(EventBus::new());
        let router = Arc::new(Router::new(Config::default()).with_events(bus.clone()));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                spawn_conn(
                    stream.unwrap(),
                    Arc::clone(&router),
                    AccessPolicy::TcpLoopback,
                );
            }
        });
        // open the long-lived SSE stream
        let mut sse = TcpStream::connect(addr).unwrap();
        sse.write_all(b"GET /api/v1/events HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        sse.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 512];
        let n = sse.read(&mut buf).unwrap();
        assert!(
            String::from_utf8_lossy(&buf[..n]).contains("text/event-stream"),
            "SSE response must be an event stream"
        );
        // a normal request is served CONCURRENTLY while the SSE stream stays open
        let mut c2 = TcpStream::connect(addr).unwrap();
        c2.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        c2.write_all(b"GET /api/v1/accounts HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        // Responses are keep-alive now, so the server won't close the socket — read the
        // response (status line + body arrive together for this small payload) instead of
        // waiting for EOF, which would only come on the idle timeout.
        let mut buf2 = [0u8; 512];
        let n2 = c2.read(&mut buf2).unwrap();
        let s2 = String::from_utf8_lossy(&buf2[..n2]);
        assert!(
            s2.starts_with("HTTP/1.1 200 OK\r\n"),
            "concurrent request blocked by SSE: {s2}"
        );
        // a background notifier guarantees the handler sees a change whenever it
        // reaches wait_change (removes the capture-vs-notify race); read until the
        // change frame arrives.
        let notifier = bus.clone();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        std::thread::spawn(move || {
            while !stop2.load(Ordering::SeqCst) {
                notifier.notify();
                std::thread::sleep(Duration::from_millis(40));
            }
        });
        let mut got_change = false;
        for _ in 0..50 {
            if let Ok(m) = sse.read(&mut buf) {
                if m > 0 && String::from_utf8_lossy(&buf[..m]).contains("event: change") {
                    got_change = true;
                    break;
                }
            }
        }
        stop.store(true, Ordering::SeqCst);
        assert!(got_change, "expected an SSE change frame after notify");
    }

    #[test]
    fn sse_path_enforces_loopback_host() {
        // The local-access policy runs before the SSE branch, so /api/v1/events is
        // guarded by the same loopback rule as every other path.
        let resp = one_tcp_response(b"GET /api/v1/events HTTP/1.1\r\nHost: example.com\r\n\r\n");
        assert!(
            resp.starts_with("HTTP/1.1 403 Forbidden\r\n"),
            "got: {resp}"
        );
    }

    #[test]
    fn build_request_resolves_cookie_session_and_carries_body() {
        // #0A: the shared request builder carries the body and resolves the session from
        // the loopback cookie when no explicit header is present.
        let r = build_request(
            "POST",
            "/api/v1/x?a=1",
            Some("cap-1".into()),
            None,
            Some("other=z; isy_session=cook-tok".into()),
            Some("application/json".into()),
            b"hello-body".to_vec(),
        );
        assert_eq!(r.method, "POST");
        assert_eq!(r.path, "/api/v1/x");
        assert_eq!(r.cap_token.as_deref(), Some("cap-1"));
        assert_eq!(r.session_token.as_deref(), Some("cook-tok"));
        assert_eq!(r.content_type.as_deref(), Some("application/json"));
        assert_eq!(r.body, b"hello-body");
        // An explicit X-Session-Token header wins over the cookie.
        let r2 = build_request(
            "GET",
            "/api/v1/x",
            None,
            Some("hdr-tok".into()),
            Some("isy_session=cook-tok".into()),
            None,
            Vec::new(),
        );
        assert_eq!(r2.session_token.as_deref(), Some("hdr-tok"));
        assert!(r2.body.is_empty());
    }

    #[test]
    fn dispatch_message_routes_without_a_socket() {
        // #0A: the in-process bridge path routes identically to HTTP — no TCP port bound.
        use isyncyou_core::Config;
        let open = Router::new(Config::default());
        let ok = dispatch_message(
            &open,
            BridgeDispatchRequest {
                method: "GET",
                target: "/api/v1/accounts",
                cap_token: None,
                session_token: None,
                cookie: None,
                content_type: None,
                body: Vec::new(),
            },
        );
        assert_eq!(ok.status, 200, "bridge GET should route");
        assert!(
            String::from_utf8_lossy(&ok.body).contains("accounts"),
            "bridge body: {}",
            String::from_utf8_lossy(&ok.body)
        );
        // The session gate applies through the bridge too (same Router::route).
        let gated = Router::new(Config::default()).with_session_token("sess-bridge".into());
        let denied = dispatch_message(
            &gated,
            BridgeDispatchRequest {
                method: "GET",
                target: "/api/v1/status",
                cap_token: None,
                session_token: None,
                cookie: None,
                content_type: None,
                body: Vec::new(),
            },
        );
        assert_eq!(denied.status, 401, "bridge without token must 401");
        let allowed = dispatch_message(
            &gated,
            BridgeDispatchRequest {
                method: "GET",
                target: "/api/v1/status",
                cap_token: None,
                session_token: Some("sess-bridge".into()),
                cookie: None,
                content_type: None,
                body: Vec::new(),
            },
        );
        assert_ne!(allowed.status, 401, "bridge with token must pass the gate");
    }

    #[test]
    fn handle_bridge_request_frames_json_response_and_enforces_session() {
        // #0A: the bridge's JSON wire protocol — Kotlin forwards strings, all parsing is
        // here. Request envelope in, response envelope out; the session gate applies.
        use isyncyou_core::Config;
        use serde_json::Value;
        let open = Router::new(Config::default());
        let out = handle_bridge_request(
            &open,
            r#"{"t":"req","id":"r7","method":"GET","path":"/api/v1/accounts","headers":{},"body":null}"#,
        );
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["t"], "res", "reply is framed for the JS onmessage router");
        assert_eq!(v["id"], "r7", "id is echoed so the JS promise resolves");
        assert_eq!(v["status"], 200);
        assert!(
            v["body"].as_str().unwrap().contains("accounts"),
            "envelope body: {out}"
        );

        // A gated router refuses without a token, passes with the canonical header
        // (case-insensitively matched).
        let gated = Router::new(Config::default()).with_session_token("sess-br".into());
        let denied = handle_bridge_request(
            &gated,
            r#"{"method":"GET","path":"/api/v1/status","headers":{}}"#,
        );
        assert_eq!(
            serde_json::from_str::<Value>(&denied).unwrap()["status"],
            401
        );
        let ok = handle_bridge_request(
            &gated,
            r#"{"method":"GET","path":"/api/v1/status","headers":{"x-session-token":"sess-br"}}"#,
        );
        assert_ne!(serde_json::from_str::<Value>(&ok).unwrap()["status"], 401);

        // Malformed JSON → a 400 envelope, never a panic.
        let bad = handle_bridge_request(&open, "not json");
        assert_eq!(serde_json::from_str::<Value>(&bad).unwrap()["status"], 400);
    }

    #[test]
    fn http_reads_body_then_serves_next_request_on_same_connection() {
        // #0A: a Content-Length body is read (not drained ad-hoc), so the next request on
        // the same keep-alive connection is still framed correctly.
        use isyncyou_core::Config;
        let router = Arc::new(Router::new(Config::default()));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                spawn_conn(
                    stream.unwrap(),
                    Arc::clone(&router),
                    AccessPolicy::TcpLoopback,
                );
            }
        });
        let mut c = TcpStream::connect(addr).unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        // A POST with a 5-byte body to an unknown route: the body must be consumed.
        c.write_all(
            b"POST /api/v1/nope HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\nhello",
        )
        .unwrap();
        let first = read_http_response(&mut c);
        assert!(first.starts_with("HTTP/1.1 "), "first response: {first}");
        // The follow-up GET on the SAME connection routes cleanly — proof the 5 body
        // bytes did not bleed into this request's parse.
        c.write_all(b"GET /api/v1/accounts HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let second = read_http_response(&mut c);
        assert!(
            second.starts_with("HTTP/1.1 200 OK\r\n"),
            "follow-up after body must succeed: {second}"
        );
        assert!(second.contains("\"accounts\""), "body: {second}");
    }
}
