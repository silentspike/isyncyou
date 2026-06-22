//! Safe, read-only HTML viewers for archived items (plan §13 HTML-viewer-security,
//! §25 web-UI viewers).
//!
//! These render **our own canonical Graph JSON** (calendar / contacts / todo /
//! onenote) into a fixed HTML skeleton with every value HTML-escaped — so the
//! output is safe *by construction*: no untrusted markup reaches the page, no
//! scripts run, and nothing is fetched (no images, no links followed).
//!
//! A mail message's own `text/html` body is rendered through an **allowlist
//! sanitizer** ([`sanitize_mail_html`], over `ammonia`/`html5ever`): scripts,
//! event handlers and embedded styles are dropped, safe `cid:` inline images are
//! rewritten to `data:` URLs, and external `http(s)` links are rewritten to a
//! local confirmation page. Only `data:`/`mailto:` URLs plus that local dialog URL
//! survive, so remote images (tracking pixels) and `javascript:` links can't load.
//! Any other raw body (or a plain-text mail) is shown as escaped source.
//!
//! Every page is served with a `Content-Security-Policy` ([`VIEWER_CSP`] or
//! [`MAIL_CSP`]) as both a response header and a `<meta>` — a second, independent
//! layer: even if markup slipped through, the browser still loads nothing remote.

use serde_json::Value;
use std::borrow::Cow;

/// Strict CSP for a viewer page: load nothing, run nothing; only the inline
/// stylesheet in the page itself is permitted. `frame-ancestors 'self'` lets the
/// same-origin app shell embed this page in its reading-pane `<iframe>` (the app
/// shell's own CSP is `frame-src 'self'`), while cross-origin framing — the
/// clickjacking vector — stays blocked.
pub const VIEWER_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; img-src 'none'; \
     base-uri 'none'; form-action 'none'; frame-ancestors 'self'";

/// CSP for the sanitized mail viewer: like [`VIEWER_CSP`] but allows **inline
/// `data:` images** (which survive sanitization) while still blocking every
/// remote fetch — so a tracking pixel can never load even if one slipped past
/// the sanitizer. `frame-ancestors 'self'` allows same-origin embedding in the
/// mail reading pane while still denying cross-origin framing.
pub const MAIL_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; img-src data:; \
     base-uri 'none'; form-action 'none'; frame-ancestors 'self'";

/// CSP for the mail viewer **with external content explicitly opted in** (the
/// user pressed "Load external content"): like [`MAIL_CSP`] but `img-src`,
/// `font-src` and `media-src` also allow `https:`/`http:` so the message's own
/// artwork, **web fonts** and media render close to the original. The dangerous
/// capabilities stay blocked: no scripts (`default-src 'none'`, no `script-src`),
/// no `connect`/`form`, no cross-origin framing — only passive resources load.
pub const MAIL_CSP_EXTERNAL: &str =
    "default-src 'none'; style-src 'unsafe-inline'; img-src data: https: http:; \
     font-src data: https: http:; media-src https: http:; \
     base-uri 'none'; form-action 'none'; frame-ancestors 'self'";

/// The CSP that matches [`sanitize_mail_html_with`] for a given `external` flag.
pub fn mail_csp(external: bool) -> &'static str {
    if external {
        MAIL_CSP_EXTERNAL
    } else {
        MAIL_CSP
    }
}

/// Cap on rendered raw-source length, so a pathological message can't produce a
/// multi-megabyte page.
const MAX_SOURCE: usize = 512 * 1024;

/// HTML-escape text for safe insertion into element content **or** a
/// double-quoted attribute (covers `& < > " '`).
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

const STYLE: &str = "body{font:14px/1.5 system-ui,sans-serif;margin:0;background:#1e1f22;color:#e3e3e3}\
.wrap{max-width:760px;margin:0 auto;padding:24px}\
h1{font-size:20px;margin:0 0 4px}\
.kind{color:#9aa0a6;font-size:12px;text-transform:uppercase;letter-spacing:.05em;margin-bottom:16px}\
.row{display:flex;gap:12px;padding:6px 0;border-top:1px solid #2c2d30}\
.k{flex:0 0 140px;color:#9aa0a6}\
.v{flex:1;white-space:pre-wrap;word-break:break-word}\
.body{margin-top:16px;padding:12px;background:#16171a;border-radius:6px;white-space:pre-wrap;word-break:break-word}\
.src{margin-top:16px;padding:12px;background:#16171a;border-radius:6px;overflow:auto;white-space:pre-wrap;word-break:break-word}\
.mail{margin-top:16px;padding:16px;background:#fff;color:#1a1a1a;border-radius:6px;overflow:auto}\
.mail a{color:#1a56db}\
.mail img{max-width:100%}\
.mail *{position:static !important;max-width:100%;overflow-wrap:break-word}\
.mail table{table-layout:fixed}\
.actions{margin-top:16px}.actions a{display:inline-block;padding:8px 10px;border:1px solid #58606f;border-radius:6px;color:#e3e3e3;text-decoration:none}\
.note{margin-top:16px;color:#9aa0a6;font-size:12px}";

