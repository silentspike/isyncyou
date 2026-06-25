//! Live contact **write** layer (#566 A4): the live client's contact verbs —
//! create, update, delete — behind [`ContactWriter`] so the engine wiring + the
//! daemon handler are unit-tested deterministically without a network;
//! `GraphClient` is the real impl.
//!
//! Like `calendar_live` (and unlike the crash-safe, ledger-backed
//! `restore_contacts`), this is the *interactive* path: the user creates/edits a
//! contact in the live client and the change is pushed straight to Microsoft 365.
//! The write token is the full restore-scope token (`Contacts.ReadWrite`, from
//! #558), resolved from the cached `login --write`. Create/update bodies are
//! sanitized to the writable contact whitelist (`sanitize_contact`); ids are
//! URL-encoded by the graph layer.

use isyncyou_core::Config;
use serde_json::Value;

/// The live contact write operations, object-safe so the daemon can hold a
/// `&dyn ContactWriter` and tests can swap in a fake.
pub trait ContactWriter {
    /// Create a contact from a composed/archived contact JSON (sanitized to the
    /// writable fields); returns the new cloud id.
    fn create_contact(&self, contact: &Value) -> Result<String, String>;
    /// Update a contact's writable fields (the patch is sanitized first).
    fn update_contact(&self, contact_id: &str, patch: &Value) -> Result<(), String>;
    /// Delete a contact.
    fn delete_contact(&self, contact_id: &str) -> Result<(), String>;
}

// Inherent GraphClient methods share names with the trait, so each delegation is
// fully qualified to call the inherent (HTTP) method, never recurse.
impl ContactWriter for isyncyou_graph::GraphClient {
    fn create_contact(&self, contact: &Value) -> Result<String, String> {
        let body = isyncyou_connectors::sanitize_contact(contact);
        let v =
            isyncyou_graph::GraphClient::create_contact(self, &body).map_err(|e| e.to_string())?;
        v.get("id")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| "created contact response has no id".to_string())
    }
    fn update_contact(&self, contact_id: &str, patch: &Value) -> Result<(), String> {
        let body = isyncyou_connectors::sanitize_contact(patch);
        isyncyou_graph::GraphClient::update_contact(self, contact_id, &body)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    fn delete_contact(&self, contact_id: &str) -> Result<(), String> {
        isyncyou_graph::GraphClient::delete_contact(self, contact_id).map_err(|e| e.to_string())
    }
}

/// Resolve the full write token (restore scopes incl. `Contacts.ReadWrite`) and
/// build a ready `GraphClient` for the live-contact write ops. The token is
/// silently refreshed from the cached `login --write`; a missing cache is an
/// error. This is the daemon's entry point into the layer.
pub fn contact_writer(cfg: &Config, account: &str) -> Result<isyncyou_graph::GraphClient, String> {
    let token = crate::auth::resolve_cached_restore_token(cfg, account)?;
    Ok(isyncyou_graph::GraphClient::new(token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakeContacts {
        log: RefCell<Vec<String>>,
    }
    impl ContactWriter for FakeContacts {
        fn create_contact(&self, contact: &Value) -> Result<String, String> {
            self.log.borrow_mut().push(format!(
                "create name={}",
                contact
                    .get("displayName")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            ));
            Ok("new-con-1".into())
        }
        fn update_contact(&self, id: &str, patch: &Value) -> Result<(), String> {
            self.log.borrow_mut().push(format!(
                "update id={id} job={}",
                patch.get("jobTitle").and_then(Value::as_str).unwrap_or("")
            ));
            Ok(())
        }
        fn delete_contact(&self, id: &str) -> Result<(), String> {
            self.log.borrow_mut().push(format!("delete id={id}"));
            Ok(())
        }
    }

    #[test]
    fn contact_writer_is_object_safe_and_ops_carry_ids() {
        let f = FakeContacts::default();
        let w: &dyn ContactWriter = &f; // compiles only if the trait is object-safe
        assert_eq!(
            w.create_contact(&json!({ "displayName": "Ada Lovelace" }))
                .unwrap(),
            "new-con-1"
        );
        w.update_contact("C1", &json!({ "jobTitle": "Analyst" }))
            .unwrap();
        w.delete_contact("C2").unwrap();
        let log = f.log.borrow();
        assert_eq!(log[0], "create name=Ada Lovelace");
        assert_eq!(log[1], "update id=C1 job=Analyst");
        assert_eq!(log[2], "delete id=C2");
    }
}
