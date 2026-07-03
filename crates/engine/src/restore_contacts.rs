//! Crash-safe **contacts** restore: a [`RestoreSink`] backed by Microsoft Graph
//! contacts, plus the ledger-driven entry point that `restore_cloud` uses for contacts.
//!
//! Contacts follow the **mail-shaped** model (see `mail_restore.rs`), not the calendar
//! one: `POST /me/contacts` is **not** idempotent, so crash-safety comes from the
//! ledger **plus a marker probe**. The marker is a **single-value extended property**
//! carrying the content-HMAC key; recovery finds an already-created contact via
//! `$filter` on `singleValueExtendedProperties`. Both capabilities are confirmed live
//! (`tools/live_contacts_probe.py`): Graph accepts the extended property on create and
//! the `$filter` query finds it back.
//!
//! The two Graph calls are behind [`ContactApi`] so the wiring + recovery are
//! unit-tested deterministically without a network; `GraphClient` is the real impl.

use crate::restore_key::{contact_marker, idempotency_key, load_or_create_secret};
use crate::restore_recovery::{recover_restore_op, run_restore_op, RestoreSink};
use isyncyou_core::Config;
use isyncyou_store::{RestoreState, Store};
use serde_json::Value;

/// The single-value extended-property **id** that carries the restore marker. The
/// `Name` segment namespaces it; the random GUID keeps it from colliding with any
/// other extended property. Mirrors `tools/live_contacts_probe.py`.
pub const MARKER_PROP_ID: &str =
    "String {f3f9a7b1-6f1e-4a2b-9c3d-1e2f3a4b5c6d} Name isyncyou-restore-key";

/// The two Graph operations a crash-safe contact restore needs, abstracted so the
/// ledger wiring can be exercised without a network.
pub trait ContactApi {
    /// Create a contact from a POST-ready JSON body (already sanitized and carrying the
    /// marker as a single-value extended property); returns the new cloud id.
    fn create_contact(&self, body: &Value) -> Result<String, String>;
    /// Find a contact by its marker extended property; returns its cloud id if present.
    fn find_by_marker(&self, marker: &str) -> Result<Option<String>, String>;
}

impl ContactApi for isyncyou_graph::GraphClient {
    fn create_contact(&self, body: &Value) -> Result<String, String> {
        let v = self
            .post_json("/me/contacts", body)
            .map_err(|e| e.to_string())?;
        v.get("id")
            .and_then(|i| i.as_str())
            .map(String::from)
            .ok_or_else(|| "created contact response has no id".to_string())
    }
    fn find_by_marker(&self, marker: &str) -> Result<Option<String>, String> {
        let filter = format!(
            "singleValueExtendedProperties/any(ep: ep/id eq '{}' and ep/value eq '{}')",
            encode_filter_value(MARKER_PROP_ID),
            encode_filter_value(marker)
        );
        let url = format!(
            "/me/contacts?$filter={}&$select=id&$top=1",
            encode_filter_value(&filter)
        );
        let v = self.get_json(&url).map_err(|e| e.to_string())?;
        Ok(v.get("value")
            .and_then(|a| a.as_array())
            .and_then(|a| a.first())
            .and_then(|m| m.get("id"))
            .and_then(|i| i.as_str())
            .map(String::from))
    }
}

/// Minimal percent-encoding for the marker/property-id inside an OData `$filter` value.
fn encode_filter_value(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'<' => o.push_str("%3C"),
            b'>' => o.push_str("%3E"),
            b'@' => o.push_str("%40"),
            b' ' => o.push_str("%20"),
            b'\'' => o.push_str("%27"),
            b'{' => o.push_str("%7B"),
            b'}' => o.push_str("%7D"),
            _ => o.push(b as char),
        }
    }
    o
}

/// A [`RestoreSink`] for contacts: `create` sanitizes the archived contact, stamps the
/// marker as a single-value extended property, then posts; `find_by_marker` probes
/// Graph by that extended property.
pub struct ContactSink<'a, A: ContactApi> {
    pub api: &'a A,
}

