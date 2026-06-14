//! Crash-safe **calendar** restore: a [`RestoreSink`] backed by Microsoft Graph
//! events, plus the ledger-driven entry point that `restore_cloud` uses for calendar.
//!
//! ## Why calendar differs from mail
//!
//! Mail restore is made crash-safe by the ledger **plus a marker probe**: a
//! non-idempotent `POST /me/messages` is reconciled after a crash by querying
//! `internetMessageId` (proven live in `tools/live_restore_probe.py`).
//!
//! Calendar restore is crash-safe by a **stronger, simpler** mechanism, established
//! empirically (`tools/live_calendar_probe.py`): `POST /me/events` honours a posted
//! **`transactionId`** and **de-duplicates server-side** — re-POSTing the same
//! `transactionId` returns the *same* event id, never a second event. So the create
//! itself is idempotent. (Graph does **not** support a `transactionId` `$filter`
//! query — it returns HTTP 400 — so there is no out-of-band probe, and none is
//! needed.)
//!
//! Concretely: the marker is a `transactionId` derived from the content HMAC; recovery
//! after a crash re-POSTs through the ledger and relies on Graph's de-dup to converge
//! to exactly one event. [`CalendarSink::find_by_marker`] therefore returns `None`
//! (no probe), which routes the generic [`recover_restore_op`] into the idempotent
//! re-create path. The single Graph call is behind [`CalendarApi`] so the wiring +
//! recovery are unit-tested deterministically (with a fake that models Graph's
//! transactionId de-dup); `GraphClient` is the real impl.

use crate::restore_key::{calendar_marker, idempotency_key, load_or_create_secret};
use crate::restore_recovery::{recover_restore_op, run_restore_op, RestoreSink};
use isyncyou_core::Config;
use isyncyou_store::{RestoreState, Store};
use serde_json::Value;

/// The one Graph operation a crash-safe calendar restore needs, abstracted so the
/// ledger wiring can be exercised without a network. There is deliberately no
/// "find" call: Graph has no `transactionId` query, and `create_event` is idempotent
/// (server-side de-dup on the `transactionId`), so re-creating is the safe reconcile.
pub trait CalendarApi {
    /// Create an event from a POST-ready JSON body (already sanitized and carrying the
    /// `transactionId` marker); returns the cloud id. **Idempotent**: a second call
    /// with the same `transactionId` returns the same id (Graph de-dups server-side),
    /// so this never produces a duplicate.
    fn create_event(&self, body: &Value) -> Result<String, String>;
}

impl CalendarApi for isyncyou_graph::GraphClient {
    fn create_event(&self, body: &Value) -> Result<String, String> {
        let v = self
            .post_json("/me/events", body)
            .map_err(|e| e.to_string())?;
        v.get("id")
            .and_then(|i| i.as_str())
            .map(String::from)
            .ok_or_else(|| "created event response has no id".to_string())
    }
}

/// A [`RestoreSink`] for calendar: `create` sanitizes the archived event, stamps the
/// marker as the `transactionId`, then posts (idempotently). `find_by_marker` returns
/// `None` because Graph offers no `transactionId` query — recovery relies on the
/// idempotent re-POST instead, which is strictly safer than a probe-then-create race.
pub struct CalendarSink<'a, A: CalendarApi> {
    pub api: &'a A,
}

impl<A: CalendarApi> RestoreSink for CalendarSink<'_, A> {
    fn create(&self, marker: &str, payload: &[u8]) -> Result<String, String> {
        let event: Value = serde_json::from_slice(payload)
            .map_err(|e| format!("archived event is not JSON: {e}"))?;
        let mut body = isyncyou_connectors::sanitize_event(&event);
        // The sanitizer keeps only writable event fields, so stamp the transactionId
        // marker afterwards (it would otherwise be dropped by the whitelist).
        let obj = body
            .as_object_mut()
            .ok_or_else(|| "sanitized event is not a JSON object".to_string())?;
        obj.insert(
            "transactionId".to_string(),
            Value::String(marker.to_string()),
        );
        self.api.create_event(&body)
    }
    fn find_by_marker(&self, _marker: &str) -> Result<Option<String>, String> {
        // Graph has no `transactionId` $filter (it returns HTTP 400), so there is no
        // probe. Returning None routes recovery into the idempotent re-create path,
        // where Graph's server-side transactionId de-dup guarantees exactly one event.
        Ok(None)
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Restore one archived calendar event to the cloud **through the operation ledger**.
/// Idempotent: a repeat of the same content recognises the existing operation and
/// either returns the committed id or reconciles an interrupted one — and even a
/// re-POST converges to one event via Graph's transactionId de-dup. Returns the
/// cloud id.
pub fn restore_calendar_via_ledger(
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
    let (_item, bytes) = crate::read_archived_body(cfg, account, "calendar", id)?;
    let secret = load_or_create_secret(&acc.archive_root.join(".isyncyou-restore-secret"))?;
    let key = idempotency_key(&secret, account, "calendar", id, &bytes);
    let op_id = format!("{account}:{key}");
    let marker = calendar_marker(&key);
    let store =
        Store::open(acc.archive_root.join(".isyncyou-store.db")).map_err(|e| e.to_string())?;
    let client = isyncyou_graph::GraphClient::new(token);
    let sink = CalendarSink { api: &client };
    finish_calendar_restore(
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
fn finish_calendar_restore<S: RestoreSink>(
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
        // Interrupted earlier: recover (idempotent re-create) — never blind-retry.
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
                .create_restore_operation(op_id, account, "calendar", source_id, key, now)
                .map_err(|e| e.to_string())?;
            let (new_id, _) = run_restore_op(store, op_id, marker, payload, sink, now)?;
            Ok(new_id)
        }
    }
}

/// How many non-terminal **calendar** restore operations are pending for `account`.
pub fn pending_calendar_restore_count(cfg: &Config, account: &str) -> Result<usize, String> {
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
        .filter(|o| o.service == "calendar")
        .count())
}

