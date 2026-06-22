//! OneNote connector — page full-list reconcile into the store (plan §6).
//!
//! OneNote has **no delta query**, so this fetches the full page list
//! (`/me/onenote/pages`, paged) and reconciles it against the store:
//! - each live page is upserted, **skipped if its `lastModifiedDateTime` is
//!   unchanged** (the cheap incremental signal OneNote gives us instead of a
//!   delta token);
//! - pages present in the store but **absent from the fresh list** are
//!   tombstoned (a full list is authoritative without a delta cursor).
//!
//! Pages are stored id-based (service `"onenote"`, `item_type = "page"`),
//! grouped by their parent section id. Page HTML + resources are a download
//! concern handled elsewhere; this connector tracks the page index. Delegated
//! access only (no app-only); read app keeps `Notes.Read`.

use crate::archive::JsonFetcher;
use crate::common::{fetch_pages, shard_path};
use crate::onedrive::SyncError;
use isyncyou_graph::Transport;
use isyncyou_store::{Item, Store};
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;

const SERVICE: &str = "onenote";
// The default OneNote page projection is already rich (title, createdDateTime,
// lastModifiedDateTime, level, order, userTags, links{oneNoteClientUrl/WebUrl},
// parentSection{id,displayName}, parentNotebook{id,displayName}) — so no narrowing
// `$select` (which would have to re-list the parent navigation props and risks
// dropping them). The full page JSON is archived to the `_pagemeta_<id>` sidecar.
const PAGES_URL: &str = "https://graph.microsoft.com/v1.0/me/onenote/pages?$top=100";

/// Archive a page's rich Graph metadata JSON to `onenote/<shard>/_pagemeta_<id>.json`
/// (atomic tmp+rename) so the webui can surface level/order/userTags/createdDateTime/
/// links without parsing the page HTML (the page's `local_path` is the `.html` body).
fn write_page_meta(archive_root: &Path, id: &str, page: &Value) -> Result<(), SyncError> {
    let abs = shard_path(archive_root, SERVICE, &format!("_pagemeta_{id}"), "json");
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(page).map_err(|e| SyncError::Malformed(e.to_string()))?;
    let tmp = abs.with_extension("json.part");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &abs)?;
    Ok(())
}

/// What one OneNote sync changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OneNoteReport {
    /// Pages seen in the live list this run.
    pub pages: usize,
    /// Pages newly stored or whose `lastModifiedDateTime` changed.
    pub upserted: usize,
    /// Pages unchanged since the last run (mtime match).
    pub unchanged: usize,
    /// Pages tombstoned because they vanished from the live list.
    pub deleted: usize,
}

/// Reconcile the full OneNote page list into `store`. `now` is the RFC3339
/// tombstone timestamp.
pub fn incremental_sync_onenote<T: Transport>(
    transport: &mut T,
    store: &Store,
    account: &str,
    now: &str,
    archive_root: Option<&Path>,
) -> Result<OneNoteReport, SyncError> {
    let pages = fetch_pages(transport, PAGES_URL)?;
    let prior: HashSet<String> = store
        .live_remote_ids(account, SERVICE)?
        .into_iter()
        .collect();
    let mut report = OneNoteReport {
        pages: pages.len(),
        ..Default::default()
    };
    let mut live: HashSet<String> = HashSet::with_capacity(pages.len());

    for page in &pages {
        let id = match page.get("id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => return Err(SyncError::Malformed("page has no id".into())),
        };
        live.insert(id.clone());

        // Archive the page's rich metadata sidecar each pass (cheap, keeps it fresh)
        // — independent of the unchanged-skip below, so every live page is covered.
        if let Some(root) = archive_root {
            write_page_meta(root, &id, page)?;
        }

        let mtime = page
            .get("lastModifiedDateTime")
            .and_then(Value::as_str)
            .map(String::from);

        // Skip unchanged pages (present, not tombstoned, same mtime).
        if let Some(existing) = store.get_item(account, SERVICE, &id)? {
            if existing.deleted_at.is_none() && existing.remote_mtime == mtime {
                report.unchanged += 1;
                continue;
            }
        }

        let title = page
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("(untitled page)");
        let mut it = Item::new(account, SERVICE, &id, title, "page");
        it.parent_remote_id = page
            .pointer("/parentSection/id")
            .and_then(Value::as_str)
            .map(String::from);
        it.remote_mtime = mtime;
        it.sync_state = "remote_dirty".into();
        store.upsert_item(&it)?;
        report.upserted += 1;
    }

    // Reconcile deletions: anything we had that the live list no longer contains.
    for gone in prior.difference(&live) {
        store.mark_deleted(account, SERVICE, gone, now)?;
        report.deleted += 1;
    }
    Ok(report)
}

