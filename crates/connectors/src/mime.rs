//! Minimal, dependency-free MIME text extraction for the mail-body search index.
//!
//! [`extract_text`] turns a raw `.eml` into searchable plain text: it walks the
//! MIME tree, prefers a `text/plain` part (falling back to a tag-stripped
//! `text/html` part), decodes `quoted-printable`/`base64` transfer encodings, and
//! prepends the decoded `Subject`. It is best-effort and never panics on
//! malformed input — the goal is good FTS recall, not a faithful renderer.
//!
//! Charset handling assumes UTF-8 (decoded losslessly via `from_utf8_lossy`);
//! legacy charsets degrade to replacement characters, which is acceptable for an
//! index. The extracted text is capped so a pathological message can't bloat the
//! index.

/// Hard cap on extracted text length (bytes of UTF-8) per message.
const MAX_TEXT: usize = 256 * 1024;

/// Extract searchable plain text from a raw `.eml` message.
pub fn extract_text(eml: &[u8]) -> String {
    let (headers, body) = split_headers(eml);
    let mut out = String::new();

    // Subject first, so subject terms are body-searchable too.
    if let Some(subj) = header_value(&headers, "subject") {
        out.push_str(&decode_header_text(&subj));
        out.push('\n');
    }
    out.push_str(&extract_part(&headers, body));

    if out.len() > MAX_TEXT {
        out.truncate(MAX_TEXT);
    }
    out
}

/// Split a MIME entity into its raw header block and body bytes at the first
/// blank line (CRLF or LF).
fn split_headers(data: &[u8]) -> (Vec<u8>, &[u8]) {
    if let Some(p) = find(data, b"\r\n\r\n") {
        (unfold(&data[..p]), &data[p + 4..])
    } else if let Some(p) = find(data, b"\n\n") {
        (unfold(&data[..p]), &data[p + 2..])
    } else {
        (unfold(data), &[])
    }
}

/// Join folded header continuation lines (a line starting with space/tab is a
/// continuation of the previous one).
fn unfold(headers: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(headers);
    let mut out = String::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with(' ') || line.starts_with('\t') {
            out.push(' ');
            out.push_str(line.trim_start());
        } else {
            out.push('\n');
            out.push_str(line);
        }
    }
    out.into_bytes()
}

