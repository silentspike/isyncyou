//! Restore — re-create backed-up items in the cloud via Graph (plan §12).
//!
//! `restore-cloud-item` is a **high-fidelity re-create**, not a byte-identical
//! import (Personal accounts have no mailbox import): a fresh item is POSTed from
//! the canonical JSON with server-managed fields (ids, etags, timestamps, links,
//! occurrence type) dropped, so Graph assigns new ones. The new item carries the
//! rich metadata that round-trips cleanly. This module starts with calendar
//! events; mail/tasks/contacts follow the same shape.

use serde_json::{Map, Value};

/// The event fields Graph accepts on `POST /me/events`. Building the payload from
/// a whitelist (rather than stripping a denylist) guarantees the create is
/// accepted even as Graph adds new read-only fields over time.
const EVENT_WRITABLE: &[&str] = &[
    "subject",
    "body",
    "start",
    "end",
    "location",
    "locations",
    "attendees",
    "categories",
    "importance",
    "sensitivity",
    "showAs",
    "isAllDay",
    "isReminderOn",
    "reminderMinutesBeforeStart",
    "recurrence",
    "responseRequested",
    "allowNewTimeProposals",
    "hideAttendees",
];

/// Build a POST-able event payload from a stored/fetched event, keeping only
/// writable fields (drops `id`/etag/timestamps/links/`type`, etc.).
pub fn sanitize_event(event: &Value) -> Value {
    let mut out = Map::new();
    if let Some(obj) = event.as_object() {
        for &k in EVENT_WRITABLE {
            match obj.get(k) {
                Some(v) if !v.is_null() => {
                    out.insert(k.to_string(), v.clone());
                }
                _ => {}
            }
        }
    }
    Value::Object(out)
}

/// Re-creates (and, for test cleanup, deletes) a calendar event in the cloud.
pub trait EventRestorer {
    fn create_event(&self, event: &Value) -> Result<Value, String>;
    fn delete_event(&self, id: &str) -> Result<(), String>;
}

#[cfg(feature = "http")]
impl EventRestorer for isyncyou_graph::GraphClient {
    fn create_event(&self, event: &Value) -> Result<Value, String> {
        self.post_json("/me/events", event)
            .map_err(|e| e.to_string())
    }
    fn delete_event(&self, id: &str) -> Result<(), String> {
        self.delete_url(&format!("/me/events/{id}"))
            .map_err(|e| e.to_string())
    }
}