/// Wrap rendered inner HTML in a complete, self-contained, locked-down page,
/// embedding `csp` as a `<meta>` CSP (a second layer beside the response header).
pub fn page_with_csp(title: &str, inner: &str, csp: &str) -> String {
    // `frame-ancestors` is ignored when delivered via <meta> (browsers honor it
    // only from the response header), so strip it from the meta copy to avoid a
    // console error. The authoritative CSP — including frame-ancestors — is the
    // response header set by the caller; this meta is the redundant second layer.
    let meta_csp: String = csp
        .split(';')
        .map(str::trim)
        .filter(|d| !d.is_empty() && !d.to_ascii_lowercase().starts_with("frame-ancestors"))
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta http-equiv=\"Content-Security-Policy\" content=\"{csp}\">\
<title>{title}</title><style>{style}</style></head>\
<body><div class=\"wrap\">{inner}</div></body></html>",
        csp = escape(&meta_csp),
        title = escape(title),
        style = STYLE,
        inner = inner,
    )
}

/// Wrap rendered inner HTML in a locked-down page (strict [`VIEWER_CSP`]).
pub fn page(title: &str, inner: &str) -> String {
    page_with_csp(title, inner, VIEWER_CSP)
}

/// Render a header (title + kind label).
fn header(kind: &str, title: &str) -> String {
    format!(
        "<h1>{}</h1><div class=\"kind\">{}</div>",
        escape(title),
        escape(kind)
    )
}

/// Render a labelled row if `value` is non-empty.
fn row(label: &str, value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    format!(
        "<div class=\"row\"><span class=\"k\">{}</span><span class=\"v\">{}</span></div>",
        escape(label),
        escape(value)
    )
}

/// Top-level string field.
fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// `v[outer][inner]` string.
fn s2<'a>(v: &'a Value, outer: &str, inner: &str) -> &'a str {
    v.get(outer)
        .and_then(|o| o.get(inner))
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// Render a Graph `dateTime`/`timeZone` pair as `"<dateTime> (<tz>)"`.
fn datetime(v: &Value, key: &str) -> String {
    let dt = s2(v, key, "dateTime");
    if dt.is_empty() {
        return String::new();
    }
    let tz = s2(v, key, "timeZone");
    if tz.is_empty() {
        dt.to_string()
    } else {
        format!("{dt} ({tz})")
    }
}

/// One `emailAddress` object → `"Name <addr>"` / `"addr"` / `""`.
fn email_addr(v: &Value) -> String {
    let ea = v.get("emailAddress").unwrap_or(v);
    let name = ea.get("name").and_then(Value::as_str).unwrap_or("");
    let addr = ea.get("address").and_then(Value::as_str).unwrap_or("");
    match (name.is_empty(), addr.is_empty()) {
        (_, true) => name.to_string(),
        (true, false) => addr.to_string(),
        (false, false) if name == addr => addr.to_string(),
        (false, false) => format!("{name} <{addr}>"),
    }
}

/// Join an array field of `emailAddress`-bearing objects / strings.
fn join_emails(v: &Value, key: &str) -> String {
    match v.get(key).and_then(Value::as_array) {
        Some(arr) => arr
            .iter()
            .map(email_addr)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", "),
        None => String::new(),
    }
}

/// Join an array of plain strings.
fn join_strings(v: &Value, key: &str) -> String {
    match v.get(key).and_then(Value::as_array) {
        Some(arr) => arr
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", "),
        None => String::new(),
    }
}

/// A free-text body block (e.g. event/task `bodyPreview`), escaped.
fn body_block(text: &str) -> String {
    if text.trim().is_empty() {
        String::new()
    } else {
        format!("<div class=\"body\">{}</div>", escape(text))
    }
}

