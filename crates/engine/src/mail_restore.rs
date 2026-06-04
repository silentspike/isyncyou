//! Crash-safe **mail** restore: a [`RestoreSink`] backed by Microsoft Graph, plus
//! the ledger-driven entry point that `restore_cloud` uses for mail.
//!
//! This is the integration of the proven ledger (ADR-001) into the live restore
//! path for the mail vertical slice: every cloud-mutating mail restore now records
//! an operation, stamps a findable `Message-ID` marker derived from the content
//! HMAC, and — on a re-entry after a crash — reconciles by probing Graph for that
//! marker instead of blindly re-posting.
//!
//! The two Graph calls are behind [`MailApi`] so the wiring + recovery are
//! unit-tested deterministically without a network; `GraphClient` is the real impl.

use crate::restore_key::{idempotency_key, load_or_create_secret, mail_marker};
use crate::restore_recovery::{recover_restore_op, run_restore_op, RestoreSink};
use isyncyou_core::Config;
use isyncyou_store::{RestoreState, Store};

/// The two Graph operations a crash-safe mail restore needs, abstracted so the
/// ledger wiring can be exercised without a network.
pub trait MailApi {
    /// Create a message from full MIME; returns the new cloud id.
    fn create_message(&self, mime: &[u8]) -> Result<String, String>;
    /// Find a message by its `internetMessageId`; returns its cloud id if present.
    fn find_by_message_id(&self, message_id: &str) -> Result<Option<String>, String>;
}

impl MailApi for isyncyou_graph::GraphClient {
    fn create_message(&self, mime: &[u8]) -> Result<String, String> {
        let v = self
            .create_message_from_mime(mime)
            .map_err(|e| e.to_string())?;
        v.get("id")
            .and_then(|i| i.as_str())
            .map(String::from)
            .ok_or_else(|| "created message response has no id".to_string())
    }
    fn find_by_message_id(&self, message_id: &str) -> Result<Option<String>, String> {
        let url = format!(
            "/me/messages?$filter=internetMessageId eq '{}'&$select=id&$top=1",
            encode_filter_value(message_id)
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

/// Minimal percent-encoding for the marker inside an OData `$filter` value.
fn encode_filter_value(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'<' => o.push_str("%3C"),
            b'>' => o.push_str("%3E"),
            b'@' => o.push_str("%40"),
            b' ' => o.push_str("%20"),
            b'\'' => o.push_str("%27"),
            _ => o.push(b as char),
        }
    }
    o
}

/// A [`RestoreSink`] for mail: `create` stamps the marker as the MIME `Message-ID`
/// then posts; `find_by_marker` probes Graph by `internetMessageId`.
pub struct MailSink<'a, A: MailApi> {
    pub api: &'a A,
}

