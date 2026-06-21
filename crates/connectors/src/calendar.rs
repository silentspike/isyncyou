//! Calendar connector — per-calendar `calendarView/delta` into the store (plan §6).
//!
//! Calendar delta is **date-range bound and per calendar**: list the user's
//! calendars, then walk `/me/calendars/{id}/calendarView/delta?startDateTime=…&
//! endDateTime=…` for each, with a per-calendar cursor (`scope = calendar id`).
//! The window is caller-supplied (the daemon uses e.g. −5/+3 years); events are
//! stored id-based (service `"calendar"`, `item_type = "event"`). No `$select`
//! on a delta query (Graph rejects it); the canonical record is the raw JSON,
//! `.ics` is only an export concern handled elsewhere.

use crate::archive::{ArchiveReport, JsonFetcher};
use crate::common::{fetch_pages, shard_path};
use crate::onedrive::SyncError;
use isyncyou_graph::{run_delta, DeltaCursor, Transport};
use isyncyou_store::{Item, Store};
use serde_json::Value;
use std::path::Path;

const SERVICE: &str = "calendar";
const CALENDARS_URL: &str = "https://graph.microsoft.com/v1.0/me/calendars?$top=100";

/// Upsert a JSON-snapshot store item under `service="calendar"` and archive its
/// canonical JSON to `calendar/<shard>/<id>.json` (atomic tmp+rename), recording
/// the relative path as `local_path`. Shared by the calendar-flank snapshots
/// (calendars / groups / permissions). Returns the byte count written.
fn archive_json_item(
    store: &Store,
    account: &str,
    archive_root: &Path,
    id: &str,
    name: &str,
    item_type: &str,
    value: &Value,
) -> Result<u64, SyncError> {
    let mut it = Item::new(account, SERVICE, id, name, item_type);
    it.sync_state = "remote_dirty".into();
    store.upsert_item(&it)?;

    let abs = shard_path(archive_root, SERVICE, id, "json");
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(value).map_err(|e| SyncError::Malformed(e.to_string()))?;
    let tmp = abs.with_extension("json.part");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &abs)?;
    let rel = abs.strip_prefix(archive_root).unwrap_or(&abs);
    store.set_local_path(account, SERVICE, id, Some(&rel.to_string_lossy()))?;
    Ok(bytes.len() as u64)
}

/// Back up the calendar **entity** flanks (#565 B1): one `item_type="calendar"`
/// snapshot per calendar from `/me/calendars`, archiving the full calendar JSON
/// (so the UI can colour-code events by each calendar's `hexColor`/`color`).
/// Re-fetched each pass (small data). #565 B3 extends this with calendar groups,
/// per-calendar permissions and event attachments.
pub fn backup_calendar_flanks<F: JsonFetcher>(
    fetcher: &F,
    store: &Store,
    account: &str,
    archive_root: &Path,
) -> Result<ArchiveReport, SyncError> {
    let mut report = ArchiveReport::default();

    let cals = fetcher
        .fetch_json("/me/calendars?$top=100")
        .map_err(SyncError::Remote)?;
    for cal in cals
        .get("value")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = cal.get("id").and_then(Value::as_str) else {
            continue;
        };
        let name = cal.get("name").and_then(Value::as_str).unwrap_or(id);
        report.bytes += archive_json_item(store, account, archive_root, id, name, "calendar", cal)?;
        report.archived += 1;
    }

    Ok(report)
}

/// What one calendar sync changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CalendarReport {
    pub calendars: usize,
    pub upserted: usize,
    pub deleted: usize,
    pub skipped: usize,
    /// Series-master events fetched separately (calendarView returns only the
    /// expanded occurrences, not the recurring master with its recurrence rule).
    pub masters: usize,
}

struct Calendar {
    id: String,
    name: String,
}

fn parse_calendars(raw: &[Value]) -> Vec<Calendar> {
    raw.iter()
        .filter_map(|c| {
            let id = c.get("id").and_then(Value::as_str)?.to_string();
            let name = c
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Some(Calendar { id, name })
        })
        .collect()
}