/// Render one archived item's canonical JSON for `service` as safe HTML.
pub fn render_item(service: &str, item: &Value) -> String {
    let inner = match service {
        "calendar" => render_event(item),
        "contacts" => render_contact(item),
        "todo" => render_task(item),
        "onenote" => render_page(item),
        _ => render_generic(item),
    };
    let title = first_nonempty(&[s(item, "subject"), s(item, "title"), s(item, "displayName")])
        .unwrap_or("Item");
    page(title, &inner)
}

fn first_nonempty<'a>(candidates: &[&'a str]) -> Option<&'a str> {
    candidates.iter().copied().find(|c| !c.is_empty())
}

fn render_event(v: &Value) -> String {
    let mut out = header("Calendar event", s(v, "subject"));
    out.push_str(&row("Start", &datetime(v, "start")));
    out.push_str(&row("End", &datetime(v, "end")));
    if v.get("isAllDay").and_then(Value::as_bool) == Some(true) {
        out.push_str(&row("All day", "yes"));
    }
    if v.get("isCancelled").and_then(Value::as_bool) == Some(true) {
        out.push_str(&row("Cancelled", "yes"));
    }
    out.push_str(&row("Location", s2(v, "location", "displayName")));
    out.push_str(&row(
        "Organizer",
        &email_addr(v.get("organizer").unwrap_or(v)),
    ));
    out.push_str(&row("Attendees", &join_emails(v, "attendees")));
    out.push_str(&body_block(s(v, "bodyPreview")));
    out
}

fn render_contact(v: &Value) -> String {
    let title = first_nonempty(&[s(v, "displayName"), s(v, "givenName")]).unwrap_or("Contact");
    let mut out = header("Contact", title);
    out.push_str(&row("Company", s(v, "companyName")));
    out.push_str(&row("Job title", s(v, "jobTitle")));
    out.push_str(&row("Department", s(v, "department")));
    out.push_str(&row("Email", &join_emails(v, "emailAddresses")));
    out.push_str(&row("Mobile", s(v, "mobilePhone")));
    out.push_str(&row("Business", &join_strings(v, "businessPhones")));
    out.push_str(&row("Home", &join_strings(v, "homePhones")));
    out
}

fn render_task(v: &Value) -> String {
    let mut out = header("Task", s(v, "title"));
    out.push_str(&row("Status", s(v, "status")));
    out.push_str(&row("Importance", s(v, "importance")));
    out.push_str(&row("Due", &datetime(v, "dueDateTime")));
    out.push_str(&row("Completed", &datetime(v, "completedDateTime")));
    out.push_str(&row("Created", &datetime(v, "createdDateTime")));
    // task body is text/plain in canonical JSON; show its content escaped.
    out.push_str(&body_block(s2(v, "body", "content")));
    out
}

fn render_page(v: &Value) -> String {
    let mut out = header("OneNote page", s(v, "title"));
    out.push_str(&row("Section", s2(v, "parentSection", "displayName")));
    out.push_str(&row("Notebook", s2(v, "parentNotebook", "displayName")));
    out.push_str(&row("Created", s(v, "createdDateTime")));
    out.push_str(&row("Modified", s(v, "lastModifiedDateTime")));
    out.push_str(
        "<div class=\"note\">Page HTML + resources are archived separately; \
        open the archived body for the page content.</div>",
    );
    out
}

/// Fallback: render every top-level scalar field as a row.
fn render_generic(v: &Value) -> String {
    let title = first_nonempty(&[
        s(v, "subject"),
        s(v, "title"),
        s(v, "displayName"),
        s(v, "name"),
    ])
    .unwrap_or("Item");
    let mut out = header("Item", title);
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            let text = match val {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => continue, // skip nested objects/arrays in the generic view
            };
            out.push_str(&row(k, &text));
        }
    }
    out
}

/// Render a raw (non-JSON) body — e.g. an `.eml` message — as **escaped source**.
/// Nothing is interpreted as markup; it is shown verbatim and inert.
pub fn source_page(service: &str, raw: &str) -> String {
    let capped = if raw.len() > MAX_SOURCE {
        &raw[..floor_char_boundary(raw, MAX_SOURCE)]
    } else {
        raw
    };
    let inner = format!(
        "{header}<div class=\"note\">Raw archived source — shown inert (never rendered). \
A sanitized HTML mail viewer is separate follow-up work.</div>\
<div class=\"src\">{body}</div>",
        header = header(&format!("{service} source"), "Archived message"),
        body = escape(capped),
    );
    page("Source", &inner)
}

