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

/// Hard cap on concurrent connection threads (safety against runaway opens on a
/// loopback-only server). SSE streams count against this, so it is generous.
const MAX_CONNS: usize = 128;
static ACTIVE_CONNS: AtomicUsize = AtomicUsize::new(0);

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
}
impl Conn for TcpStream {
    fn clone_reader(&self) -> std::io::Result<Box<dyn Read>> {
        Ok(Box::new(self.try_clone()?))
    }
}
#[cfg(unix)]
impl Conn for std::os::unix::net::UnixStream {
    fn clone_reader(&self) -> std::io::Result<Box<dyn Read>> {
        Ok(Box::new(self.try_clone()?))
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

/// Serialize a response as an HTTP/1.1 message with `Connection: close`.
pub fn format_http(resp: &ApiResponse) -> Vec<u8> {
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
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n",
        resp.status,
        reason,
        resp.content_type,
        resp.body.len()
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
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(()); // client closed
    }
    // Read headers up to the blank line; capture the small set the local security
    // policy needs. Unknown headers are ignored.
    let mut headers = RequestHeaders::default();
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
            }
        }
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
            return stream.flush();
        }
    };
    // Local-access policy (loopback host / local origin) runs first, for every path.
    if let Some(resp) = validate_request_headers(policy, &method, &headers) {
        stream.write_all(&format_http(&resp))?;
        return stream.flush();
    }
    // Effective session token (#89): an explicit `X-Session-Token` header (used by
    // the web UI's fetch() calls) OR a loopback `isy_session` cookie set natively by
    // the Android shell — the cookie auto-rides iframe/img/EventSource subresource
    // requests, so embedded resources are gated without per-URL `_st` threading.
    let session_token = headers.session_token.clone().or_else(|| {
        headers
            .cookie
            .as_deref()
            .and_then(|c| cookie_value(c, "isy_session"))
    });
    let req = ApiRequest::new(&method, &target)
        .with_cap_token(headers.cap_token.clone())
        .with_session_token(session_token);
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
            return stream.flush();
        }
        if let Some(bus) = router.events_bus() {
            return handle_sse(stream, bus);
        }
    }
    let resp = router.route(&req);
    stream.write_all(&format_http(&resp))?;
    stream.flush()
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

/// Bind `addr` and serve `router` forever (single-threaded). Returns only on a
/// fatal bind/accept error.
pub fn serve(addr: &str, router: Router) -> std::io::Result<()> {
    if !is_loopback_bind_addr(addr) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing non-loopback TCP bind address; use --socket for owner-only local access",
        ));
    }
    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    eprintln!("iSyncYou web UI listening on http://{local}/");
    let router = Arc::new(router);
    for stream in listener.incoming() {
        spawn_conn(stream?, Arc::clone(&router), AccessPolicy::TcpLoopback);
    }
    Ok(())
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

    fn one_tcp_response(request: &[u8]) -> String {
        use isyncyou_core::Config;
        use std::io::Read as _;
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
        let mut buf = String::new();
        conn.read_to_string(&mut buf).unwrap();
        buf
    }

    #[test]
    fn serve_responds_over_a_real_socket() {
        let buf = one_tcp_response(b"GET /api/v1/accounts HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert!(buf.starts_with("HTTP/1.1 200 OK\r\n"), "got: {buf}");
        assert!(buf.contains("\"accounts\""), "body: {buf}");
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
        let mut buf = String::new();
        conn.read_to_string(&mut buf).unwrap();
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
            let mut s = String::new();
            c.read_to_string(&mut s).unwrap();
            s
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
        let mut s2 = String::new();
        c2.read_to_string(&mut s2).unwrap();
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
}