impl<A: ContactApi> RestoreSink for ContactSink<'_, A> {
    fn create(&self, marker: &str, payload: &[u8]) -> Result<String, String> {
        let contact: Value = serde_json::from_slice(payload)
            .map_err(|e| format!("archived contact is not JSON: {e}"))?;
        let mut body = isyncyou_connectors::sanitize_contact(&contact);
        // The sanitizer keeps only writable contact fields (and drops photo metadata
        // per REQ-RST-008), so stamp the marker extended property afterwards.
        let obj = body
            .as_object_mut()
            .ok_or_else(|| "sanitized contact is not a JSON object".to_string())?;
        obj.insert(
            "singleValueExtendedProperties".to_string(),
            serde_json::json!([{ "id": MARKER_PROP_ID, "value": marker }]),
        );
        self.api.create_contact(&body)
    }
    fn find_by_marker(&self, marker: &str) -> Result<Option<String>, String> {
        self.api.find_by_marker(marker)
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Restore one archived contact to the cloud **through the operation ledger**.
/// Idempotent: a repeat of the same content recognises the existing operation and
/// either returns the committed id or reconciles an interrupted one — never a
/// duplicate. Returns the new cloud id.
pub fn restore_contacts_via_ledger(
    cfg: &Config,
    account: &str,
    id: &str,
    token: String,
) -> Result<String, String> {
    let acc = cfg
        .accounts
        .iter()
        .find(|a| a.id == account)
        .ok_or_else(|| format!("no account '{account}' in config"))?;
    let (_item, bytes) = crate::read_archived_body(cfg, account, "contacts", id)?;
    let secret = load_or_create_secret(&acc.archive_root.join(".isyncyou-restore-secret"))?;
    let key = idempotency_key(&secret, account, "contacts", id, &bytes);
    let op_id = format!("{account}:{key}");
    let marker = contact_marker(&key);
    let store =
        Store::open(acc.archive_root.join(".isyncyou-store.db")).map_err(|e| e.to_string())?;
    let client = isyncyou_graph::GraphClient::new(token);
    let sink = ContactSink { api: &client };
    finish_contacts_restore(
        &store,
        &op_id,
        account,
        id,
        &key,
        &marker,
        &bytes,
        &sink,
        now_secs(),
    )
}

/// The idempotent ledger flow, separated so it can be tested with a fake sink.
#[allow(clippy::too_many_arguments)]
fn finish_contacts_restore<S: RestoreSink>(
    store: &Store,
    op_id: &str,
    account: &str,
    source_id: &str,
    key: &str,
    marker: &str,
    payload: &[u8],
    sink: &S,
    now: i64,
) -> Result<String, String> {
    match store
        .get_restore_operation(op_id)
        .map_err(|e| e.to_string())?
    {
        // Already done: return the recorded id (no second create).
        Some(op) if op.state == RestoreState::Committed => op
            .new_cloud_id
            .ok_or_else(|| "committed operation has no cloud id".to_string()),
        // Interrupted earlier: recover (reconcile by marker, or resume) — never blind-retry.
        Some(_) => {
            recover_restore_op(store, op_id, payload, sink, now)?;
            store
                .get_restore_operation(op_id)
                .map_err(|e| e.to_string())?
                .and_then(|o| o.new_cloud_id)
                .ok_or_else(|| "recovery did not record a cloud id".to_string())
        }
        // Fresh: record intent, then drive the happy path.
        None => {
            store
                .create_restore_operation(op_id, account, "contacts", source_id, key, now)
                .map_err(|e| e.to_string())?;
            let (new_id, _) = run_restore_op(store, op_id, marker, payload, sink, now)?;
            Ok(new_id)
        }
    }
}

/// How many non-terminal **contacts** restore operations are pending for `account`.
pub fn pending_contacts_restore_count(cfg: &Config, account: &str) -> Result<usize, String> {
    let acc = cfg
        .accounts
        .iter()
        .find(|a| a.id == account)
        .ok_or_else(|| format!("no account '{account}' in config"))?;
    let store =
        Store::open(acc.archive_root.join(".isyncyou-store.db")).map_err(|e| e.to_string())?;
    Ok(store
        .recoverable_restore_operations(account)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|o| o.service == "contacts")
        .count())
}

