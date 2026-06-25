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

/// The Graph operations a crash-safe mail restore needs, abstracted so the
/// ledger wiring + the post-create state re-apply (#562) are unit-tested without
/// a network.
pub trait MailApi {
    /// Create a message from full MIME; returns the new cloud id.
    fn create_message(&self, mime: &[u8]) -> Result<String, String>;
    /// Find a message by its `internetMessageId`; returns its cloud id if present.
    fn find_by_message_id(&self, message_id: &str) -> Result<Option<String>, String>;
    /// Mark a restored message read/unread.
    fn set_read(&self, id: &str, is_read: bool) -> Result<(), String>;
    /// Set a restored message's follow-up flag (`flagged` / `complete`).
    fn set_flag(&self, id: &str, status: &str) -> Result<(), String>;
    /// Restore a message's categories.
    fn set_categories(&self, id: &str, categories: &[String]) -> Result<(), String>;
    /// Restore a message's importance (`low` / `high`).
    fn set_importance(&self, id: &str, importance: &str) -> Result<(), String>;
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
    fn set_read(&self, id: &str, is_read: bool) -> Result<(), String> {
        isyncyou_graph::GraphClient::set_read(self, id, is_read)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    fn set_flag(&self, id: &str, status: &str) -> Result<(), String> {
        isyncyou_graph::GraphClient::set_flag(self, id, status, None, "UTC")
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    fn set_categories(&self, id: &str, categories: &[String]) -> Result<(), String> {
        isyncyou_graph::GraphClient::set_categories(self, id, categories)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    fn set_importance(&self, id: &str, importance: &str) -> Result<(), String> {
        isyncyou_graph::GraphClient::set_importance(self, id, importance)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// The archived MAPI state a restore re-applies to the new message (#562), parsed
/// from the `<id>.json` sidecar. Only non-default values are pushed (matching the
/// reference `server.py` restore), so a plain message triggers no PATCH.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailRestoreState {
    pub categories: Vec<String>,
    pub is_read: Option<bool>,
    pub flag_status: Option<String>,
    pub importance: Option<String>,
}

impl MailRestoreState {
    /// Parse the structured state from an archived message JSON.
    pub fn from_json(o: &serde_json::Value) -> Self {
        MailRestoreState {
            categories: o["categories"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|c| c.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            is_read: o["isRead"].as_bool(),
            flag_status: o["flag"]["flagStatus"].as_str().map(String::from),
            importance: o["importance"].as_str().map(String::from),
        }
    }

    /// Re-apply the non-default fields to the freshly-created message via `api`.
    /// Idempotent (Graph PATCHes), so safe on a committed-restore replay.
    pub fn apply<A: MailApi>(&self, api: &A, id: &str) -> Result<(), String> {
        if !self.categories.is_empty() {
            api.set_categories(id, &self.categories)?;
        }
        // A MIME-created message starts unread; only an explicit read needs a PATCH.
        if self.is_read == Some(true) {
            api.set_read(id, true)?;
        }
        if let Some(s) = self.flag_status.as_deref() {
            if s != "notFlagged" {
                api.set_flag(id, s)?;
            }
        }
        if let Some(imp) = self.importance.as_deref() {
            if imp != "normal" {
                api.set_importance(id, imp)?;
            }
        }
        Ok(())
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
    let (item, bytes) = crate::read_archived_body(cfg, account, "mail", id)?;
    let secret = load_or_create_secret(&acc.archive_root.join(".isyncyou-restore-secret"))?;
    let key = idempotency_key(&secret, account, "mail", id, &bytes);
    let op_id = format!("{account}:{key}");
    let marker = mail_marker(&key);
    let store =
        Store::open(acc.archive_root.join(".isyncyou-store.db")).map_err(|e| e.to_string())?;
    let client = isyncyou_graph::GraphClient::new(token);
    let sink = MailSink { api: &client };
    let new_id = finish_mail_restore(
        &store,
        &op_id,
        account,
        id,
        &key,
        &marker,
        &bytes,
        &sink,
        now_secs(),
    )?;
    // Re-apply the archived MAPI state (#562). The message is already created (in
    // Drafts — we never send it, so a restored draft stays a draft, AC-N). The
    // PATCHes are idempotent, so a committed-restore replay self-heals.
    read_mail_restore_state(acc, &item).apply(&client, &new_id)?;
    Ok(new_id)
}

/// Read the archived `<id>.json` sidecar beside the `.eml` and parse the MAPI
/// state to re-apply. Best-effort: a missing/unreadable sidecar yields the
/// default (no PATCH), so restore still works against an older archive.
fn read_mail_restore_state(
    acc: &isyncyou_core::AccountConfig,
    item: &isyncyou_store::Item,
) -> MailRestoreState {
    item.local_path
        .as_deref()
        .and_then(|p| p.strip_suffix(".eml"))
        .map(|s| format!("{s}.json"))
        .and_then(|jrel| std::fs::read(acc.archive_root.join(jrel)).ok())
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .map(|o| MailRestoreState::from_json(&o))
        .unwrap_or_default()
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

/// How many non-terminal **mail** restore operations are pending for `account`.
/// Cheap (no token, no network) — the daemon uses it to decide whether boot
/// recovery is worth resolving a token for.
pub fn pending_mail_restore_count(cfg: &Config, account: &str) -> Result<usize, String> {
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
        .filter(|o| o.service == "mail")
        .count())
}

/// Read one archived mail item's MIME from an already-open store (no second
/// `Store::open`, so it is safe to call while holding the store).
fn archived_mail_bytes(
    store: &Store,
    acc: &isyncyou_core::AccountConfig,
    source_id: &str,
) -> Result<Vec<u8>, String> {
    let item = store
        .get_item(&acc.id, "mail", source_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no archived mail item '{source_id}'"))?;
    let rel = item
        .local_path
        .ok_or_else(|| format!("item '{source_id}' has no archived body"))?;
    std::fs::read(acc.archive_root.join(&rel)).map_err(|e| e.to_string())
}

/// Drive every pending mail restore operation for `account` to a terminal state
/// using `sink` (reconcile by marker, or resume) — the boot-recovery core, with the
/// cloud abstracted so it is testable. Returns `(recovered, still_failing)`.
pub fn recover_pending_mail_restores_with<S: RestoreSink>(
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
    for op in ops.into_iter().filter(|o| o.service == "mail") {
        let res = archived_mail_bytes(&store, acc, &op.source_item_id)
            .and_then(|bytes| recover_restore_op(&store, &op.op_id, &bytes, sink, now).map(|_| ()));
        match res {
            Ok(()) => ok += 1,
            Err(_) => failed += 1,
        }
    }
    Ok((ok, failed))
}

/// Boot recovery against the live Graph using `token`. Thin wrapper over
/// [`recover_pending_mail_restores_with`].
pub fn recover_pending_mail_restores(
    cfg: &Config,
    account: &str,
    token: String,
) -> Result<(usize, usize), String> {
    let client = isyncyou_graph::GraphClient::new(token);
    let sink = MailSink { api: &client };
    recover_pending_mail_restores_with(cfg, account, &sink)
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
        // recorded post-create PATCHes (#562 state re-apply)
        patched_read: RefCell<Option<bool>>,
        patched_flag: RefCell<Option<String>>,
        patched_categories: RefCell<Vec<String>>,
        patched_importance: RefCell<Option<String>>,
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
        fn set_read(&self, _id: &str, is_read: bool) -> Result<(), String> {
            *self.patched_read.borrow_mut() = Some(is_read);
            Ok(())
        }
        fn set_flag(&self, _id: &str, status: &str) -> Result<(), String> {
            *self.patched_flag.borrow_mut() = Some(status.to_string());
            Ok(())
        }
        fn set_categories(&self, _id: &str, categories: &[String]) -> Result<(), String> {
            *self.patched_categories.borrow_mut() = categories.to_vec();
            Ok(())
        }
        fn set_importance(&self, _id: &str, importance: &str) -> Result<(), String> {
            *self.patched_importance.borrow_mut() = Some(importance.to_string());
            Ok(())
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
    fn boot_recovery_reconciles_a_pending_op_without_creating() {
        // A mail restore that crashed after the POST landed leaves a `committing`
        // op on disk. Boot recovery must reconcile it (find by marker) and create
        // nothing new.
        let dir = std::env::temp_dir().join(format!("isyncyou-recover-{}", std::process::id()));
        let arch = dir.join("arch");
        std::fs::create_dir_all(arch.join("mail/aa")).unwrap();
        std::fs::write(arch.join("mail/aa/m.eml"), MIME).unwrap();
        let (key, marker) = key_marker();
        let op_id = format!("acc:{key}");
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut it = isyncyou_store::Item::new("acc", "mail", "src1", "Quarterly", "message");
            it.local_path = Some("mail/aa/m.eml".into());
            store.upsert_item(&it).unwrap();
            store
                .create_restore_operation(&op_id, "acc", "mail", "src1", &key, 1)
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
                mount_point: None,
            }],
            ..Default::default()
        };
        // the POST had landed -> the fake already holds the message under the marker
        let api = FakeMailApi::default();
        api.msgs.borrow_mut().push((marker.clone(), "msg-1".into()));
        let sink = MailSink { api: &api };

        assert_eq!(pending_mail_restore_count(&cfg, "acc").unwrap(), 1);
        let (ok, failed) = recover_pending_mail_restores_with(&cfg, "acc", &sink).unwrap();
        assert_eq!((ok, failed), (1, 0));
        assert_eq!(api.creates(), 0, "recovery reconciled; no new create");
        assert_eq!(pending_mail_restore_count(&cfg, "acc").unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn mail_restore_state_from_json_parses_structured_fields() {
        let o = serde_json::json!({
            "categories": ["Red category", "Work"],
            "isRead": true,
            "flag": { "flagStatus": "flagged" },
            "importance": "high",
        });
        let s = MailRestoreState::from_json(&o);
        assert_eq!(
            s.categories,
            vec!["Red category".to_string(), "Work".into()]
        );
        assert_eq!(s.is_read, Some(true));
        assert_eq!(s.flag_status.as_deref(), Some("flagged"));
        assert_eq!(s.importance.as_deref(), Some("high"));
    }

    #[test]
    fn apply_patches_non_default_state_after_create() {
        let api = FakeMailApi::default();
        let s = MailRestoreState {
            categories: vec!["Red category".into()],
            is_read: Some(true),
            flag_status: Some("flagged".into()),
            importance: Some("high".into()),
        };
        s.apply(&api, "new-1").unwrap();
        assert_eq!(
            *api.patched_categories.borrow(),
            vec!["Red category".to_string()]
        );
        assert_eq!(*api.patched_read.borrow(), Some(true));
        assert_eq!(api.patched_flag.borrow().as_deref(), Some("flagged"));
        assert_eq!(api.patched_importance.borrow().as_deref(), Some("high"));
    }

    #[test]
    fn apply_skips_default_state_and_never_sends() {
        // AC-N: a restored message stays a draft — apply has no send path, and
        // create-default values (unread / notFlagged / normal / no categories)
        // trigger no PATCH at all.
        let api = FakeMailApi::default();
        let s = MailRestoreState {
            categories: vec![],
            is_read: Some(false),
            flag_status: Some("notFlagged".into()),
            importance: Some("normal".into()),
        };
        s.apply(&api, "new-2").unwrap();
        assert!(api.patched_categories.borrow().is_empty());
        assert_eq!(*api.patched_read.borrow(), None);
        assert!(api.patched_flag.borrow().is_none());
        assert!(api.patched_importance.borrow().is_none());
    }
}