/// Read one archived calendar event's JSON from an already-open store.
fn archived_calendar_bytes(
    store: &Store,
    acc: &isyncyou_core::AccountConfig,
    source_id: &str,
) -> Result<Vec<u8>, String> {
    let item = store
        .get_item(&acc.id, "calendar", source_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no archived calendar item '{source_id}'"))?;
    let rel = item
        .local_path
        .ok_or_else(|| format!("item '{source_id}' has no archived body"))?;
    std::fs::read(acc.archive_root.join(&rel)).map_err(|e| e.to_string())
}

/// Drive every pending calendar restore operation for `account` to a terminal state
/// using `sink` (idempotent re-create) — the boot-recovery core, with the cloud
/// abstracted so it is testable. Returns `(recovered, still_failing)`.
pub fn recover_pending_calendar_restores_with<S: RestoreSink>(
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
    for op in ops.into_iter().filter(|o| o.service == "calendar") {
        let res = archived_calendar_bytes(&store, acc, &op.source_item_id)
            .and_then(|bytes| recover_restore_op(&store, &op.op_id, &bytes, sink, now).map(|_| ()));
        match res {
            Ok(()) => ok += 1,
            Err(_) => failed += 1,
        }
    }
    Ok((ok, failed))
}

/// Boot recovery against the live Graph using `token`. Thin wrapper over
/// [`recover_pending_calendar_restores_with`].
pub fn recover_pending_calendar_restores(
    cfg: &Config,
    account: &str,
    token: String,
) -> Result<(usize, usize), String> {
    let client = isyncyou_graph::GraphClient::new(token);
    let sink = CalendarSink { api: &client };
    recover_pending_calendar_restores_with(cfg, account, &sink)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A fake Graph calendar that models the empirically-confirmed real behaviour:
    /// `create_event` is **idempotent on the `transactionId`** (a second POST with the
    /// same value returns the same id — Graph de-dups server-side), so it never makes a
    /// duplicate. This is the safety the calendar restore relies on. Flags simulate the
    /// two dangerous crash interleavings.
    #[derive(Default)]
    struct FakeCalendarApi {
        events: RefCell<Vec<(String, String)>>, // (transactionId, cloud id)
        seq: RefCell<u32>,
        create_calls: RefCell<u32>,
        crash_after_store: RefCell<bool>, // POST landed (stored), response lost
        fail_before_store: RefCell<bool>, // POST never reached Graph
    }
    impl FakeCalendarApi {
        fn count(&self) -> usize {
            self.events.borrow().len()
        }
        fn create_calls(&self) -> u32 {
            *self.create_calls.borrow()
        }
    }
    impl CalendarApi for FakeCalendarApi {
        fn create_event(&self, body: &Value) -> Result<String, String> {
            *self.create_calls.borrow_mut() += 1;
            if *self.fail_before_store.borrow() {
                return Err("network failed before reaching Graph".into());
            }
            let txid = body
                .get("transactionId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            // Server-side de-dup: an existing transactionId returns its id, no new event.
            let existing = self
                .events
                .borrow()
                .iter()
                .find(|(t, _)| *t == txid)
                .map(|(_, id)| id.clone());
            if let Some(id) = existing {
                if *self.crash_after_store.borrow() {
                    return Err("network dropped after create".into());
                }
                return Ok(id);
            }
            let mut seq = self.seq.borrow_mut();
            *seq += 1;
            let id = format!("evt-{}", *seq);
            self.events.borrow_mut().push((txid, id.clone()));
            if *self.crash_after_store.borrow() {
                return Err("network dropped after create".into());
            }
            Ok(id)
        }
    }

    fn event_json() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "subject": "Quarterly review",
            "start": { "dateTime": "2026-07-01T09:00:00", "timeZone": "UTC" },
            "end": { "dateTime": "2026-07-01T10:00:00", "timeZone": "UTC" },
            // a non-writable field the sanitizer must drop:
            "id": "AAMkOLD==",
        }))
        .unwrap()
    }

    fn key_marker(payload: &[u8]) -> (String, String) {
        let key = idempotency_key(b"secret", "acc", "calendar", "src1", payload);
        let marker = calendar_marker(&key);
        (key, marker)
    }

    #[test]
    fn happy_path_creates_one_and_is_idempotent_on_repeat() {
        let s = Store::open_in_memory().unwrap();
        let api = FakeCalendarApi::default();
        let sink = CalendarSink { api: &api };
        let payload = event_json();
        let (key, marker) = key_marker(&payload);
        let op = format!("acc:{key}");

        let id1 =
            finish_calendar_restore(&s, &op, "acc", "src1", &key, &marker, &payload, &sink, 10)
                .unwrap();
        let id2 =
            finish_calendar_restore(&s, &op, "acc", "src1", &key, &marker, &payload, &sink, 20)
                .unwrap();
        assert_eq!(id1, id2);
        assert_eq!(api.count(), 1);
    }

    #[test]
    fn create_stamps_transaction_id_and_drops_non_writable_fields() {
        // Prove the sink stamps the marker as the transactionId (so Graph can de-dup on
        // it) and that the sanitizer dropped the read-only `id`.
        let api = FakeCalendarApi::default();
        let sink = CalendarSink { api: &api };
        let payload = event_json();
        let (_key, marker) = key_marker(&payload);
        sink.create(&marker, &payload).unwrap();
        // The stored event is keyed by exactly the marker we stamped.
        assert_eq!(api.events.borrow()[0].0, marker);
        // A second create with the same marker de-dups to one event.
        sink.create(&marker, &payload).unwrap();
        assert_eq!(api.count(), 1);
    }

    #[test]
    fn crash_after_post_landed_does_not_duplicate() {
        let s = Store::open_in_memory().unwrap();
        let api = FakeCalendarApi::default();
        *api.crash_after_store.borrow_mut() = true; // POST lands, then connection drops
        let sink = CalendarSink { api: &api };
        let payload = event_json();
        let (key, marker) = key_marker(&payload);
        let op = format!("acc:{key}");

        let first =
            finish_calendar_restore(&s, &op, "acc", "src1", &key, &marker, &payload, &sink, 10);
        assert!(first.is_err());
        assert_eq!(api.count(), 1, "the POST landed");

        *api.crash_after_store.borrow_mut() = false;
        let id =
            finish_calendar_restore(&s, &op, "acc", "src1", &key, &marker, &payload, &sink, 20)
                .unwrap();
        assert!(!id.is_empty());
        assert_eq!(
            api.count(),
            1,
            "no duplicate after recovery (transactionId de-dup)"
        );
    }

    #[test]
    fn crash_before_post_landed_creates_exactly_one_on_recovery() {
        let s = Store::open_in_memory().unwrap();
        let api = FakeCalendarApi::default();
        *api.fail_before_store.borrow_mut() = true; // POST never reached Graph
        let sink = CalendarSink { api: &api };
        let payload = event_json();
        let (key, marker) = key_marker(&payload);
        let op = format!("acc:{key}");

        let first =
            finish_calendar_restore(&s, &op, "acc", "src1", &key, &marker, &payload, &sink, 10);
        assert!(first.is_err());
        assert_eq!(api.count(), 0, "nothing was created");

        *api.fail_before_store.borrow_mut() = false;
        let id =
            finish_calendar_restore(&s, &op, "acc", "src1", &key, &marker, &payload, &sink, 20)
                .unwrap();
        assert!(!id.is_empty());
        assert_eq!(api.count(), 1);
    }

    #[test]
    fn boot_recovery_reconciles_a_pending_op_without_duplicating() {
        let dir = std::env::temp_dir().join(format!("isyncyou-cal-recover-{}", std::process::id()));
        let arch = dir.join("arch");
        std::fs::create_dir_all(arch.join("calendar/aa")).unwrap();
        let payload = event_json();
        std::fs::write(arch.join("calendar/aa/e.json"), &payload).unwrap();
        let (key, marker) = key_marker(&payload);
        let op_id = format!("acc:{key}");
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut it = isyncyou_store::Item::new("acc", "calendar", "src1", "Quarterly", "event");
            it.local_path = Some("calendar/aa/e.json".into());
            store.upsert_item(&it).unwrap();
            store
                .create_restore_operation(&op_id, "acc", "calendar", "src1", &key, 1)
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
        // the POST had landed -> the fake already holds the event under the marker
        let api = FakeCalendarApi::default();
        api.events
            .borrow_mut()
            .push((marker.clone(), "evt-1".into()));
        let sink = CalendarSink { api: &api };

        assert_eq!(pending_calendar_restore_count(&cfg, "acc").unwrap(), 1);
        let (ok, failed) = recover_pending_calendar_restores_with(&cfg, "acc", &sink).unwrap();
        assert_eq!((ok, failed), (1, 0));
        // Recovery re-POSTs; Graph de-dups on the transactionId, so still one event,
        // committed to the original id.
        assert_eq!(api.count(), 1, "no duplicate created on recovery");
        assert_eq!(api.create_calls(), 1, "exactly one idempotent re-create");
        assert_eq!(pending_calendar_restore_count(&cfg, "acc").unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