/// What one OneNote hierarchy backup did (#568 B-A2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OneNoteHierarchyReport {
    pub notebooks: usize,
    pub section_groups: usize,
    pub sections: usize,
    pub bytes: u64,
}

/// Upsert a hierarchy container as a store Item (with its `parent_remote_id`) and
/// archive its full JSON to `onenote/<shard>/<id>.json` (atomic tmp+rename),
/// recording `local_path`. Mirrors the todo/calendar `archive_json_item` flank
/// pattern, plus a parent link so the UI can render the tree.
#[allow(clippy::too_many_arguments)]
fn archive_hierarchy_item(
    store: &Store,
    account: &str,
    archive_root: &Path,
    id: &str,
    name: &str,
    item_type: &str,
    parent: Option<&str>,
    value: &Value,
) -> Result<u64, SyncError> {
    let mut it = Item::new(account, SERVICE, id, name, item_type);
    it.parent_remote_id = parent.map(String::from);
    it.sync_state = "remote_dirty".into();
    store.upsert_item(&it)?;

    let abs = shard_path(archive_root, SERVICE, id, "json");
    if let Some(parent_dir) = abs.parent() {
        std::fs::create_dir_all(parent_dir)?;
    }
    let bytes = serde_json::to_vec(value).map_err(|e| SyncError::Malformed(e.to_string()))?;
    let tmp = abs.with_extension("json.part");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &abs)?;
    let rel = abs.strip_prefix(archive_root).unwrap_or(&abs);
    store.set_local_path(account, SERVICE, id, Some(&rel.to_string_lossy()))?;
    Ok(bytes.len() as u64)
}

/// A container's parent id: a section / section-group sits under another section
/// group (`parentSectionGroup`) or directly under a notebook (`parentNotebook`).
fn container_parent(v: &Value) -> Option<&str> {
    v.pointer("/parentSectionGroup/id")
        .and_then(Value::as_str)
        .or_else(|| v.pointer("/parentNotebook/id").and_then(Value::as_str))
}

