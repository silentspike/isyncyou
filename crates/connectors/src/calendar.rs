//! Calendar connector — per-calendar `calendarView/delta` into the store (plan §6).
//!
//! Calendar delta is **date-range bound and per calendar**: list the user's
//! calendars, then walk `/me/calendars/{id}/calendarView/delta?startDateTime=…&
//! endDateTime=…` for each, with a per-calendar cursor (`scope = calendar id`).
//! The window is caller-supplied (the daemon uses e.g. −5/+3 years); events are
//! stored id-based (service `"calendar"`, `item_type = "event"`). No `$select`
//! on a delta query (Graph rejects it); the canonical record is the raw JSON,
//! `.ics` is only an export concern handled elsewhere.

use crate::common::fetch_pages;
use crate::onedrive::SyncError;
use isyncyou_graph::{run_delta, DeltaCursor, Transport};
use isyncyou_store::{Item, Store};
use serde_json::Value;

const SERVICE: &str = "calendar";
const CALENDARS_URL: &str = "https://graph.microsoft.com/v1.0/me/calendars?$top=100";

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

    /// Live: real per-calendar calendarView delta -> store, against the throwaway
    /// account. Needs feature `http` + `ISYNCYOU_TEST_TOKEN` carrying
    /// `Calendars.Read`.
    #[cfg(feature = "http")]
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
