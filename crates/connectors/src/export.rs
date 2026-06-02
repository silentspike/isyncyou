//! Export archived canonical JSON to interchange formats (plan §6): calendar
//! events → iCalendar (`.ics`), contacts → vCard (`.vcf`).
//!
//! These are **lossy exports** for importing elsewhere — the canonical record
//! stays the archived JSON. Pure functions over a Graph item's JSON, so they are
//! fully unit-testable.

use serde_json::Value;

/// Escape a value for an iCalendar text field (RFC 5545 §3.3.11).
fn ics_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace('\n', "\\n")
        .replace('\r', "")
}

/// Compact a Graph `dateTime` (`2026-03-01T09:00:00...`) to an iCalendar
/// floating local timestamp (`20260301T090000`).
fn ics_datetime(dt: &str) -> String {
    let core: String = dt
        .chars()
        .take_while(|&c| c != '.' && c != '+' && c != 'Z')
        .filter(|c| c.is_ascii_digit() || *c == 'T')
        .collect();
    core
}

/// Render a Graph calendar event as a single-VEVENT iCalendar document.
pub fn event_to_ics(event: &Value) -> String {
    let g = |p: &str| event.pointer(p).and_then(Value::as_str).unwrap_or("");
    let uid = {
        let u = g("/iCalUId");
        if u.is_empty() {
            g("/id")
        } else {
            u
        }
    };
    let subject = ics_escape(g("/subject"));
    let location = ics_escape(g("/location/displayName"));
    let body = ics_escape(g("/bodyPreview"));
    let start = ics_datetime(g("/start/dateTime"));
    let end = ics_datetime(g("/end/dateTime"));
    let stamp = ics_datetime(g("/lastModifiedDateTime"));

    let mut out = String::from(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//iSyncYou//EN\r\nBEGIN:VEVENT\r\n",
    );
    out.push_str(&format!("UID:{}\r\n", ics_escape(uid)));
    if !stamp.is_empty() {
        out.push_str(&format!("DTSTAMP:{stamp}\r\n"));
    }
    if !start.is_empty() {
        out.push_str(&format!("DTSTART:{start}\r\n"));
    }
    if !end.is_empty() {
        out.push_str(&format!("DTEND:{end}\r\n"));
    }
    out.push_str(&format!("SUMMARY:{subject}\r\n"));
    if !location.is_empty() {
        out.push_str(&format!("LOCATION:{location}\r\n"));
    }
    if !body.is_empty() {
        out.push_str(&format!("DESCRIPTION:{body}\r\n"));
    }
    out.push_str("END:VEVENT\r\nEND:VCALENDAR\r\n");
    out
}

/// Escape a value for a vCard text field (RFC 6350 §3.4).
fn vcard_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace('\n', "\\n")
        .replace('\r', "")
}

/// Render a Graph contact as a vCard 3.0 document.
pub fn contact_to_vcard(contact: &Value) -> String {
    let g = |k: &str| contact.get(k).and_then(Value::as_str).unwrap_or("");
    let given = g("givenName");
    let sur = g("surname");
    let display = {
        let d = g("displayName");
        if d.is_empty() {
            format!("{given} {sur}").trim().to_string()
        } else {
            d.to_string()
        }
    };

    let mut out = String::from("BEGIN:VCARD\r\nVERSION:3.0\r\n");
    out.push_str(&format!("FN:{}\r\n", vcard_escape(&display)));
    out.push_str(&format!(
        "N:{};{};;;\r\n",
        vcard_escape(sur),
        vcard_escape(given)
    ));
    // first email / phone, then company + title
    if let Some(addr) = contact
        .get("emailAddresses")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|e| e.get("address"))
        .and_then(Value::as_str)
    {
        out.push_str(&format!("EMAIL:{}\r\n", vcard_escape(addr)));
    }
    let phone = {
        let m = g("mobilePhone");
        if !m.is_empty() {
            m.to_string()
        } else {
            contact
                .get("businessPhones")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        }
    };
    if !phone.is_empty() {
        out.push_str(&format!("TEL:{}\r\n", vcard_escape(&phone)));
    }
    if !g("companyName").is_empty() {
        out.push_str(&format!("ORG:{}\r\n", vcard_escape(g("companyName"))));
    }
    if !g("jobTitle").is_empty() {
        out.push_str(&format!("TITLE:{}\r\n", vcard_escape(g("jobTitle"))));
    }
    out.push_str("END:VCARD\r\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn event_renders_valid_vevent() {
        let ev = json!({
            "id": "EVID", "iCalUId": "040000ABCD",
            "subject": "Q2 review; with team",
            "bodyPreview": "agenda, line1\nline2",
            "location": { "displayName": "Room 1" },
            "start": { "dateTime": "2026-03-01T09:00:00.0000000", "timeZone": "UTC" },
            "end": { "dateTime": "2026-03-01T10:30:00.0000000", "timeZone": "UTC" },
            "lastModifiedDateTime": "2026-02-20T08:00:00Z"
        });
        let ics = event_to_ics(&ev);
        assert!(ics.starts_with("BEGIN:VCALENDAR\r\nVERSION:2.0"));
        assert!(ics.contains("BEGIN:VEVENT") && ics.contains("END:VEVENT"));
        assert!(ics.contains("UID:040000ABCD"));
        assert!(ics.contains("DTSTART:20260301T090000"));
        assert!(ics.contains("DTEND:20260301T103000"));
        // escaping: ';' and ',' and '\n'
        assert!(ics.contains("SUMMARY:Q2 review\\; with team"));
        assert!(ics.contains("DESCRIPTION:agenda\\, line1\\nline2"));
        assert!(ics.contains("LOCATION:Room 1"));
        assert!(ics.trim_end().ends_with("END:VCALENDAR"));
    }

    #[test]
    fn event_uid_falls_back_to_id() {
        let ev = json!({ "id": "ONLYID", "subject": "x", "start": {"dateTime":"2026-01-01T00:00:00"}, "end": {"dateTime":"2026-01-01T01:00:00"} });
        assert!(event_to_ics(&ev).contains("UID:ONLYID"));
    }

    #[test]
    fn contact_renders_valid_vcard() {
        let c = json!({
            "displayName": "Ada Lovelace",
            "givenName": "Ada", "surname": "Lovelace",
            "emailAddresses": [{ "address": "ada@example.com", "name": "Ada" }],
            "mobilePhone": "+1 555 0100",
            "companyName": "Analytical Engines; Ltd",
            "jobTitle": "Mathematician"
        });
        let v = contact_to_vcard(&c);
        assert!(v.starts_with("BEGIN:VCARD\r\nVERSION:3.0"));
        assert!(v.contains("FN:Ada Lovelace"));
        assert!(v.contains("N:Lovelace;Ada;;;"));
        assert!(v.contains("EMAIL:ada@example.com"));
        assert!(v.contains("TEL:+1 555 0100"));
        assert!(v.contains("ORG:Analytical Engines\\; Ltd")); // escaped ';'
        assert!(v.contains("TITLE:Mathematician"));
        assert!(v.trim_end().ends_with("END:VCARD"));
    }

    #[test]
    fn contact_assembles_fn_and_uses_business_phone() {
        let c =
            json!({ "givenName": "Grace", "surname": "Hopper", "businessPhones": ["+1 555 0199"] });
        let v = contact_to_vcard(&c);
        assert!(v.contains("FN:Grace Hopper"));
        assert!(v.contains("TEL:+1 555 0199"));
    }
}