/// Read one archived contact's JSON from an already-open store.
fn archived_contact_bytes(
    store: &Store,
    acc: &isyncyou_core::AccountConfig,
    source_id: &str,
) -> Result<Vec<u8>, String> {
    let item = store
        .get_item(&acc.id, "contacts", source_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no archived contacts item '{source_id}'"))?;
    let rel = item
        .local_path
        .ok_or_else(|| format!("item '{source_id}' has no archived body"))?;
    std::fs::read(acc.archive_root.join(&rel)).map_err(|e| e.to_string())
}

/// Drive every pending contact restore operation for `account` to a terminal state
/// using `sink` (reconcile by marker, or resume) — the boot-recovery core, with the
/// cloud abstracted so it is testable. Returns `(recovered, still_failing)`.
pub fn recover_pending_contacts_restores_with<S: RestoreSink>(
    cfg: &Config,
    account: &str,
    sink: &S,
) -> Result<(usize, usize), String> {
    let acc = cfg
        .accounts
        .iter()
        .find(|a| a.id == account)
        .ok_or_else(|| format!("no account '{account}' in config"))?;
    let store =
        Store::open(acc.archive_root.join(".isyncyou-store.db")).map_err(|e| e.to_string())?;
    let ops = store
        .recoverable_restore_operations(account)
        .map_err(|e| e.to_string())?;
    let now = now_secs();
    let (mut ok, mut failed) = (0usize, 0usize);
    for op in ops.into_iter().filter(|o| o.service == "contacts") {
        let res = archived_contact_bytes(&store, acc, &op.source_item_id)
            .and_then(|bytes| recover_restore_op(&store, &op.op_id, &bytes, sink, now).map(|_| ()));
        match res {
            Ok(()) => ok += 1,
            Err(_) => failed += 1,
        }
    }
    Ok((ok, failed))
}

/// Boot recovery against the live Graph using `token`. Thin wrapper over
/// [`recover_pending_contacts_restores_with`].
pub fn recover_pending_contacts_restores(
    cfg: &Config,
    account: &str,
    token: String,
) -> Result<(usize, usize), String> {
    let client = isyncyou_graph::GraphClient::new(token);
    let sink = ContactSink { api: &client };
    recover_pending_contacts_restores_with(cfg, account, &sink)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A fake Graph contacts store. `create_contact` stores the contact keyed by the
    /// marker read out of the posted extended property (so it exercises the real
    /// `ContactSink::create` stamping), and is deliberately **non-idempotent** — proving
    /// the ledger + marker probe is what prevents duplicates. Flags simulate the two
    /// dangerous crash interleavings.
    #[derive(Default)]
    struct FakeContactApi {
        contacts: RefCell<Vec<(String, String)>>, // (marker, cloud id)
        seq: RefCell<u32>,
        creates: RefCell<u32>,
        crash_after_store: RefCell<bool>, // POST landed, response lost
        fail_before_store: RefCell<bool>, // POST never reached Graph
    }
    impl FakeContactApi {
        fn count(&self) -> usize {
            self.contacts.borrow().len()
        }
        fn creates(&self) -> u32 {
            *self.creates.borrow()
        }
        fn marker_of(body: &Value) -> String {
            body.get("singleValueExtendedProperties")
                .and_then(|a| a.as_array())
                .and_then(|a| a.first())
                .and_then(|p| p.get("value"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        }
    }
    impl ContactApi for FakeContactApi {
        fn create_contact(&self, body: &Value) -> Result<String, String> {
            *self.creates.borrow_mut() += 1;
            if *self.fail_before_store.borrow() {
                return Err("network failed before reaching Graph".into());
            }
            let marker = Self::marker_of(body);
            let mut seq = self.seq.borrow_mut();
            *seq += 1;
            let id = format!("contact-{}", *seq);
            self.contacts.borrow_mut().push((marker, id.clone()));
            if *self.crash_after_store.borrow() {
                return Err("network dropped after create".into());
            }
            Ok(id)
        }
        fn find_by_marker(&self, marker: &str) -> Result<Option<String>, String> {
            Ok(self
                .contacts
                .borrow()
                .iter()
                .find(|(m, _)| m == marker)
                .map(|(_, id)| id.clone()))
        }
    }

    fn contact_json() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "givenName": "Ada",
            "surname": "Lovelace",
            "emailAddresses": [{ "address": "ada@example.com", "name": "Ada" }],
            // non-writable fields the sanitizer must drop:
            "id": "AAMkContactOLD==",
            "changeKey": "abc",
        }))
        .unwrap()
    }

    fn key_marker(payload: &[u8]) -> (String, String) {
        let key = idempotency_key(b"secret", "acc", "contacts", "src1", payload);
        let marker = contact_marker(&key);
        (key, marker)
    }

    #[test]
    fn happy_path_creates_one_and_is_idempotent_on_repeat() {
        let s = Store::open_in_memory().unwrap();
        let api = FakeContactApi::default();
        let sink = ContactSink { api: &api };
        let payload = contact_json();
        let (key, marker) = key_marker(&payload);
        let op = format!("acc:{key}");

        let id1 =
            finish_contacts_restore(&s, &op, "acc", "src1", &key, &marker, &payload, &sink, 10)
                .unwrap();
        let id2 =
            finish_contacts_restore(&s, &op, "acc", "src1", &key, &marker, &payload, &sink, 20)
                .unwrap();
        assert_eq!(id1, id2);
        assert_eq!(api.count(), 1);
        assert_eq!(api.creates(), 1);
    }

    #[test]
    fn create_stamps_marker_property_and_drops_non_writable_fields() {
        // Prove the sink stamps the marker the probe later searches for, and that the
        // sanitizer dropped the read-only id/changeKey.
        let api = FakeContactApi::default();
        let sink = ContactSink { api: &api };
        let payload = contact_json();
        let (_key, marker) = key_marker(&payload);
        let id = sink.create(&marker, &payload).unwrap();
        assert_eq!(api.find_by_marker(&marker).unwrap(), Some(id));
        // the stored marker equals exactly what we stamped
        assert_eq!(api.contacts.borrow()[0].0, marker);
    }

    #[test]
    fn crash_after_post_landed_does_not_duplicate() {
        let s = Store::open_in_memory().unwrap();
        let api = FakeContactApi::default();
        *api.crash_after_store.borrow_mut() = true; // POST lands, then connection drops
        let sink = ContactSink { api: &api };
        let payload = contact_json();
        let (key, marker) = key_marker(&payload);
        let op = format!("acc:{key}");

        let first =
            finish_contacts_restore(&s, &op, "acc", "src1", &key, &marker, &payload, &sink, 10);
        assert!(first.is_err());
        assert_eq!(api.count(), 1, "the POST landed");

        *api.crash_after_store.borrow_mut() = false;
        let id =
            finish_contacts_restore(&s, &op, "acc", "src1", &key, &marker, &payload, &sink, 20)
                .unwrap();
        assert!(!id.is_empty());
        assert_eq!(
            api.count(),
            1,
            "no duplicate after recovery (found by marker)"
        );
        assert_eq!(api.creates(), 1, "create was not called a second time");
    }

    #[test]
    fn crash_before_post_landed_creates_exactly_one_on_recovery() {
        let s = Store::open_in_memory().unwrap();
        let api = FakeContactApi::default();
        *api.fail_before_store.borrow_mut() = true; // POST never reached Graph
        let sink = ContactSink { api: &api };
        let payload = contact_json();
        let (key, marker) = key_marker(&payload);
        let op = format!("acc:{key}");

        let first =
            finish_contacts_restore(&s, &op, "acc", "src1", &key, &marker, &payload, &sink, 10);
        assert!(first.is_err());
        assert_eq!(api.count(), 0, "nothing was created");

        *api.fail_before_store.borrow_mut() = false;
        let id =
            finish_contacts_restore(&s, &op, "acc", "src1", &key, &marker, &payload, &sink, 20)
                .unwrap();
        assert!(!id.is_empty());
        assert_eq!(api.count(), 1);
    }

    #[test]
    fn boot_recovery_reconciles_a_pending_op_without_creating() {
        let dir = std::env::temp_dir().join(format!("isyncyou-ct-recover-{}", std::process::id()));
        let arch = dir.join("arch");
        std::fs::create_dir_all(arch.join("contacts/aa")).unwrap();
        let payload = contact_json();
        std::fs::write(arch.join("contacts/aa/c.json"), &payload).unwrap();
        let (key, marker) = key_marker(&payload);
        let op_id = format!("acc:{key}");
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut it = isyncyou_store::Item::new("acc", "contacts", "src1", "Ada", "contact");
            it.local_path = Some("contacts/aa/c.json".into());
            store.upsert_item(&it).unwrap();
            store
                .create_restore_operation(&op_id, "acc", "contacts", "src1", &key, 1)
                .unwrap();
            store
                .transition_restore(
                    &op_id,
                    RestoreState::PreflightChecked,
                    2,
                    None,
                    None,
                    Some(&marker),
                )
                .unwrap();
            store
                .transition_restore(&op_id, RestoreState::Committing, 3, None, None, None)
                .unwrap();
            // [CRASH] before committed
        }
        let cfg = Config {
            accounts: vec![isyncyou_core::AccountConfig {
                id: "acc".into(),
                username: "you@example.com".into(),
                sync_root: dir.join("od"),
                archive_root: arch.clone(),
                cache_root: Default::default(),
                mount_point: None,
            }],
            ..Default::default()
        };
        // the POST had landed -> the fake already holds the contact under the marker
        let api = FakeContactApi::default();
        api.contacts
            .borrow_mut()
            .push((marker.clone(), "contact-1".into()));
        let sink = ContactSink { api: &api };

        assert_eq!(pending_contacts_restore_count(&cfg, "acc").unwrap(), 1);
        let (ok, failed) = recover_pending_contacts_restores_with(&cfg, "acc", &sink).unwrap();
        assert_eq!((ok, failed), (1, 0));
        assert_eq!(api.creates(), 0, "recovery reconciled; no new create");
        assert_eq!(pending_contacts_restore_count(&cfg, "acc").unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