impl<A: MailApi> RestoreSink for MailSink<'_, A> {
    fn create(&self, marker: &str, payload: &[u8]) -> Result<String, String> {
        let mime = isyncyou_connectors::set_message_id(payload, marker);
        self.api.create_message(&mime)
    }
    fn find_by_marker(&self, marker: &str) -> Result<Option<String>, String> {
        self.api.find_by_message_id(marker)
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Restore one archived mail item to the cloud **through the operation ledger**.
/// Idempotent: a repeat of the same content recognises the existing operation and
/// either returns the committed id or reconciles an interrupted one — never a
/// duplicate. Returns the new cloud id.
pub fn restore_mail_via_ledger(
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
    let (_item, bytes) = crate::read_archived_body(cfg, account, "mail", id)?;
    let secret = load_or_create_secret(&acc.archive_root.join(".isyncyou-restore-secret"))?;
    let key = idempotency_key(&secret, account, "mail", id, &bytes);
    let op_id = format!("{account}:{key}");
    let marker = mail_marker(&key);
    let store =
        Store::open(acc.archive_root.join(".isyncyou-store.db")).map_err(|e| e.to_string())?;
    let client = isyncyou_graph::GraphClient::new(token);
    let sink = MailSink { api: &client };
    finish_mail_restore(
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
fn finish_mail_restore<S: RestoreSink>(
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
                .create_restore_operation(op_id, account, "mail", source_id, key, now)
                .map_err(|e| e.to_string())?;
            let (new_id, _) = run_restore_op(store, op_id, marker, payload, sink, now)?;
            Ok(new_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A fake Graph mailbox. `create_message` stores the message keyed by the
    /// `Message-ID` parsed out of the posted MIME (so it exercises the real
    /// `set_message_id` + parser), and is deliberately non-idempotent. Flags
    /// simulate the two dangerous crash interleavings.
    #[derive(Default)]
    struct FakeMailApi {
        msgs: RefCell<Vec<(String, String)>>, // (message-id, cloud id)
        seq: RefCell<u32>,
        creates: RefCell<u32>,
        crash_after_store: RefCell<bool>, // POST landed, response lost
        fail_before_store: RefCell<bool>, // POST never reached Graph
    }
    impl FakeMailApi {
        fn count(&self) -> usize {
            self.msgs.borrow().len()
        }
        fn creates(&self) -> u32 {
            *self.creates.borrow()
        }
    }
    impl MailApi for FakeMailApi {
        fn create_message(&self, mime: &[u8]) -> Result<String, String> {
            *self.creates.borrow_mut() += 1;
            if *self.fail_before_store.borrow() {
                return Err("network failed before reaching Graph".into());
            }
            let mid = isyncyou_connectors::mail_preview(mime)
                .message_id
                .unwrap_or_default();
            let mut seq = self.seq.borrow_mut();
            *seq += 1;
            let id = format!("msg-{}", *seq);
            self.msgs.borrow_mut().push((mid, id.clone()));
            if *self.crash_after_store.borrow() {
                return Err("network dropped after create".into());
            }
            Ok(id)
        }
        fn find_by_message_id(&self, message_id: &str) -> Result<Option<String>, String> {
            Ok(self
                .msgs
                .borrow()
                .iter()
                .find(|(m, _)| m == message_id)
                .map(|(_, id)| id.clone()))
        }
    }

    const MIME: &[u8] = b"Subject: Quarterly\r\nFrom: a@example.com\r\n\r\nthe body";

    fn key_marker() -> (String, String) {
        let key = idempotency_key(b"secret", "acc", "mail", "src1", MIME);
        let marker = mail_marker(&key);
        (key, marker)
    }

    #[test]
    fn happy_path_creates_one_and_is_idempotent_on_repeat() {
        let s = Store::open_in_memory().unwrap();
        let api = FakeMailApi::default();
        let sink = MailSink { api: &api };
        let (key, marker) = key_marker();
        let op = format!("acc:{key}");

        let id1 =
            finish_mail_restore(&s, &op, "acc", "src1", &key, &marker, MIME, &sink, 10).unwrap();
        // a repeat of identical content must return the same id, no second create
        let id2 =
            finish_mail_restore(&s, &op, "acc", "src1", &key, &marker, MIME, &sink, 20).unwrap();
        assert_eq!(id1, id2);
        assert_eq!(api.count(), 1);
        assert_eq!(api.creates(), 1);
    }

    #[test]
    fn crash_after_post_landed_does_not_duplicate() {
        let s = Store::open_in_memory().unwrap();
        let api = FakeMailApi::default();
        *api.crash_after_store.borrow_mut() = true; // POST lands, then connection drops
        let sink = MailSink { api: &api };
        let (key, marker) = key_marker();
        let op = format!("acc:{key}");

        // first attempt: the message is created in the cloud, then the call errors
        let first = finish_mail_restore(&s, &op, "acc", "src1", &key, &marker, MIME, &sink, 10);
        assert!(first.is_err());
        assert_eq!(api.count(), 1, "the POST landed");

        // recovery attempt: must find the message by marker and NOT create a second
        *api.crash_after_store.borrow_mut() = false;
        let id =
            finish_mail_restore(&s, &op, "acc", "src1", &key, &marker, MIME, &sink, 20).unwrap();
        assert!(!id.is_empty());
        assert_eq!(api.count(), 1, "no duplicate after recovery");
        assert_eq!(api.creates(), 1, "create was not called a second time");
    }

    #[test]
    fn crash_before_post_landed_creates_exactly_one_on_recovery() {
        let s = Store::open_in_memory().unwrap();
        let api = FakeMailApi::default();
        *api.fail_before_store.borrow_mut() = true; // POST never reached Graph
        let sink = MailSink { api: &api };
        let (key, marker) = key_marker();
        let op = format!("acc:{key}");

        let first = finish_mail_restore(&s, &op, "acc", "src1", &key, &marker, MIME, &sink, 10);
        assert!(first.is_err());
        assert_eq!(api.count(), 0, "nothing was created");

        // recovery: marker not found -> safe to create exactly one
        *api.fail_before_store.borrow_mut() = false;
        let id =
            finish_mail_restore(&s, &op, "acc", "src1", &key, &marker, MIME, &sink, 20).unwrap();
        assert!(!id.is_empty());
        assert_eq!(api.count(), 1);
    }

    #[test]
    fn marker_round_trips_through_set_message_id_and_parser() {
        // The fake keys messages by the Message-ID parsed from the posted MIME, so a
        // green happy-path test already proves set_message_id stamped the marker that
        // find_by_marker later searches for. Assert it explicitly too.
        let (_key, marker) = key_marker();
        let stamped = isyncyou_connectors::set_message_id(MIME, &marker);
        let parsed = isyncyou_connectors::mail_preview(&stamped).message_id;
        assert_eq!(parsed.as_deref(), Some(marker.as_str()));
    }
}
