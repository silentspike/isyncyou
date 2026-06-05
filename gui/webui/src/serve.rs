//! Minimal localhost HTTP/1.1 adapter for [`Router`] (plan §25).
//!
//! Hand-rolled over `std::net` — no web framework — because the surface is tiny:
//! GET requests plus a few capability-token-guarded POSTs, one response per
//! connection (`Connection: close`).
//! The loop is **single-threaded**: a personal localhost UI serves one user, and
//! handling requests sequentially means each per-request [`Store`] open holds the
//! single-instance lock only momentarily, with no contention. (The production
//! daemon, which keeps stores open, will front this with an async server.)
//!
//! Two transports share one [`handle`]: TCP loopback ([`serve`]) for the browser
//! UI, and a **Unix-domain socket** ([`serve_unix`]) for owner-only local access
//! where filesystem permissions (mode 0600) are the access control.

use crate::{ApiRequest, ApiResponse, Router};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
#[cfg(unix)]
use std::path::PathBuf;

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
    host: Option<String>,
    origin: Option<String>,
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
            } else if k.eq_ignore_ascii_case("host") {
                headers.host = Some(v);
            } else if k.eq_ignore_ascii_case("origin") {
                headers.origin = Some(v);
            }
        }
    }
    let resp = match parse_request_line(request_line.trim_end()) {
        Some((method, target)) => match validate_request_headers(policy, &method, &headers) {
            Some(resp) => resp,
            None => router.route(
                &ApiRequest::new(&method, &target).with_cap_token(headers.cap_token.clone()),
            ),
        },
        None => ApiResponse {
            status: 400,
            content_type: "text/plain".into(),
            body: b"bad request line".to_vec(),
            headers: Vec::new(),
        },
    };
    stream.write_all(&format_http(&resp))?;
    stream.flush()
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
    for stream in listener.incoming() {
        let mut stream = stream?;
        if let Err(e) = handle(&mut stream, &router, AccessPolicy::TcpLoopback) {
            eprintln!("connection error: {e}");
        }
        // best-effort: drain anything unread so the client sees a clean close
        let _ = stream.read(&mut [0u8; 0]);
    }
    Ok(())
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
    for stream in listener.incoming() {
        let mut stream = stream?;
        if let Err(e) = handle(&mut stream, &router, AccessPolicy::UnixSocket) {
            eprintln!("connection error: {e}");
        }
        let _ = stream.read(&mut [0u8; 0]);
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
}