/// Back up the OneNote **structure** (#568): notebooks → section groups → sections
/// as store items (`item_type` `notebook`/`section-group`/`section`) with their
/// parent chain + full JSON sidecars, so the UI can render the notebook→section→
/// page tree (pages already point at their `parentSection`). Re-fetched each pass
/// (small data). Sections/groups nest via `container_parent`.
pub fn backup_onenote_hierarchy<F: JsonFetcher>(
    fetcher: &F,
    store: &Store,
    account: &str,
    archive_root: &Path,
) -> Result<OneNoteHierarchyReport, SyncError> {
    let mut report = OneNoteHierarchyReport::default();
    let values = |v: &Value| -> Vec<Value> {
        v.get("value")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };

    // Notebooks — the roots of the tree.
    let nbs = fetcher
        .fetch_json("/me/onenote/notebooks?$top=100")
        .map_err(SyncError::Remote)?;
    for nb in values(&nbs) {
        let Some(id) = nb.get("id").and_then(Value::as_str) else {
            continue;
        };
        let name = nb.get("displayName").and_then(Value::as_str).unwrap_or(id);
        report.bytes += archive_hierarchy_item(
            store,
            account,
            archive_root,
            id,
            name,
            "notebook",
            None,
            &nb,
        )?;
        report.notebooks += 1;
    }

    // Section groups (optional intermediate level).
    let sgs = fetcher
        .fetch_json("/me/onenote/sectionGroups?$top=100")
        .map_err(SyncError::Remote)?;
    for sg in values(&sgs) {
        let Some(id) = sg.get("id").and_then(Value::as_str) else {
            continue;
        };
        let name = sg.get("displayName").and_then(Value::as_str).unwrap_or(id);
        let parent = container_parent(&sg);
        report.bytes += archive_hierarchy_item(
            store,
            account,
            archive_root,
            id,
            name,
            "section-group",
            parent,
            &sg,
        )?;
        report.section_groups += 1;
    }

    // Sections — pages hang off these (page.parent_remote_id == section.id).
    let secs = fetcher
        .fetch_json("/me/onenote/sections?$top=100")
        .map_err(SyncError::Remote)?;
    for sec in values(&secs) {
        let Some(id) = sec.get("id").and_then(Value::as_str) else {
            continue;
        };
        let name = sec.get("displayName").and_then(Value::as_str).unwrap_or(id);
        let parent = container_parent(&sec);
        report.bytes += archive_hierarchy_item(
            store,
            account,
            archive_root,
            id,
            name,
            "section",
            parent,
            &sec,
        )?;
        report.sections += 1;
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

    fn page(id: &str, title: &str, mtime: &str, section: &str) -> Value {
        json!({
            "id": id,
            "title": title,
            "lastModifiedDateTime": mtime,
            "parentSection": { "id": section }
        })
    }

    #[test]
    fn ingests_pages_with_section_parent() {
        let store = Store::open_in_memory().unwrap();
        let mut t = MockTransport(
            vec![Response::ok(json!({ "value": [
                page("p1","Ideas","2026-01-01T00:00:00Z","S1"),
                page("p2","Notes","2026-01-02T00:00:00Z","S1"),
            ] }))],
            0,
        );
        let r =
            incremental_sync_onenote(&mut t, &store, "acc", "2026-06-02T00:00:00Z", None).unwrap();
        assert_eq!(r.pages, 2);
        assert_eq!(r.upserted, 2);
        assert_eq!(r.deleted, 0);
        let p1 = store.get_item("acc", SERVICE, "p1").unwrap().unwrap();
        assert_eq!(p1.name, "Ideas");
        assert_eq!(p1.item_type, "page");
        assert_eq!(p1.parent_remote_id.as_deref(), Some("S1"));
        assert_eq!(p1.remote_mtime.as_deref(), Some("2026-01-01T00:00:00Z"));
    }

    #[test]
    fn writes_page_metadata_sidecar_with_rich_fields() {
        let store = Store::open_in_memory().unwrap();
        let arch = tempfile::tempdir().unwrap();
        let rich = json!({
            "id": "p1", "title": "Ideas", "lastModifiedDateTime": "2026-01-01T00:00:00Z",
            "createdDateTime": "2025-12-01T00:00:00Z", "level": 0, "order": 2,
            "userTags": ["important"],
            "links": { "oneNoteWebUrl": { "href": "https://onenote.com/p1" } },
            "parentSection": { "id": "S1", "displayName": "Sec" },
            "parentNotebook": { "id": "N1", "displayName": "Note" }
        });
        let mut t = MockTransport(vec![Response::ok(json!({ "value": [rich] }))], 0);
        incremental_sync_onenote(&mut t, &store, "acc", "t", Some(arch.path())).unwrap();
        let p1 = store.get_item("acc", SERVICE, "p1").unwrap().unwrap();
        assert_eq!(p1.parent_remote_id.as_deref(), Some("S1"));
        // the _pagemeta_ sidecar carries the rich fields the UI surfaces
        let abs = shard_path(arch.path(), SERVICE, "_pagemeta_p1", "json");
        let body: Value = serde_json::from_slice(&std::fs::read(&abs).unwrap()).unwrap();
        assert_eq!(body.get("level").and_then(Value::as_i64), Some(0));
        assert_eq!(body.get("order").and_then(Value::as_i64), Some(2));
        assert_eq!(
            body.pointer("/userTags/0").and_then(Value::as_str),
            Some("important")
        );
        assert_eq!(
            body.pointer("/links/oneNoteWebUrl/href")
                .and_then(Value::as_str),
            Some("https://onenote.com/p1")
        );
        assert_eq!(
            body.pointer("/parentNotebook/id").and_then(Value::as_str),
            Some("N1")
        );
        assert_eq!(
            body.get("createdDateTime").and_then(Value::as_str),
            Some("2025-12-01T00:00:00Z")
        );
    }

    struct HierFetcher;
    impl JsonFetcher for HierFetcher {
        fn fetch_json(&self, url: &str) -> std::result::Result<Value, String> {
            if url.contains("/notebooks") {
                Ok(json!({ "value": [{ "id": "N1", "displayName": "Personal" }] }))
            } else if url.contains("/sectionGroups") {
                Ok(
                    json!({ "value": [{ "id": "G1", "displayName": "Group", "parentNotebook": { "id": "N1" } }] }),
                )
            } else if url.contains("/sections") {
                Ok(json!({ "value": [
                    { "id": "S1", "displayName": "Ideas", "parentNotebook": { "id": "N1" } },
                    { "id": "S2", "displayName": "Sub", "parentSectionGroup": { "id": "G1" } },
                ]}))
            } else {
                Ok(json!({ "value": [] }))
            }
        }
    }

    #[test]
    fn backup_onenote_hierarchy_stores_notebooks_groups_sections_with_parents() {
        let store = Store::open_in_memory().unwrap();
        let arch = tempfile::tempdir().unwrap();
        let r = backup_onenote_hierarchy(&HierFetcher, &store, "acc", arch.path()).unwrap();
        assert_eq!((r.notebooks, r.section_groups, r.sections), (1, 1, 2));

        let nb = store.get_item("acc", SERVICE, "N1").unwrap().unwrap();
        assert_eq!(nb.item_type, "notebook");
        assert_eq!(nb.parent_remote_id, None);
        let g1 = store.get_item("acc", SERVICE, "G1").unwrap().unwrap();
        assert_eq!(g1.item_type, "section-group");
        assert_eq!(g1.parent_remote_id.as_deref(), Some("N1"));
        let s1 = store.get_item("acc", SERVICE, "S1").unwrap().unwrap();
        assert_eq!(s1.item_type, "section");
        assert_eq!(s1.parent_remote_id.as_deref(), Some("N1"));
        // S2 nests under the section group, not the notebook directly
        let s2 = store.get_item("acc", SERVICE, "S2").unwrap().unwrap();
        assert_eq!(s2.parent_remote_id.as_deref(), Some("G1"));
        // the section sidecar carries the full JSON (displayName)
        let body: Value = serde_json::from_slice(
            &std::fs::read(arch.path().join(s1.local_path.unwrap())).unwrap(),
        )
        .unwrap();
        assert_eq!(
            body.get("displayName").and_then(Value::as_str),
            Some("Ideas")
        );
    }

    #[test]
    fn unchanged_pages_are_skipped_on_second_run() {
        let store = Store::open_in_memory().unwrap();
        let pages = json!({ "value": [page("p1","Ideas","2026-01-01T00:00:00Z","S1")] });
        let mut t1 = MockTransport(vec![Response::ok(pages.clone())], 0);
        let r1 = incremental_sync_onenote(&mut t1, &store, "acc", "t", None).unwrap();
        assert_eq!(r1.upserted, 1);
        // identical list again -> same mtime -> skipped
        let mut t2 = MockTransport(vec![Response::ok(pages)], 0);
        let r2 = incremental_sync_onenote(&mut t2, &store, "acc", "t", None).unwrap();
        assert_eq!(r2.upserted, 0);
        assert_eq!(r2.unchanged, 1);
        // a changed mtime -> re-upserted
        let mut t3 = MockTransport(
            vec![Response::ok(
                json!({ "value": [page("p1","Ideas (edited)","2026-02-09T00:00:00Z","S1")] }),
            )],
            0,
        );
        let r3 = incremental_sync_onenote(&mut t3, &store, "acc", "t", None).unwrap();
        assert_eq!(r3.upserted, 1);
        assert_eq!(r3.unchanged, 0);
        assert_eq!(
            store.get_item("acc", SERVICE, "p1").unwrap().unwrap().name,
            "Ideas (edited)"
        );
    }

    #[test]
    fn pages_absent_from_live_list_are_tombstoned() {
        let store = Store::open_in_memory().unwrap();
        let mut t1 = MockTransport(
            vec![Response::ok(json!({ "value": [
                page("p1","Keep","2026-01-01T00:00:00Z","S1"),
                page("p2","Drop","2026-01-01T00:00:00Z","S1"),
            ] }))],
            0,
        );
        incremental_sync_onenote(&mut t1, &store, "acc", "t", None).unwrap();
        // second run: p2 gone from the list
        let mut t2 = MockTransport(
            vec![Response::ok(
                json!({ "value": [page("p1","Keep","2026-01-01T00:00:00Z","S1")] }),
            )],
            0,
        );
        let r =
            incremental_sync_onenote(&mut t2, &store, "acc", "2026-06-02T00:00:00Z", None).unwrap();
        assert_eq!(r.deleted, 1);
        assert!(store
            .get_item("acc", SERVICE, "p2")
            .unwrap()
            .unwrap()
            .deleted_at
            .is_some());
        assert!(store
            .get_item("acc", SERVICE, "p1")
            .unwrap()
            .unwrap()
            .deleted_at
            .is_none());
    }

    #[test]
    fn paginates_the_page_list() {
        let store = Store::open_in_memory().unwrap();
        let mut t = MockTransport(
            vec![
                Response::ok(
                    json!({ "value": [page("p1","A","2026-01-01T00:00:00Z","S1")], "@odata.nextLink": "u2" }),
                ),
                Response::ok(json!({ "value": [page("p2","B","2026-01-01T00:00:00Z","S1")] })),
            ],
            0,
        );
        let r = incremental_sync_onenote(&mut t, &store, "acc", "t", None).unwrap();
        assert_eq!(r.pages, 2);
        assert_eq!(r.upserted, 2);
    }

    /// Live: real OneNote page list -> store, against the throwaway account.
    /// Needs feature `http` + `ISYNCYOU_TEST_TOKEN` carrying `Notes.Read`.
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_incremental_sync_onenote() {
        let _gate = crate::live_test_gate();
        let token = match std::env::var("ISYNCYOU_TEST_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!("skipping live_incremental_sync_onenote: ISYNCYOU_TEST_TOKEN not set");
                return;
            }
        };
        let store = Store::open_in_memory().unwrap();
        let mut client = isyncyou_graph::GraphClient::new(token);
        let report = incremental_sync_onenote(
            &mut client,
            &store,
            "testuser",
            "2026-06-02T00:00:00Z",
            None,
        )
        .expect("live onenote sync should succeed");
        // If the account has any pages, they must be in the store now.
        if report.pages > 0 {
            assert_eq!(report.upserted + report.unchanged, report.pages);
        }
        eprintln!(
            "live onenote sync: pages={} upserted={} unchanged={} deleted={}",
            report.pages, report.upserted, report.unchanged, report.deleted
        );
    }
}