/// `str::floor_char_boundary` is unstable; this is the same idea on stable.
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// A safe inline image that can replace a matching `cid:` URL.
#[derive(Debug, Clone, Copy)]
pub struct InlineImageRef<'a> {
    pub cid: &'a str,
    pub content_type: &'a str,
    pub data: &'a [u8],
}

/// Sanitize a mail message's own `text/html` body into safe, inert markup.
///
/// Uses the `ammonia` allowlist sanitizer (battle-tested over `html5ever`):
/// scripts, event handlers, `<iframe>`/`<object>`/`<style>` etc. are dropped, and
/// — via the restricted scheme set — **only `data:`/`mailto:` URLs and the local
/// external-link dialog URL survive**, so remote images (tracking pixels),
/// unresolved `cid:` images, `javascript:` links, and remote stylesheets cannot
/// load. Links keep their text but get `rel=noopener…`.
pub fn sanitize_mail_html(raw: &str) -> String {
    sanitize_mail_html_with(raw, false)
}

/// Like [`sanitize_mail_html`] but, for fidelity to the original message, **keeps
/// the message's own CSS** — inline `style="…"` attributes and `<style>` blocks
/// survive (they are layout, not behaviour). When `external` is set, `http(s)` is
/// added to the allowed URL schemes so `<img src="https://…">` survives and the
/// relaxed CSP ([`MAIL_CSP_EXTERNAL`]) also lets the CSS pull web fonts/media;
/// otherwise only `data:` resources survive, exactly as before. Scripts/event
/// handlers are always dropped, so even with external content nothing executes.
pub fn sanitize_mail_html_with(raw: &str, external: bool) -> String {
    let mut schemes: std::collections::HashSet<&str> = ["data", "mailto"].into_iter().collect();
    if external {
        schemes.insert("https");
        schemes.insert("http");
    }
    let mut b = ammonia::Builder::default();
    b.url_schemes(schemes)
        .url_relative(ammonia::UrlRelative::Custom(Box::new(
            allow_mail_relative_url,
        )))
        .link_rel(Some("noopener noreferrer nofollow"))
        // keep the message's styling for a faithful render (inert under the CSP)
        .add_tags(["style"])
        .rm_clean_content_tags(["style"])
        .add_generic_attributes(["style"]);
    b.clean(raw).to_string()
}