/// Sync every calendar's events within `[window_start, window_end)` incrementally
/// into `store`. The window bounds are RFC3339 strings supplied by the caller so
/// this stays deterministic/testable; `now` is the tombstone timestamp.
pub fn incremental_sync_calendar<T: Transport>(
    transport: &mut T,
    store: &Store,
    account: &str,
    window_start: &str,
    window_end: &str,
    now: &str,
) -> Result<CalendarReport, SyncError> {
    // Outlook immutable-ID policy (plan §6): stable ids + UTC times.
    transport.set_prefer_immutable_id(true);
    let raw = fetch_pages(transport, CALENDARS_URL)?;
    let calendars = parse_calendars(&raw);
    let mut report = CalendarReport {
        calendars: calendars.len(),
        ..Default::default()
    };

    for cal in &calendars {
        // Record the calendar itself so events can be grouped/restored under it.
        let mut ci = Item::new(account, SERVICE, &cal.id, &cal.name, "calendar");
        ci.sync_state = "remote_dirty".into();
        store.upsert_item(&ci)?;

        let base = format!(
            "https://graph.microsoft.com/v1.0/me/calendars/{}/calendarView/delta?startDateTime={}&endDateTime={}",
            cal.id, window_start, window_end
        );
        let cursor = store
            .get_delta_cursor(account, SERVICE, &cal.id)?
            .map(DeltaCursor::new);
        let out = run_delta(transport, &base, cursor.as_ref(), 5)?;
        let mut master_ids: Vec<String> = Vec::new();
        for ev in &out.items {
            if let Some(mid) = ev.get("seriesMasterId").and_then(Value::as_str) {
                if !master_ids.iter().any(|m| m == mid) {
                    master_ids.push(mid.to_string());
                }
            }
            match ingest_event(store, account, &cal.id, ev, now)? {
                Ingest::Upserted => report.upserted += 1,
                Ingest::Deleted => report.deleted += 1,
                Ingest::Skipped => report.skipped += 1,
            }
        }
        store.set_delta_cursor(account, SERVICE, &cal.id, out.cursor.as_str())?;

        // calendarView/delta expands recurring events into occurrences but never
        // returns the series master, so fetch each referenced master once (plan §6:
        // master/instances separated). The master carries the recurrence rule; its
        // occurrences link to it via series_master_id. A master that 404s (deleted)
        // is skipped — the occurrences still carry the link.
        for mid in &master_ids {
            if store.get_item(account, SERVICE, mid)?.is_some() {
                continue;
            }
            let url = format!("https://graph.microsoft.com/v1.0/me/events/{mid}");
            let resp = transport.get(&url);
            if (200..300).contains(&resp.status) {
                if let Some(body) = resp.body {
                    if let Ingest::Upserted = ingest_event(store, account, &cal.id, &body, now)? {
                        report.masters += 1;
                    }
                }
            }
        }
    }
    Ok(report)
}

