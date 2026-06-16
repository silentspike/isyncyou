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

/// Extract the **raw (decoded) HTML** of the first `text/html` part of an `.eml`,
/// if any. Unlike [`extract_text`] (which strips tags for the search index), this
/// keeps the markup so a sanitizing viewer can render it. Returns `None` when the
/// message is plain-text only. Best-effort and never panics.
pub fn extract_html(eml: &[u8]) -> Option<String> {
    extract_html_with_inline_images(eml).map(|h| h.html)
}

/// One safe inline MIME image referenced by a `cid:` URL in an HTML mail part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineImage {
    pub cid: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

/// Decoded HTML plus the owner-addressed inline images that may be safely mapped
/// from `cid:` to `data:` in the sanitized viewer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HtmlWithInlineImages {
    pub html: String,
    pub inline_images: Vec<InlineImage>,
}

const MAX_INLINE_IMAGE_BYTES: usize = 512 * 1024;

/// Extract the first decoded HTML part and safe `Content-ID` image parts from an
/// `.eml`. Only non-SVG image types that the browser can render inertly under a
/// `data:`-only CSP are returned. Best-effort and never panics.
pub fn extract_html_with_inline_images(eml: &[u8]) -> Option<HtmlWithInlineImages> {
    let (headers, body) = split_headers(eml);
    let mut out = HtmlWithInlineImages::default();
    collect_html_resources(&headers, body, &mut out);
    if out.html.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// A non-destructive preview of what restoring an archived mail item *would* create
/// in the cloud, built purely from the archived `.eml`. It contacts nothing — it is
/// a read of local bytes only — so it is safe to show even when cloud restore is
/// disabled. Best-effort and never panics on malformed input.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MailPreview {
    pub subject: Option<String>,
    pub from: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub date: Option<String>,
    pub message_id: Option<String>,
    /// Size of the archived MIME in bytes.
    pub size_bytes: usize,
    /// Whether the message carries an HTML alternative.
    pub has_html: bool,
    /// Number of declared attachments (MIME parts with `Content-Disposition:
    /// attachment`). A best-effort count over the archived MIME, never panics.
    pub attachment_count: usize,
    /// A short, whitespace-collapsed snippet of the decoded body text.
    pub body_snippet: String,
}

/// Parse an archived `.eml` into a [`MailPreview`]. Reads only; never contacts Graph.
pub fn mail_preview(eml: &[u8]) -> MailPreview {
    let (headers, body) = split_headers(eml);
    MailPreview {
        subject: header_value(&headers, "subject").map(|s| decode_header_text(&s)),
        from: header_value(&headers, "from").map(|s| decode_header_text(&s)),
        to: header_value(&headers, "to")
            .map(|s| split_addresses(&decode_header_text(&s)))
            .unwrap_or_default(),
        cc: header_value(&headers, "cc")
            .map(|s| split_addresses(&decode_header_text(&s)))
            .unwrap_or_default(),
        date: header_value(&headers, "date"),
        message_id: header_value(&headers, "message-id"),
        size_bytes: eml.len(),
        has_html: extract_html(eml).is_some(),
        attachment_count: count_attachments(eml),
        body_snippet: snippet(&extract_part(&headers, body), 280),
    }
}

/// Count declared attachments: MIME `Content-Disposition` headers whose value
/// starts with `attachment`. A cheap, real heuristic over the raw bytes (no full
/// MIME tree walk needed) — robust to folding and case. Never panics.
pub fn count_attachments(eml: &[u8]) -> usize {
    let text = String::from_utf8_lossy(eml);
    let mut n = 0;
    for line in text.split('\n') {
        let l = line.trim_start();
        if l.len() >= 20 && l[..20.min(l.len())].eq_ignore_ascii_case("content-disposition:") {
            let v = l[20..].trim_start().to_ascii_lowercase();
            if v.starts_with("attachment") {
                n += 1;
            }
        }
    }
    n
}

