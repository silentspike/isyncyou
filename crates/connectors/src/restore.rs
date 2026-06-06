//! Restore — re-create backed-up items in the cloud via Graph (plan §12).
//!
//! `restore-cloud-item` is a **high-fidelity re-create**, not a byte-identical
//! import (Personal accounts have no mailbox import): a fresh item is POSTed from
//! the canonical JSON with server-managed fields (ids, etags, timestamps, links,
//! occurrence type) dropped, so Graph assigns new ones. The new item carries the
//! rich metadata that round-trips cleanly.
//!
//! Everything goes through one path-based [`Restorer`] (create/delete a resource
//! by its Graph path); each `restore_*` helper picks the writable fields for its
//! type and posts to the right collection.

use serde_json::{Map, Value};

/// Re-creates and (for restore-undo / test cleanup) deletes cloud resources by
/// their Graph collection / resource path. Abstracted so the restore helpers are
/// unit-testable with a mock and live-tested with the real client.
pub trait Restorer {
    /// POST `body` to a collection path (e.g. `/me/events`); return the created
    /// resource JSON.
    fn create(&self, collection_path: &str, body: &Value) -> Result<Value, String>;
    /// DELETE a resource path (e.g. `/me/events/{id}`).
    fn delete(&self, resource_path: &str) -> Result<(), String>;
}

#[cfg(feature = "http")]
impl Restorer for isyncyou_graph::GraphClient {
    fn create(&self, collection_path: &str, body: &Value) -> Result<Value, String> {
        self.post_json(collection_path, body)
            .map_err(|e| e.to_string())
    }
    fn delete(&self, resource_path: &str) -> Result<(), String> {
        self.delete_url(resource_path).map_err(|e| e.to_string())
    }
}

/// Fields Graph accepts on `POST /me/events`. A whitelist (not a denylist)
/// guarantees the create is accepted even as Graph adds read-only fields.
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

/// Fields Graph accepts on `POST /me/todo/lists/{id}/tasks`.
const TASK_WRITABLE: &[&str] = &[
    "title",
    "body",
    "importance",
    "status",
    "isReminderOn",
    "reminderDateTime",
    "dueDateTime",
    "startDateTime",
    "completedDateTime",
    "categories",
    "recurrence",
];

/// Fields Graph accepts on `POST /me/contacts`.
const CONTACT_WRITABLE: &[&str] = &[
    "givenName",
    "surname",
    "middleName",
    "nickName",
    "displayName",
    "title",
    "companyName",
    "jobTitle",
    "department",
    "emailAddresses",
    "businessPhones",
    "homePhones",
    "mobilePhone",
    "homeAddress",
    "businessAddress",
    "personalNotes",
    "birthday",
    "categories",
];

