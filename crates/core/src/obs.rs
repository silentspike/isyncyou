//! Observability primitives (#onedrive-mobile 0E): secret-safe structured logging so
//! operation IDs, Graph request-ids, retry counters, encryption failures and ledger
//! states can be traced — without ever leaking a bearer token, refresh token, password
//! or auth code into a log line or a diagnostic export.
//!
//! Redaction is deliberately manual (no regex dependency) and *fail-safe*: it targets the
//! known secret carriers (`Bearer …`, the token/secret JSON+query keys). A value that
//! isn't one of those is left alone — logs stay useful — but every known carrier is
//! blanked. Tests assert no known-secret shape survives.

const REDACTED: &str = "<redacted>";

/// The keys whose *value* is a secret, in JSON (`"key":"…"`) or query (`key=…`) form.
const SECRET_KEYS: &[&str] = &[
    "access_token",
    "refresh_token",
    "id_token",
    "client_secret",
    "password",
    "authorization",
    "code",
    "token",
];

/// Redact secrets from an arbitrary string before it is logged or exported. Blanks
/// `Bearer <token>` and the value of any [`SECRET_KEYS`] entry in JSON or query form.
pub fn redact(input: &str) -> String {
    let mut s = redact_after_marker(input, "Bearer ");
    for key in SECRET_KEYS {
        s = redact_key_value(&s, key);
    }
    s
}

/// Replace the token that follows `marker` (up to the next whitespace) with `<redacted>`.
fn redact_after_marker(input: &str, marker: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find(marker) {
        out.push_str(&rest[..pos + marker.len()]);
        let after = &rest[pos + marker.len()..];
        let end = after
            .find(|c: char| c.is_whitespace())
            .unwrap_or(after.len());
        if end > 0 {
            out.push_str(REDACTED);
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// Replace the value of `key` (case-insensitive), in either `"key":"value"` /
/// `"key": value` (JSON) or `key=value` (query/form) shape, with `<redacted>`. The value
/// ends at the next `"`, `,`, `&`, `}`, `\n` or whitespace.
fn redact_key_value(input: &str, key: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let key_l = key.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if lower[i..].starts_with(&key_l) {
            let after_key = i + key_l.len();
            // Skip an optional closing quote + separator (`:` or `=`) + optional quote/space.
            let mut j = after_key;
            let bytes = input.as_bytes();
            while j < bytes.len() && matches!(bytes[j], b'"' | b' ' | b'\t') {
                j += 1;
            }
            if j < bytes.len() && matches!(bytes[j], b':' | b'=') {
                j += 1;
                while j < bytes.len() && matches!(bytes[j], b'"' | b' ' | b'\t') {
                    j += 1;
                }
                // value runs to the next delimiter
                let mut k = j;
                while k < bytes.len()
                    && !matches!(bytes[k], b'"' | b',' | b'&' | b'}' | b'\n' | b'\r' | b' ')
                {
                    k += 1;
                }
                if k > j {
                    out.push_str(&input[i..j]);
                    out.push_str(REDACTED);
                    i = k;
                    continue;
                }
            }
        }
        // no match here: copy one char (respecting UTF-8 boundaries)
        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Build one secret-safe structured observability line. `op_id` correlates an operation
/// across the ledger and the Graph `request-id`s it produced (a `request-id` carried in
/// `detail` survives; it is a correlation token, not a secret); `detail` is redacted.
pub fn format_event(op_id: &str, event: &str, detail: &str) -> String {
    format!("isyncyou-obs op={op_id} event={event} {}", redact(detail))
}

/// Emit one secret-safe structured observability line (see [`format_event`]) to stderr.
pub fn event(op_id: &str, event: &str, detail: &str) {
    eprintln!("{}", format_event(op_id, event, detail));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_blanks_bearer_and_secret_keys_but_keeps_context() {
        let raw = "GET /x Authorization: Bearer eyJabc.DEF-123_ghi request-id=abc-1 status=200";
        let r = redact(raw);
        assert!(
            !r.contains("eyJabc.DEF-123_ghi"),
            "bearer token must be gone: {r}"
        );
        assert!(
            r.contains("request-id=abc-1"),
            "non-secret context kept: {r}"
        );
        assert!(r.contains("status=200"));
    }

    #[test]
    fn redact_blanks_token_values_in_json_and_query() {
        let json = r#"{"access_token":"SEKRET-AAA","name":"ada","refresh_token":"RRR.bbb"}"#;
        let r = redact(json);
        assert!(
            !r.contains("SEKRET-AAA"),
            "access_token value must be redacted: {r}"
        );
        assert!(
            !r.contains("RRR.bbb"),
            "refresh_token value must be redacted: {r}"
        );
        assert!(r.contains(r#""name":"ada""#), "non-secret field kept: {r}");

        let q = "code=OAUTH-CODE-XYZ&state=s1&client_secret=SHH";
        let rq = redact(q);
        assert!(
            !rq.contains("OAUTH-CODE-XYZ"),
            "auth code must be redacted: {rq}"
        );
        assert!(!rq.contains("SHH"), "client_secret must be redacted: {rq}");
        assert!(rq.contains("state=s1"), "non-secret query kept: {rq}");
    }

    #[test]
    fn redact_leaves_secret_free_text_untouched() {
        let s = "op=op1 event=applied result_id=f1 attempts=1";
        assert_eq!(redact(s), s);
    }

    #[test]
    fn format_event_carries_op_and_request_id_but_no_bearer() {
        // AC1: a structured line correlates op-id + Graph request-id; a bearer that
        // leaks into `detail` is blanked, the request-id (a correlation token) survives.
        let line = format_event(
            "op-42",
            "inflight",
            "request-id=graph-abc-9 Authorization: Bearer eyJLEAKED.tok_123 status=201",
        );
        assert!(line.contains("op=op-42"), "op-id present: {line}");
        assert!(line.contains("event=inflight"), "event present: {line}");
        assert!(
            line.contains("request-id=graph-abc-9"),
            "request-id kept: {line}"
        );
        assert!(
            !line.contains("eyJLEAKED.tok_123"),
            "bearer must be gone: {line}"
        );
    }
}