/// Render a mail message's `text/html` body as a safe, CSP-locked page. Safe
/// archived `cid:` image references are first mapped to `data:` URLs; unmapped
/// `cid:` references are stripped by [`sanitize_mail_html`]. `subject` is shown
/// escaped as the heading.
pub fn mail_page_with_inline_images(
    subject: &str,
    raw_html: &str,
    inline_images: &[InlineImageRef<'_>],
    external: bool,
) -> String {
    let rewritten = rewrite_external_links(&inline_cid_images(raw_html, inline_images));
    let clean = sanitize_mail_html_with(&rewritten, external);
    let note = if external {
        "Sanitized view — scripts removed; external content (images, fonts) is loaded at your \
         request. External links open through a confirmation page. Use \u{201c}raw\u{201d} for \
         the original source."
    } else {
        "Sanitized view — scripts removed; external content (images, fonts) is blocked. External \
         links open through a confirmation page. Use \u{201c}raw\u{201d} for the original source."
    };
    let inner = format!(
        "{header}<div class=\"note\">{note}</div><div class=\"mail\">{clean}</div>",
        header = header("Mail", subject),
    );
    page_with_csp(subject, &inner, mail_csp(external))
}

/// Render an archived OneNote page's raw HTML as a sanitized, CSP-locked page
/// (same `ammonia` allowlist as mail: scripts removed, remote resources blocked,
/// external links routed through the confirmation page). OneNote bodies are plain
/// HTML — no MIME/cid handling — so this is the mail path minus inline images.
pub fn note_page(title: &str, raw_html: &str) -> String {
    let clean = sanitize_mail_html(&rewrite_external_links(raw_html));
    let inner = format!(
        "{header}<div class=\"note\">Sanitized view — scripts removed; external images are \
         blocked. External links open through a confirmation page.</div>\
<div class=\"mail\">{clean}</div>",
        header = header("OneNote", title),
    );
    page_with_csp(title, &inner, MAIL_CSP)
}

/// Render the explicit interstitial used before leaving the local viewer for a
/// URL that came from archived mail. Nothing is fetched or opened automatically.
pub fn external_link_dialog_page(url: &str) -> Option<String> {
    if !is_safe_external_url(url) {
        return None;
    }
    let inner = format!(
        "{header}<div class=\"note\">This link came from archived mail and was not opened \
         automatically.</div><div class=\"src\">{url}</div>\
<p class=\"actions\"><a href=\"{href}\" rel=\"noopener noreferrer nofollow\">Open external link</a></p>",
        header = header("External link", "Open external link?"),
        url = escape(url),
        href = escape(url),
    );
    Some(page_with_csp("Open external link", &inner, VIEWER_CSP))
}

pub fn is_safe_external_url(url: &str) -> bool {
    if url.is_empty() || url.len() > 4096 || url.trim() != url {
        return false;
    }
    if url
        .bytes()
        .any(|b| b < 0x20 || b == 0x7f || b == b'<' || b == b'>' || b == b'"')
    {
        return false;
    }
    let lower = url.to_ascii_lowercase();
    let rest = if let Some(rest) = lower.strip_prefix("https://") {
        rest
    } else if let Some(rest) = lower.strip_prefix("http://") {
        rest
    } else {
        return false;
    };
    !rest.is_empty() && !matches!(rest.as_bytes()[0], b'/' | b'?' | b'#')
}

fn rewrite_external_links(raw_html: &str) -> String {
    let mut out = String::with_capacity(raw_html.len());
    let mut cursor = 0;
    while let Some(rel) = find_ascii_case_insensitive(&raw_html[cursor..], "<a") {
        let tag_start = cursor + rel;
        out.push_str(&raw_html[cursor..tag_start]);
        let Some(tag_end) = find_tag_end(raw_html, tag_start) else {
            out.push_str(&raw_html[tag_start..]);
            return out;
        };
        let tag = &raw_html[tag_start..tag_end];
        if is_anchor_tag(tag) {
            out.push_str(&rewrite_anchor_tag(tag));
        } else {
            out.push_str(tag);
        }
        cursor = tag_end;
    }
    out.push_str(&raw_html[cursor..]);
    out
}

fn is_anchor_tag(tag: &str) -> bool {
    matches!(
        tag.as_bytes().get(2).copied(),
        Some(b'>') | Some(b'/') | Some(b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
    )
}

fn find_tag_end(s: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    for (off, ch) in s[start..].char_indices() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (None, '"' | '\'') => quote = Some(ch),
            (None, '>') => return Some(start + off + 1),
            _ => {}
        }
    }
    None
}

fn rewrite_anchor_tag(tag: &str) -> String {
    let bytes = tag.as_bytes();
    let mut i = 2;
    while i + 4 <= bytes.len() {
        if ascii_eq_ignore_case(&bytes[i..i + 4], b"href")
            && (i == 0 || !is_attr_name_byte(bytes[i - 1]))
        {
            let mut j = i + 4;
            while j < bytes.len() && is_ascii_ws(bytes[j]) {
                j += 1;
            }
            if bytes.get(j) != Some(&b'=') {
                i += 1;
                continue;
            }
            j += 1;
            while j < bytes.len() && is_ascii_ws(bytes[j]) {
                j += 1;
            }
            if j >= bytes.len() {
                break;
            }
            let (value_start, value_end) = if matches!(bytes[j], b'"' | b'\'') {
                let quote = bytes[j];
                let value_start = j + 1;
                let value_end = bytes[value_start..]
                    .iter()
                    .position(|b| *b == quote)
                    .map(|p| value_start + p)
                    .unwrap_or(bytes.len());
                (value_start, value_end)
            } else {
                let value_start = j;
                let value_end = bytes[value_start..]
                    .iter()
                    .position(|b| is_ascii_ws(*b) || *b == b'>')
                    .map(|p| value_start + p)
                    .unwrap_or(bytes.len());
                (value_start, value_end)
            };
            let value = &tag[value_start..value_end];
            if is_safe_external_url(value) {
                let mut rewritten = String::with_capacity(tag.len() + value.len());
                rewritten.push_str(&tag[..value_start]);
                rewritten.push_str(&external_dialog_href(value));
                rewritten.push_str(&tag[value_end..]);
                return rewritten;
            }
            i = value_end.saturating_add(1);
        } else {
            i += 1;
        }
    }
    tag.to_string()
}