/// Return a copy of `eml` whose `Message-ID` header is exactly `message_id`
/// (replacing any existing one). Used by crash-safe restore to stamp a controlled,
/// findable marker into the MIME before posting it, so recovery can locate a
/// possibly-created message by its `internetMessageId`. Headers are rewritten
/// unfolded with CRLF; the body is preserved byte-for-byte. Never panics.
pub fn set_message_id(eml: &[u8], message_id: &str) -> Vec<u8> {
    let (headers, body) = split_headers(eml);
    let mut out = String::new();
    out.push_str("Message-ID: ");
    out.push_str(message_id);
    out.push_str("\r\n");
    for line in String::from_utf8_lossy(&headers).split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if line.to_ascii_lowercase().starts_with("message-id:") {
            continue; // drop the original Message-ID
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// Split a header address list on commas into trimmed, non-empty entries.
fn split_addresses(s: &str) -> Vec<String> {
    s.split(',')
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
        .collect()
}

/// Collapse runs of whitespace and truncate to `max` characters (on a char
/// boundary), appending an ellipsis when truncated.
fn snippet(text: &str, max: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        collapsed
    } else {
        let head: String = collapsed.chars().take(max).collect();
        format!("{head}…")
    }
}

/// Walk a MIME entity, collecting the first HTML body and safe inline image parts.
fn collect_html_resources(headers: &[u8], body: &[u8], out: &mut HtmlWithInlineImages) {
    let ctype = header_value(headers, "content-type").unwrap_or_default();
    let ctype_l = ctype.to_ascii_lowercase();

    if ctype_l.starts_with("multipart/") {
        let Some(boundary) = ct_param(&ctype, "boundary") else {
            return;
        };
        for part in split_multipart(body, &boundary) {
            let (ph, pb) = split_headers(&part);
            collect_html_resources(&ph, pb, out);
        }
        return;
    }

    let cte = header_value(headers, "content-transfer-encoding")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if ctype_l.starts_with("text/html") {
        if out.html.is_empty() {
            let decoded = decode_body(body, &cte);
            out.html = String::from_utf8_lossy(&decoded).into_owned();
        }
        return;
    }

    let base_type = ctype_l.split(';').next().unwrap_or("").trim();
    if let Some(content_type) = safe_inline_image_type(base_type) {
        if let Some(cid) = content_id(headers) {
            let decoded = decode_body(body, &cte);
            if !decoded.is_empty() && decoded.len() <= MAX_INLINE_IMAGE_BYTES {
                out.inline_images.push(InlineImage {
                    cid,
                    content_type: content_type.to_string(),
                    data: decoded,
                });
            }
        }
    }
}

fn safe_inline_image_type(content_type: &str) -> Option<&'static str> {
    match content_type {
        "image/png" => Some("image/png"),
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        _ => None,
    }
}

