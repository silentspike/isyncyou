//! Minimal localhost HTTP/1.1 adapter for [`Router`] (plan §25).
//!
//! Hand-rolled over `std::net` — no web framework — because the surface is tiny:
//! GET requests with no body, one response per connection (`Connection: close`).
//! The loop is **single-threaded**: a personal localhost UI serves one user, and
//! handling requests sequentially means each per-request [`Store`] open holds the
//! single-instance lock only momentarily, with no contention. (The production
//! daemon, which keeps stores open, will front this with an async server.)

use crate::{ApiRequest, ApiResponse, Router};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

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

fn handle(stream: &mut TcpStream, router: &Router) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(()); // client closed
    }
    // Drain headers up to the blank line (we don't need them for GET).
    loop {
        let mut h = String::new();
        let n = reader.read_line(&mut h)?;
        if n == 0 || h == "\r\n" || h == "\n" {
            break;
        }
    }
    let resp = match parse_request_line(request_line.trim_end()) {
        Some((method, target)) => router.route(&ApiRequest::new(&method, &target)),
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
}
