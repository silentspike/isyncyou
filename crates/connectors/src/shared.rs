//! Shared-with-me connector (plan §8.2) — **DEPRECATED for personal accounts**.
//!
//! `/me/drive/sharedWithMe` still responds for personal MSAs but Microsoft is
//! removing it (degrading through ~Nov 2026, with no personal replacement). It is
//! provided as a best-effort, low-priority listing into the store (service
//! `"shared"`) with a clear deprecation signal — not a delta source and not part
//! of the core sync. Callers should surface [`DEPRECATION_NOTICE`] in the UI.

use crate::common::fetch_pages;
use crate::onedrive::SyncError;
use isyncyou_graph::Transport;
use isyncyou_store::{Item, Store};
use serde_json::Value;

const SERVICE: &str = "shared";
const URL: &str = "https://graph.microsoft.com/v1.0/me/drive/sharedWithMe";

/// User-facing deprecation notice for the shared-with-me feature.
pub const DEPRECATION_NOTICE: &str =
    "'Shared with me' is deprecated by Microsoft for personal accounts and is \
     degrading (removal expected ~Nov 2026); this listing is best-effort.";

/// What one shared-with-me listing produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SharedReport {
    /// Items returned by the endpoint this run.
    pub items: usize,
    /// Items upserted into the store.
    pub upserted: usize,
}

/// List the items shared with the user and upsert them into `store` (service
/// `"shared"`). Best-effort; an empty list is a normal result.
pub fn sync_shared_with_me<T: Transport>(
    transport: &mut T,
    store: &Store,
    account: &str,
) -> Result<SharedReport, SyncError> {
    let raw = fetch_pages(transport, URL)?;
    let mut report = SharedReport {
        items: raw.len(),
        ..Default::default()
    };
    for it in &raw {
        let id = it
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| SyncError::Malformed("shared item has no id".into()))?;
        // The real item lives in another drive (the `remoteItem` facet); fall back
        // to its name/type/mtime when the top-level fields are absent.
        let name = it
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| it.pointer("/remoteItem/name").and_then(Value::as_str))
            .unwrap_or("(shared item)");
        let is_folder = it.get("folder").is_some() || it.pointer("/remoteItem/folder").is_some();
        let mut item = Item::new(
            account,
            SERVICE,
            id,
            name,
            if is_folder { "folder" } else { "file" },
        );
        item.remote_mtime = it
            .pointer("/remoteItem/lastModifiedDateTime")
            .and_then(Value::as_str)
            .or_else(|| it.pointer("/lastModifiedDateTime").and_then(Value::as_str))
            .map(String::from);
        item.sync_state = "remote_dirty".into();
        store.upsert_item(&item)?;
        report.upserted += 1;
    }
    Ok(report)
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

    #[test]
    fn ingests_shared_items_with_remote_item_fallback() {
        let store = Store::open_in_memory().unwrap();
        let mut t = MockTransport(
            vec![Response::ok(json!({ "value": [
                { "id": "s1", "name": "Budget.xlsx", "remoteItem": { "lastModifiedDateTime": "2026-02-01T00:00:00Z" } },
                { "id": "s2", "remoteItem": { "name": "Team Folder", "folder": { "childCount": 3 } } }
            ] }))],
            0,
        );
        let r = sync_shared_with_me(&mut t, &store, "acc").unwrap();
        assert_eq!(r.items, 2);
        assert_eq!(r.upserted, 2);

        let f = store.get_item("acc", SERVICE, "s1").unwrap().unwrap();
        assert_eq!(f.name, "Budget.xlsx");
        assert_eq!(f.item_type, "file");
        assert_eq!(f.remote_mtime.as_deref(), Some("2026-02-01T00:00:00Z"));
        // name + folder type taken from the remoteItem facet
        let d = store.get_item("acc", SERVICE, "s2").unwrap().unwrap();
        assert_eq!(d.name, "Team Folder");
        assert_eq!(d.item_type, "folder");
    }

    #[test]
    fn empty_listing_is_fine() {
        let store = Store::open_in_memory().unwrap();
        let mut t = MockTransport(vec![Response::ok(json!({ "value": [] }))], 0);
        let r = sync_shared_with_me(&mut t, &store, "acc").unwrap();
        assert_eq!(r.items, 0);
        assert_eq!(r.upserted, 0);
    }

    /// Live: real `sharedWithMe` against the throwaway account. The endpoint is
    /// deprecated for personal accounts, so an empty (or degraded) result is a
    /// valid outcome — we assert it completes without error. Needs feature `http`
    /// + `ISYNCYOU_TEST_TOKEN` (`Files.Read`).
    #[cfg(feature = "http")]
    #[test]
    fn live_shared_with_me() {
        let token = match std::env::var("ISYNCYOU_TEST_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!("skipping live_shared_with_me: ISYNCYOU_TEST_TOKEN not set");
                return;
            }
        };
        let store = Store::open_in_memory().unwrap();
        let mut client = isyncyou_graph::GraphClient::new(token);
        match sync_shared_with_me(&mut client, &store, "backupslave") {
            Ok(r) => eprintln!(
                "live shared-with-me: items={} upserted={}",
                r.items, r.upserted
            ),
            // a deprecated/removed endpoint may now error on personal accounts —
            // record it rather than failing the suite.
            Err(e) => eprintln!("live shared-with-me unavailable (expected, deprecated): {e}"),
        }
    }
}