fn allow_mail_relative_url(url: &str) -> Option<Cow<'_, str>> {
    local_external_dialog_target(url).map(|_| Cow::Borrowed(url))
}

fn local_external_dialog_target(url: &str) -> Option<String> {
    let query = url.strip_prefix("/api/v1/open-external?")?;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "url" {
            let decoded = percent_decode_query(value)?;
            if is_safe_external_url(&decoded) {
                return Some(decoded);
            }
        }
    }
    None
}

fn external_dialog_href(url: &str) -> String {
    format!(
        "/api/v1/open-external?url={}",
        percent_encode_component(url)
    )
}

fn percent_decode_query(value: &str) -> Option<String> {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_value(bytes[i + 1])?;
                let lo = hex_value(bytes[i + 2])?;
                out.push((hi << 4) | lo);
                i += 2;
            }
            b'%' => return None,
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

fn percent_encode_component(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|w| ascii_eq_ignore_case(w, needle.as_bytes()))
}

fn ascii_eq_ignore_case(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right.iter())
            .all(|(l, r)| l.eq_ignore_ascii_case(r))
}

fn is_attr_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-')
}

fn is_ascii_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn inline_cid_images(raw_html: &str, inline_images: &[InlineImageRef<'_>]) -> String {
    let mut out = raw_html.to_string();
    for img in inline_images {
        if !safe_inline_image_type(img.content_type) || img.data.is_empty() {
            continue;
        }
        let data_url = format!(
            "data:{};base64,{}",
            img.content_type,
            base64_encode(img.data)
        );
        let cid = normalize_cid(img.cid);
        for target in cid_url_variants(&cid) {
            out = out.replace(&target, &data_url);
        }
    }
    out
}

fn normalize_cid(cid: &str) -> String {
    cid.trim()
        .trim_start_matches("cid:")
        .trim_start_matches("CID:")
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_string()
}

fn cid_url_variants(cid: &str) -> Vec<String> {
    let escaped = percent_encode_cid(cid);
    let mut out = vec![format!("cid:{cid}"), format!("CID:{cid}")];
    if escaped != cid {
        out.push(format!("cid:{escaped}"));
        out.push(format!("CID:{escaped}"));
    }
    out
}

fn percent_encode_cid(cid: &str) -> String {
    let mut out = String::new();
    for b in cid.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn safe_inline_image_type(content_type: &str) -> bool {
    matches!(
        content_type,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0b0000_0011) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0b0000_1111) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0b0011_1111) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn escape_neutralizes_markup_and_quotes() {
        assert_eq!(
            escape(r#"<script>alert("x")&'</script>"#),
            "&lt;script&gt;alert(&quot;x&quot;)&amp;&#39;&lt;/script&gt;"
        );
    }

    #[test]
    fn event_renders_fields_and_escapes_values() {
        let ev = json!({
            "subject": "Lunch <b>plan</b>",
            "start": {"dateTime": "2026-06-02T12:00:00", "timeZone": "Europe/Vienna"},
            "end": {"dateTime": "2026-06-02T13:00:00", "timeZone": "Europe/Vienna"},
            "location": {"displayName": "Cafe & Co"},
            "organizer": {"emailAddress": {"name": "Ann", "address": "ann@x.com"}},
            "attendees": [{"emailAddress": {"name": "Bo", "address": "bo@x.com"}}],
            "bodyPreview": "see you <there>"
        });
        let html = render_item("calendar", &ev);
        assert!(html.contains("Calendar event"));
        assert!(html.contains("Europe/Vienna"));
        assert!(html.contains("Ann &lt;ann@x.com&gt;"));
        assert!(html.contains("Cafe &amp; Co"));
        // the malicious subject is escaped, never live markup
        assert!(html.contains("Lunch &lt;b&gt;plan&lt;/b&gt;"));
        assert!(!html.contains("<b>plan</b>"));
        assert!(html.contains("see you &lt;there&gt;"));
    }

    #[test]
    fn contact_joins_emails_and_phones() {
        let c = json!({
            "displayName": "Carl Customer",
            "companyName": "Acme",
            "emailAddresses": [{"address": "carl@acme.test"}, {"name": "Alt", "address": "c@x.io"}],
            "businessPhones": ["+1 555 0100", "+1 555 0101"]
        });
        let html = render_item("contacts", &c);
        assert!(html.contains("Carl Customer"));
        assert!(html.contains("carl@acme.test, Alt &lt;c@x.io&gt;"));
        assert!(html.contains("+1 555 0100, +1 555 0101"));
    }

    #[test]
    fn task_shows_status_due_and_body() {
        let t = json!({
            "title": "File taxes",
            "status": "notStarted",
            "dueDateTime": {"dateTime": "2026-04-30T00:00:00", "timeZone": "UTC"},
            "body": {"content": "don't forget", "contentType": "text"}
        });
        let html = render_item("todo", &t);
        assert!(html.contains("File taxes"));
        assert!(html.contains("notStarted"));
        assert!(html.contains("2026-04-30T00:00:00 (UTC)"));
        assert!(html.contains("don&#39;t forget"));
    }

    #[test]
    fn generic_renders_scalars_and_skips_nested() {
        let v = json!({ "name": "x", "count": 3, "flag": true, "nested": {"a": 1} });
        let html = render_item("unknown-service", &v);
        assert!(html.contains("count"));
        assert!(html.contains(">3<"));
        assert!(html.contains("flag"));
        // nested object is skipped, not dumped
        assert!(!html.contains("nested"));
    }

    #[test]
    fn source_page_escapes_and_caps() {
        let html = source_page("mail", "From: a@b\r\n<script>evil()</script>");
        assert!(html.contains("&lt;script&gt;evil()&lt;/script&gt;"));
        assert!(!html.contains("<script>evil"));
        // capping a huge body stays on a char boundary and does not panic
        let big = "ä".repeat(MAX_SOURCE); // 2 bytes each -> well over the cap
        let _ = source_page("mail", &big);
    }

    #[test]
    fn page_embeds_csp_meta() {
        let p = page("T", "<p>x</p>");
        assert!(p.contains("Content-Security-Policy"));
        assert!(p.contains("default-src &#39;none&#39;"));
    }

    #[test]
    fn sanitize_strips_scripts_and_blocks_remote_resources() {
        let dirty = "<p>Hi</p><script>steal()</script>\
<img src=\"https://tracker.example/p.gif\">\
<img src=\"cid:not-archived@example.test\">\
<a href=\"https://evil.test\" onclick=\"x()\">link</a>\
<img src=\"data:image/png;base64,AAAA\">";
        let clean = sanitize_mail_html(dirty);
        assert!(clean.contains("<p>Hi</p>"), "safe markup lost: {clean}");
        assert!(!clean.contains("steal"), "script body survived: {clean}");
        assert!(
            !clean.contains("onclick"),
            "event handler survived: {clean}"
        );
        assert!(
            !clean.contains("https://tracker"),
            "remote img src survived: {clean}"
        );
        assert!(
            !clean.contains("https://evil"),
            "remote href survived: {clean}"
        );
        assert!(
            !clean.contains("cid:not-archived"),
            "unresolved cid survived: {clean}"
        );
        assert!(clean.contains("link"), "link text should remain: {clean}");
        assert!(
            clean.contains("data:image/png"),
            "inline data image should survive: {clean}"
        );
    }

    #[test]
    fn sanitize_keeps_styling_for_fidelity() {
        // inline style + <style> block survive (layout), but script/event handlers don't
        let dirty = "<style>.x{color:red}</style>\
<table style=\"width:600px;background:#fff\"><tr><td style=\"padding:12px\">Hi</td></tr></table>\
<script>steal()</script>";
        let clean = sanitize_mail_html(dirty);
        assert!(clean.contains("<style>"), "<style> block dropped: {clean}");
        assert!(
            clean.contains(".x{color:red}"),
            "css content dropped: {clean}"
        );
        assert!(
            clean.contains("style=\"width:600px;background:#fff\""),
            "inline style dropped: {clean}"
        );
        assert!(
            clean.contains("padding:12px"),
            "inline style dropped: {clean}"
        );
        assert!(!clean.contains("steal"), "script survived: {clean}");
    }

    #[test]
    fn external_content_opt_in_keeps_src_and_uses_relaxed_csp() {
        let html = "<p><img src=\"https://cdn.example/hero.png\"></p>";
        // default: stripped + strict CSP (no remote images, no remote fonts)
        let off = mail_page_with_inline_images("S", html, &[], false);
        assert!(
            !off.contains("https://cdn.example"),
            "remote img leaked: {off}"
        );
        assert!(off.contains("img-src data:;"), "strict CSP missing: {off}");
        assert!(
            !off.contains("font-src https:"),
            "fonts should be blocked by default: {off}"
        );
        // opted in: src survives + relaxed CSP allows https images AND web fonts
        let on = mail_page_with_inline_images("S", html, &[], true);
        assert!(
            on.contains("https://cdn.example/hero.png"),
            "remote img should survive when opted in: {on}"
        );
        assert!(
            on.contains("img-src data: https: http:"),
            "relaxed img CSP missing: {on}"
        );
        assert!(
            on.contains("font-src data: https: http:"),
            "relaxed font CSP missing: {on}"
        );
        // links are still routed through the dialog regardless of the image opt-in
        let links =
            mail_page_with_inline_images("S", "<a href=\"https://evil.test/x\">go</a>", &[], true);
        assert!(
            !links.contains("href=\"https://evil.test"),
            "direct external href survived even with images on: {links}"
        );
    }

    #[test]
    fn note_page_sanitizes_and_is_csp_locked() {
        let p = note_page(
            "My Note",
            "<h1>Heading</h1><p>text</p><script>steal()</script>\
<img src=\"https://tracker.example/p.gif\">",
        );
        assert!(p.contains("<h1>Heading</h1>"), "safe markup lost: {p}");
        assert!(!p.contains("steal"), "script survived: {p}");
        assert!(!p.contains("https://tracker"), "remote img survived: {p}");
        assert!(p.contains("My Note"), "title missing: {p}");
        // carries the strict mail CSP (escaped in the <meta>)
        assert!(p.contains("default-src &#39;none&#39;"), "no CSP: {p}");
    }

    #[test]
    fn mail_page_rewrites_external_links_to_confirm_dialog() {
        let p = mail_page_with_inline_images(
            "Links",
            "<p><a href=\"https://example.test/path?q=1&x=2\" onclick=\"x()\">open</a>\
<img src=\"https://tracker.example/p.gif\"></p>",
            &[],
            false,
        );
        assert!(
            p.contains(
                "/api/v1/open-external?url=https%3A%2F%2Fexample.test%2Fpath%3Fq%3D1%26x%3D2"
            ),
            "external link was not rewritten to the local dialog: {p}"
        );
        assert!(
            !p.contains("href=\"https://example.test"),
            "direct external href survived: {p}"
        );
        assert!(!p.contains("onclick"), "event handler survived: {p}");
        assert!(
            !p.contains("https://tracker"),
            "remote image source survived: {p}"
        );
    }

    #[test]
    fn external_link_dialog_page_requires_safe_http_url() {
        let page = external_link_dialog_page("https://example.test/a?x=1&y=2").unwrap();
        assert!(
            page.contains("Open external link?"),
            "heading missing: {page}"
        );
        assert!(
            page.contains("href=\"https://example.test/a?x=1&amp;y=2\""),
            "escaped outbound link missing: {page}"
        );
        assert!(
            page.contains("default-src &#39;none&#39;"),
            "dialog page should carry strict meta CSP: {page}"
        );
        assert!(external_link_dialog_page("javascript:alert(1)").is_none());
        assert!(external_link_dialog_page("/api/v1/accounts").is_none());
        assert!(external_link_dialog_page("https://example.test/\nnext").is_none());
    }

    #[test]
    fn mail_page_is_csp_locked_and_shows_escaped_subject() {
        let p = mail_page_with_inline_images("Hello <there>", "<p>body</p>", &[], false);
        assert!(
            p.contains("Hello &lt;there&gt;"),
            "subject not escaped: {p}"
        );
        assert!(p.contains("img-src data:"), "mail CSP missing: {p}");
        assert!(p.contains("<p>body</p>"), "sanitized body missing");
        assert!(!p.contains("<script"), "no scripts in a mail page");
    }

    #[test]
    fn mail_page_maps_cid_images_to_data_urls() {
        let img = InlineImageRef {
            cid: "logo@example.test",
            content_type: "image/png",
            data: b"PNGDATA",
        };
        let p = mail_page_with_inline_images(
            "Inline",
            "<p><img src=\"cid:logo@example.test\"><img src=\"cid:missing@example.test\"></p>",
            &[img],
            false,
        );
        assert!(
            p.contains("data:image/png;base64,UE5HREFUQQ=="),
            "cid image was not embedded: {p}"
        );
        assert!(!p.contains("cid:logo"), "resolved cid leaked: {p}");
        assert!(!p.contains("cid:missing"), "unresolved cid leaked: {p}");
        assert!(p.contains("img-src data:"), "mail CSP missing: {p}");
    }
}