/// Case-insensitive lookup of a header's value (first occurrence).
fn header_value(headers: &[u8], name: &str) -> Option<String> {
    let text = String::from_utf8_lossy(headers);
    let want = name.to_ascii_lowercase();
    for line in text.split('\n') {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().to_ascii_lowercase() == want {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// A `Content-Type` parameter (e.g. `boundary`, `charset`) from a header value.
fn ct_param(value: &str, param: &str) -> Option<String> {
    let want = format!("{}=", param.to_ascii_lowercase());
    for part in value.split(';') {
        let part = part.trim();
        if part.to_ascii_lowercase().starts_with(&want) {
            let v = &part[want.len()..];
            return Some(v.trim().trim_matches('"').to_string());
        }
    }
    None
}

/// Recursively extract text from one MIME entity (given its headers + body).
fn extract_part(headers: &[u8], body: &[u8]) -> String {
    let ctype = header_value(headers, "content-type").unwrap_or_default();
    let ctype_l = ctype.to_ascii_lowercase();
    let cte = header_value(headers, "content-transfer-encoding")
        .unwrap_or_default()
        .to_ascii_lowercase();

    if ctype_l.starts_with("multipart/") {
        let boundary = match ct_param(&ctype, "boundary") {
            Some(b) => b,
            None => return String::new(),
        };
        let parts = split_multipart(body, &boundary);
        // prefer the first text/plain part; else the first text/html part.
        let mut html_fallback: Option<String> = None;
        for part in &parts {
            let (ph, pb) = split_headers(part);
            let pct = header_value(&ph, "content-type")
                .unwrap_or_default()
                .to_ascii_lowercase();
            if pct.starts_with("multipart/") {
                let nested = extract_part(&ph, pb);
                if !nested.trim().is_empty() {
                    return nested;
                }
            } else if pct.starts_with("text/plain") || (pct.is_empty()) {
                return extract_part(&ph, pb);
            } else if pct.starts_with("text/html") && html_fallback.is_none() {
                html_fallback = Some(extract_part(&ph, pb));
            }
        }
        return html_fallback.unwrap_or_default();
    }

    let decoded = decode_body(body, &cte);
    if ctype_l.starts_with("text/html") {
        strip_html(&String::from_utf8_lossy(&decoded))
    } else {
        // text/plain or unknown -> treat as text
        String::from_utf8_lossy(&decoded).into_owned()
    }
}

/// Split a multipart body into its parts by `--boundary` delimiters.
fn split_multipart(body: &[u8], boundary: &str) -> Vec<Vec<u8>> {
    let delim = format!("--{boundary}");
    let text = String::from_utf8_lossy(body);
    let mut parts = Vec::new();
    let mut current: Option<String> = None;
    for line in text.split('\n') {
        let trimmed = line.strip_suffix('\r').unwrap_or(line);
        if trimmed == delim || trimmed == format!("{delim}--") {
            if let Some(buf) = current.take() {
                parts.push(buf.into_bytes());
            }
            if trimmed.ends_with("--") {
                break; // closing delimiter
            }
            current = Some(String::new());
        } else if let Some(buf) = current.as_mut() {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    parts
}

/// Decode a body by its transfer encoding (`quoted-printable`/`base64`/other).
fn decode_body(body: &[u8], cte: &str) -> Vec<u8> {
    match cte {
        "base64" => base64_decode(body),
        "quoted-printable" => qp_decode(body),
        _ => body.to_vec(), // 7bit / 8bit / binary / unknown
    }
}

/// Decode standard base64, ignoring whitespace/newlines and stopping at padding.
fn base64_decode(data: &[u8]) -> Vec<u8> {
    fn val(b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &b in data {
        if b == b'=' {
            break;
        }
        if let Some(v) = val(b) {
            buf = (buf << 6) | v as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((buf >> bits) as u8);
            }
        }
    }
    out
}

/// Decode quoted-printable (`=XX` hex bytes, `=\n` soft line breaks).
fn qp_decode(data: &[u8]) -> Vec<u8> {
    let hex = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == b'=' {
            if i + 1 < data.len() && (data[i + 1] == b'\n') {
                i += 2; // soft break "=\n"
                continue;
            }
            if i + 2 < data.len() && data[i + 1] == b'\r' && data[i + 2] == b'\n' {
                i += 3; // soft break "=\r\n"
                continue;
            }
            if i + 2 < data.len() {
                if let (Some(h), Some(l)) = (hex(data[i + 1]), hex(data[i + 2])) {
                    out.push(h << 4 | l);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(data[i]);
        i += 1;
    }
    out
}

/// Crudely strip HTML: drop `<...>` tags + `<script>/<style>` contents and decode
/// the few entities that matter for search recall.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // skip whole <script>/<style> blocks
            for tag in ["script", "style"] {
                let open = format!("<{tag}");
                if lower[i..].starts_with(&open) {
                    let close = format!("</{tag}>");
                    if let Some(end) = lower[i..].find(&close) {
                        i += end + close.len();
                    } else {
                        i = bytes.len();
                    }
                }
            }
            if i >= bytes.len() {
                break;
            }
            if bytes[i] == b'<' {
                match html[i..].find('>') {
                    Some(end) => {
                        i += end + 1;
                        out.push(' ');
                    }
                    None => break,
                }
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
}

/// Decode RFC 2047 encoded-word subjects (`=?utf-8?B?..?=` / `=?utf-8?Q?..?=`),
/// best-effort; leaves plain text untouched.
fn decode_header_text(s: &str) -> String {
    if !s.contains("=?") {
        return s.to_string();
    }
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("=?") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        // =?charset?enc?text?=  -> inner = "charset?enc?text"
        if let Some(close) = after.find("?=") {
            let inner = &after[..close];
            let f: Vec<&str> = inner.splitn(3, '?').collect();
            if f.len() == 3 {
                let bytes = match f[1].to_ascii_uppercase().as_str() {
                    "B" => base64_decode(f[2].as_bytes()),
                    "Q" => qp_decode(f[2].replace('_', " ").as_bytes()),
                    _ => f[2].as_bytes().to_vec(),
                };
                out.push_str(&String::from_utf8_lossy(&bytes));
                rest = &after[close + 2..];
                continue;
            }
        }
        // malformed encoded-word: emit literally and stop
        out.push_str(&rest[start..]);
        return out;
    }
    out.push_str(rest);
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_7bit() {
        let eml =
            b"Subject: Hello\r\nContent-Type: text/plain\r\n\r\nThe invoice total is 1200 EUR.\r\n";
        let t = extract_text(eml);
        assert!(t.contains("Hello"));
        assert!(t.contains("invoice total is 1200 EUR"));
    }

    #[test]
    fn quoted_printable_decodes_umlauts_and_soft_breaks() {
        let eml = b"Subject: Test\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nGesch=C3=A4ftsf=C3=BChrung sehr lan=\r\nger Satz.";
        let t = extract_text(eml);
        assert!(t.contains("Geschäftsführung"), "qp not decoded: {t:?}");
        assert!(t.contains("langer Satz"), "soft break not joined: {t:?}");
    }

    #[test]
    fn base64_body_decodes() {
        // "Confidential report attached" base64-encoded
        let b64 = "Q29uZmlkZW50aWFsIHJlcG9ydCBhdHRhY2hlZA==";
        let eml = format!(
            "Subject: x\r\nContent-Type: text/plain\r\nContent-Transfer-Encoding: base64\r\n\r\n{b64}"
        );
        let t = extract_text(eml.as_bytes());
        assert!(t.contains("Confidential report attached"), "got: {t:?}");
    }

    #[test]
    fn multipart_prefers_text_plain() {
        let eml = b"Subject: Multi\r\nContent-Type: multipart/alternative; boundary=BND\r\n\r\n\
--BND\r\nContent-Type: text/plain\r\n\r\nplain body keyword apple\r\n\
--BND\r\nContent-Type: text/html\r\n\r\n<p>html body keyword banana</p>\r\n\
--BND--\r\n";
        let t = extract_text(eml);
        assert!(t.contains("apple"), "should pick text/plain: {t:?}");
        assert!(
            !t.contains("banana"),
            "should not use html when plain exists: {t:?}"
        );
    }

    #[test]
    fn html_only_is_stripped() {
        let eml = b"Subject: H\r\nContent-Type: text/html\r\n\r\n<html><style>p{color:red}</style><body><p>Hello <b>world</b></p><script>evil()</script></body></html>";
        let t = extract_text(eml);
        assert!(
            t.contains("Hello") && t.contains("world"),
            "tags not stripped: {t:?}"
        );
        assert!(
            !t.contains("evil") && !t.contains("color:red"),
            "script/style leaked: {t:?}"
        );
    }

    #[test]
    fn encoded_word_subject_decodes() {
        let eml = b"Subject: =?utf-8?B?w4RwZmVs?=\r\nContent-Type: text/plain\r\n\r\nbody";
        let t = extract_text(eml);
        assert!(
            t.contains("Äpfel"),
            "encoded-word subject not decoded: {t:?}"
        );
    }

    #[test]
    fn malformed_input_does_not_panic() {
        let _ = extract_text(b"");
        let _ = extract_text(b"no headers at all just text");
        let _ = extract_text(b"Content-Type: multipart/mixed; boundary=X\r\n\r\nno parts");
        let _ = extract_text(b"=?utf-8?B?broken");
    }
}
