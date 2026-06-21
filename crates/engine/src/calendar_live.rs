//! Live calendar **write** layer (#565 B6): the live client's event verbs —
//! create, update, delete, respond — behind [`CalendarWriter`] so the engine
//! wiring + the daemon handler are unit-tested deterministically without a
//! network; `GraphClient` is the real impl.
//!
//! Like `mail_live` (and unlike the crash-safe, ledger-backed `restore_calendar`),
//! this is the *interactive* path: the user creates/edits/responds in the live
//! client and the change is pushed straight to Microsoft 365. The write token is
//! the full restore-scope token (`Calendars.ReadWrite`, from #558), resolved from
//! the cached `login --write`. Create/update bodies are sanitized to the writable
//! event whitelist (`sanitize_event`); ids are URL-encoded by the graph layer.

use isyncyou_core::Config;
use serde_json::Value;

/// The live calendar write operations, object-safe so the daemon can hold a
/// `&dyn CalendarWriter` and tests can swap in a fake.
pub trait CalendarWriter {
    /// Create an event from a composed/archived event JSON (sanitized to the
    /// writable fields); returns the new cloud id.
    fn create_event(&self, event: &Value) -> Result<String, String>;
    /// Update an event's writable fields (the patch is sanitized first).
    fn update_event(&self, event_id: &str, patch: &Value) -> Result<(), String>;
    /// Delete an event.
    fn delete_event(&self, event_id: &str) -> Result<(), String>;
    /// Respond to an invitation: `accept` / `decline` / `tentative` (+ optional
    /// comment); the response email is sent.
    fn respond(&self, event_id: &str, response: &str, comment: &str) -> Result<(), String>;
}

// Inherent GraphClient methods share names with the trait, so each delegation is
// fully qualified to call the inherent (HTTP) method, never recurse.
impl CalendarWriter for isyncyou_graph::GraphClient {
    fn create_event(&self, event: &Value) -> Result<String, String> {
        let body = isyncyou_connectors::sanitize_event(event);
        let v =
            isyncyou_graph::GraphClient::create_event(self, &body).map_err(|e| e.to_string())?;
        v.get("id")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| "created event response has no id".to_string())
    }
    fn update_event(&self, event_id: &str, patch: &Value) -> Result<(), String> {
        let body = isyncyou_connectors::sanitize_event(patch);
        isyncyou_graph::GraphClient::update_event(self, event_id, &body)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    fn delete_event(&self, event_id: &str) -> Result<(), String> {
        isyncyou_graph::GraphClient::delete_event(self, event_id).map_err(|e| e.to_string())
    }
    fn respond(&self, event_id: &str, response: &str, comment: &str) -> Result<(), String> {
        isyncyou_graph::GraphClient::respond_event(self, event_id, response, comment)
            .map_err(|e| e.to_string())
    }
}

/// Resolve the full write token (restore scopes incl. `Calendars.ReadWrite`) and
/// build a ready `GraphClient` for the live-calendar write ops. The token is
/// silently refreshed from the cached `login --write`; a missing cache is an
/// error. This is the daemon's entry point into the layer.
pub fn calendar_writer(cfg: &Config, account: &str) -> Result<isyncyou_graph::GraphClient, String> {
    let token = crate::auth::resolve_cached_restore_token(cfg, account)?;
    Ok(isyncyou_graph::GraphClient::new(token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakeCal {
        log: RefCell<Vec<String>>,
    }
    impl CalendarWriter for FakeCal {
        fn create_event(&self, event: &Value) -> Result<String, String> {
            self.log.borrow_mut().push(format!(
                "create subj={}",
                event.get("subject").and_then(Value::as_str).unwrap_or("")
            ));
            Ok("new-evt-1".into())
        }
        fn update_event(&self, id: &str, patch: &Value) -> Result<(), String> {
            self.log.borrow_mut().push(format!(
                "update id={id} showAs={}",
                patch.get("showAs").and_then(Value::as_str).unwrap_or("")
            ));
            Ok(())
        }
        fn delete_event(&self, id: &str) -> Result<(), String> {
            self.log.borrow_mut().push(format!("delete id={id}"));
            Ok(())
        }
        fn respond(&self, id: &str, response: &str, comment: &str) -> Result<(), String> {
            self.log
                .borrow_mut()
                .push(format!("respond id={id} r={response} c={comment}"));
            Ok(())
        }
    }

    #[test]
    fn calendar_writer_is_object_safe_and_ops_carry_ids() {
        let f = FakeCal::default();
        let w: &dyn CalendarWriter = &f; // compiles only if the trait is object-safe
        assert_eq!(
            w.create_event(&json!({ "subject": "Plan" })).unwrap(),
            "new-evt-1"
        );
        w.update_event("E1", &json!({ "showAs": "busy" })).unwrap();
        w.delete_event("E2").unwrap();
        w.respond("E3", "accept", "see you").unwrap();
        let log = f.log.borrow();
        assert_eq!(log[0], "create subj=Plan");
        assert_eq!(log[1], "update id=E1 showAs=busy");
        assert_eq!(log[2], "delete id=E2");
        assert_eq!(log[3], "respond id=E3 r=accept c=see you");
    }
}