/// Keep only the whitelisted, non-null fields of an object.
fn pick(value: &Value, keys: &[&str]) -> Value {
    let mut out = Map::new();
    if let Some(obj) = value.as_object() {
        for &k in keys {
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

/// Build a POST-able event payload (drops id/etag/timestamps/links/`type`, etc.).
pub fn sanitize_event(event: &Value) -> Value {
    pick(event, EVENT_WRITABLE)
}
/// Build a POST-able task payload.
pub fn sanitize_task(task: &Value) -> Value {
    pick(task, TASK_WRITABLE)
}
/// Build a POST-able contact payload.
pub fn sanitize_contact(contact: &Value) -> Value {
    pick(contact, CONTACT_WRITABLE)
}

fn created_id(created: Value) -> Result<String, String> {
    created
        .get("id")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| "create response had no id".into())
}

fn require_nonempty(p: &Value, what: &str) -> Result<(), String> {
    if p.as_object().map(Map::is_empty).unwrap_or(true) {
        Err(format!("{what} has no restorable fields"))
    } else {
        Ok(())
    }
}

/// Restore one calendar event: sanitize → `POST /me/events` → new event id.
pub fn restore_event<R: Restorer>(restorer: &R, event: &Value) -> Result<String, String> {
    let payload = sanitize_event(event);
    require_nonempty(&payload, "event")?;
    created_id(restorer.create("/me/events", &payload)?)
}

/// Restore one task into `list_id`: sanitize → `POST .../tasks` → new task id.
pub fn restore_task<R: Restorer>(
    restorer: &R,
    list_id: &str,
    task: &Value,
) -> Result<String, String> {
    let payload = sanitize_task(task);
    require_nonempty(&payload, "task")?;
    created_id(restorer.create(&format!("/me/todo/lists/{list_id}/tasks"), &payload)?)
}

/// Restore one contact: sanitize → `POST /me/contacts` → new contact id.
pub fn restore_contact<R: Restorer>(restorer: &R, contact: &Value) -> Result<String, String> {
    let payload = sanitize_contact(contact);
    require_nonempty(&payload, "contact")?;
    created_id(restorer.create("/me/contacts", &payload)?)
}

/// Re-creates a mail message from its full MIME (`.eml`). Separate from
/// [`Restorer`] because mail restore posts raw MIME, not a sanitized JSON
/// object: the whole message (headers, body, attachments) round-trips verbatim.
pub trait MessageCreator {
    fn create_message_from_mime(&self, mime: &[u8]) -> Result<Value, String>;
}

#[cfg(feature = "http")]
impl MessageCreator for isyncyou_graph::GraphClient {
    fn create_message_from_mime(&self, mime: &[u8]) -> Result<Value, String> {
        isyncyou_graph::GraphClient::create_message_from_mime(self, mime).map_err(|e| e.to_string())
    }
}

/// Restore one mail message from its `.eml` MIME: the created message lands in
/// Drafts (Graph's behaviour for a MIME create); returns the new message id.
pub fn restore_message<C: MessageCreator>(creator: &C, mime: &[u8]) -> Result<String, String> {
    if mime.is_empty() {
        return Err("message MIME is empty".into());
    }
    created_id(creator.create_message_from_mime(mime)?)
}

/// Creates a OneNote page from its archived HTML. Separate from [`Restorer`] since
/// a page can't be re-created by a JSON POST — the HTML is re-posted (plan §6).
pub trait PageCreator {
    fn create_page_from_html(&self, html: &[u8]) -> Result<Value, String>;
    fn create_page_from_html_with_resources(
        &self,
        html: &[u8],
        resources: &[OneNoteResourcePart],
    ) -> Result<Value, String>;
}

/// One binary resource part for OneNote page restore. The page HTML must refer to
/// each part as `name:<part_name>` in `img src` / `object data`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneNoteResourcePart {
    pub part_name: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

#[cfg(feature = "http")]
impl PageCreator for isyncyou_graph::GraphClient {
    fn create_page_from_html(&self, html: &[u8]) -> Result<Value, String> {
        isyncyou_graph::GraphClient::create_onenote_page(self, html).map_err(|e| e.to_string())
    }
    fn create_page_from_html_with_resources(
        &self,
        html: &[u8],
        resources: &[OneNoteResourcePart],
    ) -> Result<Value, String> {
        let parts: Vec<_> = resources
            .iter()
            .map(|part| isyncyou_graph::http::OneNotePagePart {
                name: part.part_name.clone(),
                content_type: part.content_type.clone(),
                bytes: part.bytes.clone(),
            })
            .collect();
        isyncyou_graph::GraphClient::create_onenote_page_multipart(self, html, &parts)
            .map_err(|e| e.to_string())
    }
}

/// Restore one OneNote page from its archived HTML body: re-create it in the
/// default section; returns the new page id.
pub fn restore_page<C: PageCreator>(creator: &C, html: &[u8]) -> Result<String, String> {
    if html.is_empty() {
        return Err("page HTML is empty".into());
    }
    created_id(creator.create_page_from_html(html)?)
}

/// Restore one OneNote page from archived HTML plus binary resource parts. Use
/// this when the HTML has already been rewritten to `name:<part-name>` references
/// for images/files; with no resources it falls back to the plain HTML path.
pub fn restore_page_with_resources<C: PageCreator>(
    creator: &C,
    html: &[u8],
    resources: &[OneNoteResourcePart],
) -> Result<String, String> {
    if html.is_empty() {
        return Err("page HTML is empty".into());
    }
    if resources.is_empty() {
        return restore_page(creator, html);
    }
    created_id(creator.create_page_from_html_with_resources(html, resources)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    /// Records every (path, body) created and every path deleted; echoes a new id.
    struct MockRestorer {
        created: RefCell<Vec<(String, Value)>>,
        deleted: RefCell<Vec<String>>,
    }
    impl MockRestorer {
        fn new() -> Self {
            MockRestorer {
                created: RefCell::new(Vec::new()),
                deleted: RefCell::new(Vec::new()),
            }
        }
    }
    impl Restorer for MockRestorer {
        fn create(&self, path: &str, body: &Value) -> Result<Value, String> {
            self.created
                .borrow_mut()
                .push((path.to_string(), body.clone()));
            Ok(json!({ "id": "NEW", "echo": body }))
        }
        fn delete(&self, path: &str) -> Result<(), String> {
            self.deleted.borrow_mut().push(path.to_string());
            Ok(())
        }
    }

    fn full_event() -> Value {
        json!({
            "id": "AAMkOLD", "@odata.etag": "W/\"abc\"", "changeKey": "abc",
            "createdDateTime": "2026-01-01T00:00:00Z", "lastModifiedDateTime": "2026-01-02T00:00:00Z",
            "webLink": "https://outlook.live.com/...", "iCalUId": "040000...",
            "type": "singleInstance",
            "organizer": { "emailAddress": { "address": "testuser@example.com" } },
            "subject": "Quarterly review",
            "body": { "contentType": "html", "content": "<p>agenda</p>" },
            "start": { "dateTime": "2026-03-01T09:00:00", "timeZone": "UTC" },
            "end": { "dateTime": "2026-03-01T10:00:00", "timeZone": "UTC" },
            "location": { "displayName": "Room 1" }, "categories": ["Blue category"], "isAllDay": false
        })
    }
    fn full_task() -> Value {
        json!({
            "id": "TASKOLD", "@odata.etag": "W/\"t\"", "createdDateTime": "2026-01-01T00:00:00Z",
            "lastModifiedDateTime": "2026-01-02T00:00:00Z",
            "title": "Write report", "status": "notStarted", "importance": "high",
            "body": { "content": "draft the Q2 report", "contentType": "text" },
            "dueDateTime": { "dateTime": "2026-03-10T00:00:00", "timeZone": "UTC" }
        })
    }
    fn full_contact() -> Value {
        json!({
            "id": "CTOLD", "@odata.etag": "W/\"c\"", "createdDateTime": "2026-01-01T00:00:00Z",
            "displayName": "Ada Lovelace", "givenName": "Ada", "surname": "Lovelace",
            "emailAddresses": [{ "address": "ada@example.com", "name": "Ada Lovelace" }],
            "mobilePhone": "+1 555 0100"
        })
    }

    #[test]
    fn sanitize_event_keeps_writable_drops_server_fields() {
        let s = sanitize_event(&full_event());
        for k in [
            "id",
            "@odata.etag",
            "changeKey",
            "createdDateTime",
            "webLink",
            "type",
            "organizer",
        ] {
            assert!(s.get(k).is_none(), "{k} should be stripped");
        }
        assert_eq!(s.get("subject").unwrap(), "Quarterly review");
        assert!(s.get("start").is_some() && s.get("end").is_some());
    }

    #[test]
    fn restore_event_posts_to_events_path_sanitized() {
        let m = MockRestorer::new();
        assert_eq!(restore_event(&m, &full_event()).unwrap(), "NEW");
        let c = m.created.borrow();
        assert_eq!(c[0].0, "/me/events");
        assert!(c[0].1.get("id").is_none());
        assert_eq!(c[0].1.get("subject").unwrap(), "Quarterly review");
    }

    #[test]
    fn restore_task_posts_to_list_path_sanitized() {
        let m = MockRestorer::new();
        assert_eq!(restore_task(&m, "LIST1", &full_task()).unwrap(), "NEW");
        let c = m.created.borrow();
        assert_eq!(c[0].0, "/me/todo/lists/LIST1/tasks");
        assert!(c[0].1.get("id").is_none() && c[0].1.get("@odata.etag").is_none());
        assert_eq!(c[0].1.get("title").unwrap(), "Write report");
        assert_eq!(c[0].1.get("importance").unwrap(), "high");
    }

    #[test]
    fn restore_contact_posts_to_contacts_path_sanitized() {
        let m = MockRestorer::new();
        assert_eq!(restore_contact(&m, &full_contact()).unwrap(), "NEW");
        let c = m.created.borrow();
        assert_eq!(c[0].0, "/me/contacts");
        assert!(c[0].1.get("id").is_none());
        assert_eq!(c[0].1.get("displayName").unwrap(), "Ada Lovelace");
        assert!(c[0].1.get("emailAddresses").is_some());
    }

    #[test]
    fn sanitize_contact_drops_photo_fields_for_personal_account_restore() {
        let payload = sanitize_contact(&json!({
            "id": "c1",
            "displayName": "Ada Lovelace",
            "photo": { "id": "240X240" },
            "photo@odata.mediaContentType": "image/jpeg",
            "photo@odata.mediaEtag": "etag",
            "businessPhones": ["+1 555 0100"]
        }));
        assert_eq!(payload.get("displayName").unwrap(), "Ada Lovelace");
        assert_eq!(
            payload.get("businessPhones").unwrap(),
            &json!(["+1 555 0100"])
        );
        assert!(
            payload.get("photo").is_none(),
            "contact photo metadata must not be posted in the contact create payload"
        );
        assert!(
            payload.get("photo@odata.mediaContentType").is_none()
                && payload.get("photo@odata.mediaEtag").is_none(),
            "photo sidecar metadata must not survive sanitize_contact"
        );
    }

    #[test]
    fn empty_payloads_are_rejected() {
        let m = MockRestorer::new();
        assert!(restore_event(&m, &json!({ "id": "x" })).is_err());
        assert!(restore_task(&m, "L", &json!({ "id": "x" })).is_err());
        assert!(restore_contact(&m, &json!({ "id": "x" })).is_err());
        assert!(m.created.borrow().is_empty());
    }

    struct MockMessageCreator {
        last: RefCell<Vec<u8>>,
    }
    impl MessageCreator for MockMessageCreator {
        fn create_message_from_mime(&self, mime: &[u8]) -> Result<Value, String> {
            *self.last.borrow_mut() = mime.to_vec();
            Ok(json!({ "id": "MSG1", "isDraft": true }))
        }
    }

    #[test]
    fn restore_message_passes_mime_and_returns_id() {
        let m = MockMessageCreator {
            last: RefCell::new(Vec::new()),
        };
        let mime = b"From: a@example.com\r\nSubject: Hi\r\n\r\nBody\r\n";
        assert_eq!(restore_message(&m, mime).unwrap(), "MSG1");
        assert_eq!(m.last.borrow().as_slice(), mime);
    }

    #[test]
    fn restore_message_rejects_empty_mime() {
        let m = MockMessageCreator {
            last: RefCell::new(Vec::new()),
        };
        assert!(restore_message(&m, b"").is_err());
    }

    struct MockPageCreator {
        plain: RefCell<Vec<Vec<u8>>>,
        multipart: RefCell<Vec<(Vec<u8>, Vec<OneNoteResourcePart>)>>,
    }
    impl PageCreator for MockPageCreator {
        fn create_page_from_html(&self, html: &[u8]) -> Result<Value, String> {
            self.plain.borrow_mut().push(html.to_vec());
            Ok(json!({ "id": "PAGE1" }))
        }
        fn create_page_from_html_with_resources(
            &self,
            html: &[u8],
            resources: &[OneNoteResourcePart],
        ) -> Result<Value, String> {
            self.multipart
                .borrow_mut()
                .push((html.to_vec(), resources.to_vec()));
            Ok(json!({ "id": "PAGE2" }))
        }
    }

    #[test]
    fn restore_page_uses_plain_html_when_no_resources() {
        let m = MockPageCreator {
            plain: RefCell::new(Vec::new()),
            multipart: RefCell::new(Vec::new()),
        };
        let html = br#"<html><body><p>plain page</p></body></html>"#;
        assert_eq!(restore_page_with_resources(&m, html, &[]).unwrap(), "PAGE1");
        assert_eq!(m.plain.borrow()[0], html);
        assert!(m.multipart.borrow().is_empty());
    }

    #[test]
    fn restore_page_with_resources_uses_multipart_parts() {
        let m = MockPageCreator {
            plain: RefCell::new(Vec::new()),
            multipart: RefCell::new(Vec::new()),
        };
        let html = br#"<html><body><img src="name:imageBlock1" /></body></html>"#;
        let resources = vec![OneNoteResourcePart {
            part_name: "imageBlock1".into(),
            content_type: "image/png".into(),
            bytes: b"png-bytes".to_vec(),
        }];

        assert_eq!(
            restore_page_with_resources(&m, html, &resources).unwrap(),
            "PAGE2"
        );
        assert!(m.plain.borrow().is_empty());
        let multipart = m.multipart.borrow();
        assert_eq!(multipart[0].0, html);
        assert_eq!(multipart[0].1, resources);
    }

    /// Live round-trip: fetch one event, restore it, verify subject, delete copy.
    /// Needs `http` + `ISYNCYOU_TEST_WRITE_TOKEN` (`Calendars.ReadWrite`).
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_restore_event_roundtrip() {
        let _gate = crate::live_test_gate();
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
        let body = match client
            .get("https://graph.microsoft.com/v1.0/me/events?$top=1")
            .body
        {
            Some(b) => b,
            None => {
                eprintln!("events list returned no body; skipping");
                return;
            }
        };
        let Some(event) = body
            .get("value")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .cloned()
        else {
            eprintln!("no events to restore; skipping");
            return;
        };
        let subject = event
            .get("subject")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let new_id = restore_event(&client, &event).expect("restore should succeed");
        let restored = client
            .get(&format!(
                "https://graph.microsoft.com/v1.0/me/events/{new_id}"
            ))
            .body
            .expect("GET restored event");
        assert_eq!(
            restored
                .get("subject")
                .and_then(Value::as_str)
                .unwrap_or(""),
            subject
        );
        eprintln!("restored event '{subject}' as {new_id}");
        client
            .delete(&format!("/me/events/{new_id}"))
            .expect("cleanup");
    }

    /// Live round-trip: fetch one task list + a task, restore into that list,
    /// verify the title, delete the copy. Needs `Tasks.ReadWrite`.
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_restore_task_roundtrip() {
        let _gate = crate::live_test_gate();
        use isyncyou_graph::Transport;
        let token = match std::env::var("ISYNCYOU_TEST_WRITE_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!(
                    "skipping live_restore_task_roundtrip: ISYNCYOU_TEST_WRITE_TOKEN not set"
                );
                return;
            }
        };
        let mut client = isyncyou_graph::GraphClient::new(token);
        let lists = client
            .get("https://graph.microsoft.com/v1.0/me/todo/lists?$top=1")
            .body
            .and_then(|b| {
                b.get("value")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .cloned()
            });
        let Some(list) = lists else {
            eprintln!("no task lists; skipping");
            return;
        };
        let list_id = list.get("id").and_then(Value::as_str).unwrap().to_string();
        let task = client
            .get(&format!(
                "https://graph.microsoft.com/v1.0/me/todo/lists/{list_id}/tasks?$top=1"
            ))
            .body
            .and_then(|b| {
                b.get("value")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .cloned()
            });
        let Some(task) = task else {
            eprintln!("no tasks in the first list; skipping");
            return;
        };
        let title = task
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let new_id = restore_task(&client, &list_id, &task).expect("restore task should succeed");
        let restored = client
            .get(&format!(
                "https://graph.microsoft.com/v1.0/me/todo/lists/{list_id}/tasks/{new_id}"
            ))
            .body
            .expect("GET restored task");
        assert_eq!(
            restored.get("title").and_then(Value::as_str).unwrap_or(""),
            title
        );
        eprintln!("restored task '{title}' as {new_id} in list {list_id}");
        client
            .delete(&format!("/me/todo/lists/{list_id}/tasks/{new_id}"))
            .expect("cleanup");
    }

    /// Live round-trip: create a synthetic contact (the account has none to
    /// fetch), verify it, delete it. Needs `Contacts.ReadWrite`.
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_restore_contact_roundtrip() {
        let _gate = crate::live_test_gate();
        use isyncyou_graph::Transport;
        let token = match std::env::var("ISYNCYOU_TEST_WRITE_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!(
                    "skipping live_restore_contact_roundtrip: ISYNCYOU_TEST_WRITE_TOKEN not set"
                );
                return;
            }
        };
        let mut client = isyncyou_graph::GraphClient::new(token);
        let synthetic = json!({
            "id": "SHOULD_BE_STRIPPED",
            "@odata.etag": "W/\"x\"",
            "displayName": "iSyncYou Restore Test",
            "givenName": "iSyncYou",
            "surname": "RestoreTest",
            "emailAddresses": [{ "address": "isyncyou-test@example.com", "name": "iSyncYou Restore Test" }],
            "mobilePhone": "+1 555 0123"
        });
        let new_id = restore_contact(&client, &synthetic).expect("restore contact should succeed");
        let restored = client
            .get(&format!(
                "https://graph.microsoft.com/v1.0/me/contacts/{new_id}"
            ))
            .body
            .expect("GET restored contact");
        assert_eq!(
            restored
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or(""),
            "iSyncYou Restore Test"
        );
        eprintln!("restored synthetic contact as {new_id}");
        client
            .delete(&format!("/me/contacts/{new_id}"))
            .expect("cleanup");
    }

    /// Live round-trip: re-create a mail message from synthetic MIME (lands in
    /// Drafts), verify the subject, delete it. Needs `Mail.ReadWrite`.
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_restore_message_roundtrip() {
        let _gate = crate::live_test_gate();
        use isyncyou_graph::Transport;
        let token = match std::env::var("ISYNCYOU_TEST_WRITE_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!(
                    "skipping live_restore_message_roundtrip: ISYNCYOU_TEST_WRITE_TOKEN not set"
                );
                return;
            }
        };
        let mut client = isyncyou_graph::GraphClient::new(token);
        let mime = b"MIME-Version: 1.0\r\n\
From: iSyncYou Test <isyncyou-test@example.com>\r\n\
To: testuser@example.com\r\n\
Subject: iSyncYou MIME restore test\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
This message was re-created from MIME by iSyncYou.\r\n";
        let new_id = restore_message(&client, mime).expect("restore message should succeed");
        let restored = client
            .get(&format!(
                "https://graph.microsoft.com/v1.0/me/messages/{new_id}"
            ))
            .body
            .expect("GET restored message");
        assert_eq!(
            restored
                .get("subject")
                .and_then(Value::as_str)
                .unwrap_or(""),
            "iSyncYou MIME restore test"
        );
        eprintln!(
            "restored MIME message as {new_id} (isDraft={:?})",
            restored.get("isDraft")
        );
        client
            .delete(&format!("/me/messages/{new_id}"))
            .expect("cleanup");
    }

    /// Live round-trip: restore (create) a OneNote page from HTML, then clean it up.
    /// Needs `http` + `ISYNCYOU_TEST_WRITE_TOKEN` with `Notes.ReadWrite`. OneNote is
    /// eventually consistent, so the cleanup delete is retried until it propagates
    /// (a successful delete confirms the page was created).
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_restore_page_roundtrip() {
        let _gate = crate::live_test_gate();
        let token = match std::env::var("ISYNCYOU_TEST_WRITE_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!(
                    "skipping live_restore_page_roundtrip: ISYNCYOU_TEST_WRITE_TOKEN not set"
                );
                return;
            }
        };
        let client = isyncyou_graph::GraphClient::new(token);
        let html = b"<!DOCTYPE html><html><head><title>iSyncYou page restore test</title></head>\
                     <body><p>round-trip</p></body></html>";
        let new_id = restore_page(&client, html).expect("restore_page should create a page");
        assert!(!new_id.is_empty(), "expected a new page id");
        eprintln!("restored onenote page {new_id}");
        // delete with retry — OneNote may 404 a fresh page until it propagates
        let mut cleaned = false;
        for _ in 0..8 {
            std::thread::sleep(std::time::Duration::from_secs(8));
            if client.delete_onenote_page(&new_id).is_ok() {
                cleaned = true;
                break;
            }
        }
        assert!(cleaned, "failed to clean up restored page {new_id}");
    }
}