/// Sync calendars via `/me/events` (#565 B2, the default model): list each
/// calendar, then page **all** its events. Recurring series come back as their
/// MASTER (carrying the recurrence rule), never expanded into occurrences — so a
/// daily series is one stored row, not tens of thousands (AC-N). There is no
/// date window, so far-future events are captured (AC-3). Plain `/me/events` has
/// no Graph delta, so deletions are reconciled by set-difference against the
/// current id list per calendar.
pub fn events_sync_calendar<T: Transport>(
    transport: &mut T,
    store: &Store,
    account: &str,
    now: &str,
) -> Result<CalendarReport, SyncError> {
    transport.set_prefer_immutable_id(true);
    let raw = fetch_pages(transport, CALENDARS_URL)?;
    let calendars = parse_calendars(&raw);
    let mut report = CalendarReport {
        calendars: calendars.len(),
        ..Default::default()
    };

    for cal in &calendars {
        let mut ci = Item::new(account, SERVICE, &cal.id, &cal.name, "calendar");
        ci.sync_state = "remote_dirty".into();
        store.upsert_item(&ci)?;

        let url = format!(
            "https://graph.microsoft.com/v1.0/me/calendars/{}/events?$top=50",
            cal.id
        );
        let events = fetch_pages(transport, &url)?;
        let mut seen: Vec<String> = Vec::with_capacity(events.len());
        for ev in &events {
            if let Some(id) = ev.get("id").and_then(Value::as_str) {
                seen.push(id.to_string());
            }
            match ingest_event(store, account, &cal.id, ev, now)? {
                Ingest::Upserted => report.upserted += 1,
                Ingest::Deleted => report.deleted += 1,
                Ingest::Skipped => report.skipped += 1,
            }
        }
        // No delta on plain /me/events, so reconcile deletions: a live event under
        // this calendar that the cloud no longer lists has been removed.
        for it in store.items_by_service(account, SERVICE)? {
            if it.item_type == "event"
                && it.parent_remote_id.as_deref() == Some(cal.id.as_str())
                && it.deleted_at.is_none()
                && !seen.iter().any(|s| s == &it.remote_id)
            {
                store.mark_deleted(account, SERVICE, &it.remote_id, now)?;
                report.deleted += 1;
            }
        }
    }
    Ok(report)
}

enum Ingest {
    Upserted,
    Deleted,
    Skipped,
}