/// Restore one calendar event: sanitize → create → return the new event id.
pub fn restore_event<R: EventRestorer>(restorer: &R, event: &Value) -> Result<String, String> {
    let payload = sanitize_event(event);
    if payload.as_object().map(Map::is_empty).unwrap_or(true) {
        return Err("event has no restorable fields".into());
    }
    let created = restorer.create_event(&payload)?;
    created
        .get("id")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| "create response had no id".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    struct MockRestorer {
        posted: RefCell<Option<Value>>,
        deleted: RefCell<Vec<String>>,
    }
    impl MockRestorer {
        fn new() -> Self {
            MockRestorer {
                posted: RefCell::new(None),
                deleted: RefCell::new(Vec::new()),
            }
        }
    }
    impl EventRestorer for MockRestorer {
        fn create_event(&self, event: &Value) -> Result<Value, String> {
            *self.posted.borrow_mut() = Some(event.clone());
            Ok(json!({ "id": "NEWEV", "subject": event.get("subject") }))
        }
        fn delete_event(&self, id: &str) -> Result<(), String> {
            self.deleted.borrow_mut().push(id.to_string());
            Ok(())
        }
    }

    fn full_event() -> Value {
        json!({
            "id": "AAMkOLD",
            "@odata.etag": "W/\"abc\"",
            "changeKey": "abc",
            "createdDateTime": "2026-01-01T00:00:00Z",
            "lastModifiedDateTime": "2026-01-02T00:00:00Z",
            "webLink": "https://outlook.live.com/...",
            "iCalUId": "040000...",
            "type": "singleInstance",
            "organizer": { "emailAddress": { "address": "backupslave@outlook.com" } },
            "subject": "Quarterly review",
            "body": { "contentType": "html", "content": "<p>agenda</p>" },
            "start": { "dateTime": "2026-03-01T09:00:00", "timeZone": "UTC" },
            "end": { "dateTime": "2026-03-01T10:00:00", "timeZone": "UTC" },
            "location": { "displayName": "Room 1" },
            "categories": ["Blue category"],
            "isAllDay": false
        })
    }

    #[test]
    fn sanitize_keeps_writable_drops_server_fields() {
        let s = sanitize_event(&full_event());
        // server-managed fields gone
        for k in [
            "id",
            "@odata.etag",
            "changeKey",
            "createdDateTime",
            "lastModifiedDateTime",
            "webLink",
            "iCalUId",
            "type",
            "organizer",
        ] {
            assert!(s.get(k).is_none(), "{k} should be stripped");
        }
        // writable fields preserved
        assert_eq!(s.get("subject").unwrap(), "Quarterly review");
        assert!(s.get("start").is_some());
        assert!(s.get("end").is_some());
        assert_eq!(s.get("categories").unwrap(), &json!(["Blue category"]));
    }

    #[test]
    fn restore_event_posts_sanitized_and_returns_new_id() {
        let m = MockRestorer::new();
        let id = restore_event(&m, &full_event()).unwrap();
        assert_eq!(id, "NEWEV");
        let posted = m.posted.borrow();
        let posted = posted.as_ref().unwrap();
        assert!(posted.get("id").is_none(), "must not post the old id");
        assert_eq!(posted.get("subject").unwrap(), "Quarterly review");
    }

    #[test]
    fn event_without_writable_fields_is_rejected() {
        let m = MockRestorer::new();
        let err = restore_event(&m, &json!({ "id": "x", "@odata.etag": "y" })).unwrap_err();
        assert!(err.contains("no restorable fields"));
        assert!(m.posted.borrow().is_none());
    }

    /// Live round-trip: fetch one of the account's events, restore it as a new
    /// event, verify the subject matches, then delete the copy. Needs feature
    /// `http` + `ISYNCYOU_TEST_WRITE_TOKEN` carrying `Calendars.ReadWrite`.
    #[cfg(feature = "http")]
    #[test]
    fn live_restore_event_roundtrip() {
        use isyncyou_graph::Transport;
        let token = match std::env::var("ISYNCYOU_TEST_WRITE_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!(
                    "skipping live_restore_event_roundtrip: ISYNCYOU_TEST_WRITE_TOKEN not set"
                );
                return;
            }
        };
        let mut client = isyncyou_graph::GraphClient::new(token);
        let list = client.get("https://graph.microsoft.com/v1.0/me/events?$top=1");
        let body = match list.body {
            Some(b) => b,
            None => {
                eprintln!(
                    "events list returned no body (HTTP {}); skipping",
                    list.status
                );
                return;
            }
        };
        let event = body
            .get("value")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .cloned();
        let Some(event) = event else {
            eprintln!("account has no events to restore; skipping");
            return;
        };
        let subject = event
            .get("subject")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let new_id = restore_event(&client, &event).expect("restore should succeed");
        assert!(!new_id.is_empty());

        let restored = client
            .get(&format!(
                "https://graph.microsoft.com/v1.0/me/events/{new_id}"
            ))
            .body
            .expect("GET restored event should have a body");
        assert_eq!(
            restored
                .get("subject")
                .and_then(Value::as_str)
                .unwrap_or(""),
            subject,
            "restored event subject must match the original"
        );
        eprintln!("restored event '{subject}' as {new_id}");

        client
            .delete_event(&new_id)
            .expect("cleanup delete should succeed");
        eprintln!("cleaned up restored event {new_id}");
    }
}
