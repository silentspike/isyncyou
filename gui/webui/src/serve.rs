//! Minimal localhost HTTP/1.1 adapter for [`Router`] (plan §25).
//!
//! Hand-rolled over `std::net` — no web framework — because the surface is tiny:
//! GET requests with no body, one response per connection (`Connection: close`).
//! The loop is **single-threaded**: a personal localhost UI serves one user, and
//! handling requests sequentially means each per-request [`Store`] open holds the
//! single-instance lock only momentarily, with no contention. (The production
//! daemon, which keeps stores open, will front this with an async server.)
//!
//! Two transports share one [`handle`]: TCP ([`serve`]) for the opt-in remote
//! case, and a **Unix-domain socket** ([`serve_unix`]) — the desktop default per
//! plan §11, where filesystem permissions (mode 0600) are the access control.

use crate::{ApiRequest, ApiResponse, Router};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

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

fn handle<S: Conn>(stream: &mut S, router: &Router) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.clone_reader()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(()); // client closed
    }
    // Read headers up to the blank line; capture the capability token for POSTs.
    let mut cap_token = None;
    loop {
        let mut h = String::new();
        let n = reader.read_line(&mut h)?;
        if n == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.trim().eq_ignore_ascii_case("x-capability-token") {
                cap_token = Some(v.trim().to_string());
            }
        }
    }
    let resp = match parse_request_line(request_line.trim_end()) {
        Some((method, target)) => {
            router.route(&ApiRequest::new(&method, &target).with_cap_token(cap_token))
        }
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
    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    eprintln!("iSyncYou web UI listening on http://{local}/");
    for stream in listener.incoming() {
        let mut stream = stream?;
        if let Err(e) = handle(&mut stream, &router) {
            eprintln!("connection error: {e}");
        }
        // best-effort: drain anything unread so the client sees a clean close
        let _ = stream.read(&mut [0u8; 0]);
    }
    Ok(())
}

/// Bind a **Unix-domain socket** at `path` and serve `router` forever (the
/// desktop default, plan §11). A stale socket file is removed first; the socket
/// is created with mode 0600 so only the owner can talk to the engine — the
/// access control for the local API. Returns only on a fatal bind/accept error.
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
        if let Err(e) = handle(&mut stream, &router) {
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
    fn serve_responds_over_a_real_socket() {
        use isyncyou_core::Config;
        use std::io::Read as _;
        // bind on an ephemeral port in a thread, then make one request
        let router = Router::new(Config::default());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            // serve a single connection then stop (mirrors serve()'s handler)
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = handle(&mut stream, &router);
            }
        });
        let mut conn = TcpStream::connect(addr).unwrap();
        conn.write_all(b"GET /api/v1/accounts HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let mut buf = String::new();
        conn.read_to_string(&mut buf).unwrap();
        assert!(buf.starts_with("HTTP/1.1 200 OK\r\n"), "got: {buf}");
        assert!(buf.contains("\"accounts\""), "body: {buf}");
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
                let _ = handle(&mut stream, &router);
            }
        });
        let mut conn = UnixStream::connect(&sock).unwrap();
        conn.write_all(b"GET /api/v1/accounts HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let mut buf = String::new();
        conn.read_to_string(&mut buf).unwrap();
        assert!(buf.starts_with("HTTP/1.1 200 OK\r\n"), "got: {buf}");
        assert!(buf.contains("\"accounts\""), "body: {buf}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