fn content_id(headers: &[u8]) -> Option<String> {
    let raw = header_value(headers, "content-id")?;
    let id = raw
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
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
        // lang-allow: German QP-encoded fixture, present to verify umlaut + soft-break decoding.
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
        // empty input → empty extracted text and no HTML part
        assert!(extract_text(b"").is_empty());
        assert_eq!(extract_html(b""), None);
        // headerless garbage has no subject line and no HTML part
        assert_eq!(extract_html(b"no headers at all just text"), None);
        // a multipart envelope with no actual parts yields no HTML
        assert_eq!(
            extract_html(b"Content-Type: multipart/mixed; boundary=X\r\n\r\nno parts"),
            None
        );
        // a broken RFC2047 encoded-word decodes without panicking
        let _ = extract_text(b"=?utf-8?B?broken");
    }

    #[test]
    fn extract_html_returns_raw_markup_of_html_part() {
        // multipart/alternative: text/plain + text/html — extract_html picks the HTML
        let eml = b"Content-Type: multipart/alternative; boundary=B\r\n\r\n\
--B\r\nContent-Type: text/plain\r\n\r\nplain version\r\n\
--B\r\nContent-Type: text/html\r\n\r\n<p>Hello <b>world</b></p>\r\n\
--B--\r\n";
        let html = extract_html(eml).expect("should find an html part");
        assert!(html.contains("<b>world</b>"), "raw markup lost: {html:?}");
        assert!(html.contains("<p>Hello"), "markup: {html:?}");
    }

    #[test]
    fn extract_html_decodes_quoted_printable_and_is_none_for_plain() {
        let qp = b"Content-Type: text/html\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n<p>caf=C3=A9</p>";
        assert!(extract_html(qp).unwrap().contains("café"), "qp not decoded");
        // a plain-text-only message has no HTML part
        let plain = b"Content-Type: text/plain\r\n\r\njust text";
        assert_eq!(extract_html(plain), None);
    }

    #[test]
    fn extract_html_with_inline_images_collects_safe_cid_parts() {
        let eml = b"Content-Type: multipart/related; boundary=B\r\n\r\n\
--B\r\nContent-Type: text/html\r\n\r\n<p><img src=\"cid:logo@example.test\"></p>\r\n\
--B\r\nContent-Type: image/png\r\nContent-ID: <logo@example.test>\r\nContent-Transfer-Encoding: base64\r\n\r\nUE5HREFUQQ==\r\n\
--B\r\nContent-Type: image/svg+xml\r\nContent-ID: <vector@example.test>\r\n\r\n<svg></svg>\r\n\
--B--\r\n";
        let html = extract_html_with_inline_images(eml).expect("html part");
        assert!(html.html.contains("cid:logo@example.test"));
        assert_eq!(html.inline_images.len(), 1);
        assert_eq!(html.inline_images[0].cid, "logo@example.test");
        assert_eq!(html.inline_images[0].content_type, "image/png");
        assert_eq!(html.inline_images[0].data, b"PNGDATA");
    }

    #[test]
    fn mail_preview_parses_headers_and_body_without_network() {
        let eml = b"Subject: =?utf-8?q?Caf=C3=A9_meeting?=\r\n\
                    From: Alice <alice@example.com>\r\n\
                    To: bob@example.com, Carol <carol@example.com>\r\n\
                    Cc: dave@example.com\r\n\
                    Date: Mon, 01 Jun 2026 09:00:00 +0000\r\n\
                    Message-ID: <abc123@example.com>\r\n\
                    Content-Type: text/plain\r\n\r\n\
                    Hello   there\n\nthis is the body.";
        let p = mail_preview(eml);
        assert_eq!(p.subject.as_deref(), Some("Café meeting"));
        assert_eq!(p.from.as_deref(), Some("Alice <alice@example.com>"));
        assert_eq!(p.to, vec!["bob@example.com", "Carol <carol@example.com>"]);
        assert_eq!(p.cc, vec!["dave@example.com"]);
        assert_eq!(p.date.as_deref(), Some("Mon, 01 Jun 2026 09:00:00 +0000"));
        assert_eq!(p.message_id.as_deref(), Some("<abc123@example.com>"));
        assert!(!p.has_html);
        assert_eq!(p.size_bytes, eml.len());
        // body snippet is whitespace-collapsed and excludes the subject line
        assert_eq!(p.body_snippet, "Hello there this is the body.");
    }

    #[test]
    fn mail_preview_flags_html_and_truncates_snippet() {
        let eml = b"Subject: x\r\nContent-Type: text/html\r\n\r\n<p>hello <b>world</b></p>";
        let p = mail_preview(eml);
        assert!(p.has_html);
        assert!(p.body_snippet.contains("hello"));

        let long_body = "word ".repeat(200);
        let eml2 = format!("Subject: y\r\nContent-Type: text/plain\r\n\r\n{long_body}");
        let p2 = mail_preview(eml2.as_bytes());
        assert!(p2.body_snippet.ends_with('…'));
        assert_eq!(p2.body_snippet.chars().count(), 281); // 280 + ellipsis
    }

    #[test]
    fn mail_preview_never_panics_on_garbage() {
        let empty = mail_preview(b"");
        assert_eq!(empty.subject, None);
        assert_eq!(empty.size_bytes, 0);
        assert!(!empty.has_html);
        assert!(empty.to.is_empty());

        let garbage = mail_preview(b"this is not an email at all");
        assert_eq!(garbage.subject, None);
        assert_eq!(garbage.size_bytes, 27);
        assert!(!garbage.has_html);
    }

    #[test]
    fn set_message_id_replaces_existing_and_preserves_body() {
        let eml = b"Subject: Hi\r\nMessage-ID: <old@example.com>\r\nFrom: a@example.com\r\n\r\nbody stays";
        let out = set_message_id(eml, "<new@restore.invalid>");
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("Message-ID: <new@restore.invalid>\r\n"));
        assert!(
            !s.contains("<old@example.com>"),
            "old Message-ID must be gone"
        );
        assert_eq!(
            s.matches("Message-ID:").count(),
            1,
            "exactly one Message-ID"
        );
        assert!(s.contains("Subject: Hi"));
        assert!(s.ends_with("\r\n\r\nbody stays"));
    }

    #[test]
    fn set_message_id_inserts_when_absent_and_handles_garbage() {
        let eml = b"Subject: NoId\r\n\r\nthe body";
        let s = String::from_utf8(set_message_id(eml, "<x@restore.invalid>")).unwrap();
        assert!(s.starts_with("Message-ID: <x@restore.invalid>\r\n"));
        assert_eq!(s.matches("Message-ID:").count(), 1);
        assert!(s.ends_with("\r\n\r\nthe body"));
        // never panics on header-less input
        let _ = set_message_id(b"not an email", "<y@restore.invalid>");
    }
}
