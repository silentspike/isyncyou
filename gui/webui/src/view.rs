//! Safe, read-only HTML viewers for archived items (plan §13 HTML-viewer-security,
//! §25 web-UI viewers).
//!
//! These render **our own canonical Graph JSON** (calendar / contacts / todo /
//! onenote) into a fixed HTML skeleton with every value HTML-escaped — so the
//! output is safe *by construction*: no untrusted markup reaches the page, no
//! scripts run, and nothing is fetched (no images, no links followed). A raw
//! `.eml` (or any non-JSON body) is shown as escaped source, never rendered.
//!
//! The pages are meant to be served with [`VIEWER_CSP`] as a `Content-Security-
//! Policy` header (the router does this), which is a second, independent layer:
//! even if a value somehow carried markup, the browser would still load nothing.
//!
//! A *rich* HTML mail renderer (parse + allowlist-sanitize the message's own
//! `text/html` body) is deliberately out of scope here: doing it safely needs a
//! battle-tested HTML-sanitizer dependency (e.g. `ammonia`/`html5ever`), an
//! architectural addition to this otherwise dependency-light crate. Until then,
//! mail is viewable as inert source via this viewer and as inert bytes via
//! `/api/v1/body`.

use serde_json::Value;

/// Strict CSP for a viewer page: load nothing, run nothing; only the inline
/// stylesheet in the page itself is permitted.
pub const VIEWER_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; img-src 'none'; \
     base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

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
.note{margin-top:16px;color:#9aa0a6;font-size:12px}";

/// Wrap rendered inner HTML in a complete, self-contained, locked-down page.
pub fn page(title: &str, inner: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta http-equiv=\"Content-Security-Policy\" content=\"{csp}\">\
<title>{title}</title><style>{style}</style></head>\
<body><div class=\"wrap\">{inner}</div></body></html>",
        csp = escape(VIEWER_CSP),
        title = escape(title),
        style = STYLE,
        inner = inner,
    )
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
}