/// Ingest one `calendarView/delta` entry for a given calendar.
fn ingest_event(
    store: &Store,
    account: &str,
    calendar_id: &str,
    ev: &Value,
    now: &str,
) -> Result<Ingest, SyncError> {
    let id = ev
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| SyncError::Malformed("event has no id".into()))?;

    if ev.get("@removed").is_some() {
        // Only tombstone if the event still belongs to this calendar (events do
        // not normally move between calendars, but the guard keeps a re-add by a
        // later calendar's delta authoritative — mirrors the mail connector).
        let still_here = store
            .get_item(account, SERVICE, id)?
            .map(|it| it.parent_remote_id.as_deref() == Some(calendar_id))
            .unwrap_or(true);
        if still_here {
            store.mark_deleted(account, SERVICE, id, now)?;
            return Ok(Ingest::Deleted);
        }
        return Ok(Ingest::Skipped);
    }

    let subject = ev
        .get("subject")
        .and_then(Value::as_str)
        .unwrap_or("(no subject)");
    let mut it = Item::new(account, SERVICE, id, subject, "event");
    it.parent_remote_id = Some(calendar_id.to_string());
    it.etag = ev
        .get("@odata.etag")
        .and_then(Value::as_str)
        .or_else(|| ev.get("changeKey").and_then(Value::as_str))
        .map(String::from);
    it.remote_mtime = ev
        .get("lastModifiedDateTime")
        .and_then(Value::as_str)
        .map(String::from);
    // Immutable-ID companions (plan §6): changeKey + iCalUId (stable across the
    // series and across exports).
    it.change_key = ev
        .get("changeKey")
        .and_then(Value::as_str)
        .map(String::from);
    it.ical_uid = ev.get("iCalUId").and_then(Value::as_str).map(String::from);
    // Series separation (plan §6): an occurrence/exception carries its master's id;
    // the master row itself and single-instance events have none.
    it.series_master_id = ev
        .get("seriesMasterId")
        .and_then(Value::as_str)
        .map(String::from);
    it.sync_state = "remote_dirty".into();
    store.upsert_item(&it)?;
    Ok(Ingest::Upserted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use isyncyou_graph::client::Response;
    use serde_json::json;

    struct MockTransport(Vec<Response>, usize);
    impl Transport for MockTransport {
        fn get(&mut self, _url: &str) -> Response {
            let r = self.0[self.1].clone();
            self.1 += 1;
            r
        }
    }

    fn cal(id: &str, name: &str) -> Value {
        json!({ "id": id, "name": name })
    }
    fn event(id: &str, subject: &str) -> Value {
        json!({
            "id": id,
            "subject": subject,
            "@odata.etag": "W/\"EV\"",
            "lastModifiedDateTime": "2026-02-03T04:05:06Z"
        })
    }
    fn removed(id: &str) -> Value {
        json!({ "id": id, "@removed": { "reason": "deleted" } })
    }

    const WIN_S: &str = "2021-01-01T00:00:00Z";
    const WIN_E: &str = "2029-01-01T00:00:00Z";

    #[test]
    fn separates_series_master_from_its_occurrences() {
        // calendarView returns occurrences (each carrying seriesMasterId) but not
        // the master; the connector fetches the master separately (plan §6).
        let store = Store::open_in_memory().unwrap();
        let occ = json!({
            "id": "OCC1", "subject": "Standup", "type": "occurrence",
            "seriesMasterId": "MASTER1",
            "@odata.etag": "W/\"O\"", "lastModifiedDateTime": "2026-02-03T04:05:06Z"
        });
        let master = json!({
            "id": "MASTER1", "subject": "Standup", "type": "seriesMaster",
            "@odata.etag": "W/\"M\"", "lastModifiedDateTime": "2026-01-01T00:00:00Z",
            "recurrence": { "pattern": { "type": "daily", "interval": 1 } }
        });
        let mut t = MockTransport(
            vec![
                Response::ok(json!({ "value": [cal("C1", "Calendar")] })),
                Response::ok(json!({ "value": [occ], "@odata.deltaLink": "DL" })),
                // the separate GET /me/events/MASTER1
                Response::ok(master),
            ],
            0,
        );
        let r =
            incremental_sync_calendar(&mut t, &store, "acc", WIN_S, WIN_E, "2026-06-03T00:00:00Z")
                .unwrap();
        assert_eq!(r.upserted, 1, "the occurrence");
        assert_eq!(r.masters, 1, "the series master fetched separately");

        // occurrence links to its master
        let occ = store.get_item("acc", SERVICE, "OCC1").unwrap().unwrap();
        assert_eq!(occ.series_master_id.as_deref(), Some("MASTER1"));
        // master is stored as its own event, with no master of its own
        let m = store.get_item("acc", SERVICE, "MASTER1").unwrap().unwrap();
        assert_eq!(m.item_type, "event");
        assert!(m.series_master_id.is_none(), "the master has no master");
        assert_eq!(m.parent_remote_id.as_deref(), Some("C1"));
    }

    #[test]
    fn ingests_calendars_events_and_per_calendar_cursors() {
        let store = Store::open_in_memory().unwrap();
        let mut t = MockTransport(
            vec![
                Response::ok(json!({ "value": [cal("C1","Calendar"), cal("C2","Work")] })),
                Response::ok(
                    json!({ "value": [event("e1","Standup"), event("e2","1:1")], "@odata.deltaLink": "DC1" }),
                ),
                Response::ok(
                    json!({ "value": [event("e3","Release")], "@odata.deltaLink": "DC2" }),
                ),
            ],
            0,
        );
        let r =
            incremental_sync_calendar(&mut t, &store, "acc", WIN_S, WIN_E, "2026-06-02T00:00:00Z")
                .unwrap();
        assert_eq!(r.calendars, 2);
        assert_eq!(r.upserted, 3);

        let c1 = store.get_item("acc", SERVICE, "C1").unwrap().unwrap();
        assert_eq!(c1.name, "Calendar");
        assert_eq!(c1.item_type, "calendar");
        let e1 = store.get_item("acc", SERVICE, "e1").unwrap().unwrap();
        assert_eq!(e1.name, "Standup");
        assert_eq!(e1.item_type, "event");
        assert_eq!(e1.parent_remote_id.as_deref(), Some("C1"));
        assert_eq!(e1.remote_mtime.as_deref(), Some("2026-02-03T04:05:06Z"));
        assert_eq!(
            store
                .get_delta_cursor("acc", SERVICE, "C1")
                .unwrap()
                .as_deref(),
            Some("DC1")
        );
        assert_eq!(
            store
                .get_delta_cursor("acc", SERVICE, "C2")
                .unwrap()
                .as_deref(),
            Some("DC2")
        );
    }

    #[test]
    fn cancelled_event_is_tombstoned() {
        let store = Store::open_in_memory().unwrap();
        let mut t1 = MockTransport(
            vec![
                Response::ok(json!({ "value": [cal("C1","Calendar")] })),
                Response::ok(json!({ "value": [event("e9","Party")], "@odata.deltaLink": "D1" })),
            ],
            0,
        );
        incremental_sync_calendar(&mut t1, &store, "acc", WIN_S, WIN_E, "t").unwrap();
        let mut t2 = MockTransport(
            vec![
                Response::ok(json!({ "value": [cal("C1","Calendar")] })),
                Response::ok(json!({ "value": [removed("e9")], "@odata.deltaLink": "D2" })),
            ],
            0,
        );
        let r =
            incremental_sync_calendar(&mut t2, &store, "acc", WIN_S, WIN_E, "2026-06-02T00:00:00Z")
                .unwrap();
        assert_eq!(r.deleted, 1);
        assert!(store
            .get_item("acc", SERVICE, "e9")
            .unwrap()
            .unwrap()
            .deleted_at
            .is_some());
    }

    #[test]
    fn second_run_reuses_persisted_cursor() {
        let store = Store::open_in_memory().unwrap();
        let mut t1 = MockTransport(
            vec![
                Response::ok(json!({ "value": [cal("C1","Calendar")] })),
                Response::ok(json!({ "value": [event("e1","A")], "@odata.deltaLink": "D1" })),
            ],
            0,
        );
        incremental_sync_calendar(&mut t1, &store, "acc", WIN_S, WIN_E, "t").unwrap();
        // second run: the calendar list, then an incremental delta page from D1
        let mut t2 = MockTransport(
            vec![
                Response::ok(json!({ "value": [cal("C1","Calendar")] })),
                Response::ok(json!({ "value": [event("e2","B")], "@odata.deltaLink": "D2" })),
            ],
            0,
        );
        let r = incremental_sync_calendar(&mut t2, &store, "acc", WIN_S, WIN_E, "t").unwrap();
        assert_eq!(r.upserted, 1);
        assert_eq!(
            store
                .get_delta_cursor("acc", SERVICE, "C1")
                .unwrap()
                .as_deref(),
            Some("D2")
        );
    }

    #[test]
    fn events_mode_stores_master_and_far_future_without_explosion() {
        let store = Store::open_in_memory().unwrap();
        // /me/events returns the recurring MASTER (with its rule) + a far-future
        // single — neither expanded into occurrences.
        let master = json!({
            "id": "M1", "subject": "Daily standup", "type": "seriesMaster",
            "@odata.etag": "W/\"M\"", "lastModifiedDateTime": "2026-01-01T00:00:00Z",
            "recurrence": { "pattern": { "type": "daily", "interval": 1 }, "range": { "type": "noEnd" } }
        });
        let far = json!({
            "id": "F1", "subject": "2040 plan", "type": "singleInstance",
            "@odata.etag": "W/\"F\"", "lastModifiedDateTime": "2040-06-01T00:00:00Z"
        });
        let mut t = MockTransport(
            vec![
                Response::ok(json!({ "value": [cal("C1", "Calendar")] })),
                Response::ok(json!({ "value": [master, far] })), // one page, no nextLink
            ],
            0,
        );
        let r = events_sync_calendar(&mut t, &store, "acc", "2026-06-21T00:00:00Z").unwrap();
        assert_eq!(r.upserted, 2);
        // the daily series is exactly ONE stored row (the master), not occurrences
        let m = store.get_item("acc", SERVICE, "M1").unwrap().unwrap();
        assert_eq!(m.item_type, "event");
        assert!(m.series_master_id.is_none(), "the master has no master");
        // the 2040 event is captured despite no date window
        assert!(store.get_item("acc", SERVICE, "F1").unwrap().is_some());
        let events = store
            .items_by_service("acc", SERVICE)
            .unwrap()
            .into_iter()
            .filter(|i| i.item_type == "event")
            .count();
        assert_eq!(events, 2, "no occurrence explosion (AC-N)");
    }

    #[test]
    fn events_mode_reconciles_deletions() {
        let store = Store::open_in_memory().unwrap();
        let mut t1 = MockTransport(
            vec![
                Response::ok(json!({ "value": [cal("C1", "Calendar")] })),
                Response::ok(json!({ "value": [event("e1", "A"), event("e2", "B")] })),
            ],
            0,
        );
        events_sync_calendar(&mut t1, &store, "acc", "t").unwrap();
        // second pass: e2 is gone from the listing -> reconciled as deleted
        let mut t2 = MockTransport(
            vec![
                Response::ok(json!({ "value": [cal("C1", "Calendar")] })),
                Response::ok(json!({ "value": [event("e1", "A")] })),
            ],
            0,
        );
        let r = events_sync_calendar(&mut t2, &store, "acc", "2026-06-21T00:00:00Z").unwrap();
        assert_eq!(r.deleted, 1);
        assert!(store
            .get_item("acc", SERVICE, "e2")
            .unwrap()
            .unwrap()
            .deleted_at
            .is_some());
        assert!(store
            .get_item("acc", SERVICE, "e1")
            .unwrap()
            .unwrap()
            .deleted_at
            .is_none());
    }

    struct MockJsonFetcher(Value);
    impl JsonFetcher for MockJsonFetcher {
        fn fetch_json(&self, _url: &str) -> std::result::Result<Value, String> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn backup_calendar_flanks_snapshots_calendars_with_colour() {
        let store = Store::open_in_memory().unwrap();
        let arch = tempfile::tempdir().unwrap();
        let fetcher = MockJsonFetcher(json!({ "value": [
            { "id": "C1", "name": "Calendar", "hexColor": "#FF0000", "color": "lightRed", "isDefaultCalendar": true },
            { "id": "C2", "name": "Work", "hexColor": "#00AA00", "color": "lightGreen" },
        ]}));
        let r = backup_calendar_flanks(&fetcher, &store, "acc", arch.path()).unwrap();
        assert_eq!(r.archived, 2);
        let c1 = store.get_item("acc", SERVICE, "C1").unwrap().unwrap();
        assert_eq!(c1.item_type, "calendar");
        let rel = c1.local_path.expect("sidecar path recorded");
        let bytes = std::fs::read(arch.path().join(&rel)).unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v.get("hexColor").and_then(Value::as_str), Some("#FF0000"));
        assert_eq!(
            v.get("isDefaultCalendar").and_then(Value::as_bool),
            Some(true)
        );
    }

    /// Live: real per-calendar calendarView delta -> store, against the throwaway
    /// account. Needs feature `http` + `ISYNCYOU_TEST_TOKEN` carrying
    /// `Calendars.Read`.
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_incremental_sync_calendar() {
        let _gate = crate::live_test_gate();
        let token = match std::env::var("ISYNCYOU_TEST_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!("skipping live_incremental_sync_calendar: ISYNCYOU_TEST_TOKEN not set");
                return;
            }
        };
        let store = Store::open_in_memory().unwrap();
        let mut client = isyncyou_graph::GraphClient::new(token);
        let report = incremental_sync_calendar(
            &mut client,
            &store,
            "testuser",
            "2019-01-01T00:00:00Z",
            "2030-01-01T00:00:00Z",
            "2026-06-02T00:00:00Z",
        )
        .expect("live calendar sync should succeed");
        assert!(report.calendars > 0, "expected at least one calendar");
        eprintln!(
            "live calendar sync: calendars={} upserted={} deleted={} skipped={}",
            report.calendars, report.upserted, report.deleted, report.skipped
        );
    }
}
