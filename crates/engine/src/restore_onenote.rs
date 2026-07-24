//! Crash-safe **OneNote** restore: a [`RestoreSink`] backed by Microsoft Graph
//! OneNote, plus the ledger-driven entry point that `restore_cloud` uses for OneNote.
//!
//! ## Why OneNote is the weakest probe (but clean)
//!
//! A live probe (`tools/live_onenote_probe.py`) established two workable mechanisms:
//! a marker in the page **title** found by `$filter`, and an **HTML comment** marker in
//! the page body found by listing pages and scanning each page's `/content`. We use the
//! **comment** marker: it is *invisible* when rendered (cleaner fidelity than ToDo's
//! visible body marker), at the cost of an O(n) content scan on the rare crash recovery.
//!
//! A correct restore is also **resource-aware**: the archived page HTML keeps the
//! original Graph resource URLs, with binaries archived under `onenote_resources/` and a
//! `<page>.resources.json` manifest. So restore rewrites each URL to a multipart
//! `name:<part>` reference and re-uploads the bytes — never re-fetching the source page's
//! (possibly-expired) URLs. The Graph calls are behind [`OneNoteApi`] so the wiring +
//! recovery are unit-tested deterministically; `GraphClient` is the real impl.

use crate::restore_key::{idempotency_key, load_or_create_secret, onenote_marker};
use crate::restore_recovery::{
    recover_restore_op, run_restore_op, RestoreError, RestoreResult, RestoreSink,
};
use isyncyou_connectors::OneNoteResourcePart;
use isyncyou_core::Config;
use isyncyou_store::{RestoreState, Store};
use std::path::Path;

/// The two Graph operations a crash-safe OneNote restore needs, abstracted so the
/// ledger wiring can be exercised without a network.
pub trait OneNoteApi {
    /// Create a page from POST-ready HTML (already marker-stamped and rewritten to
    /// `name:<part>` references) plus its binary parts; returns the new cloud id.
    /// `section_id` places the page into its **original** section (#568); `None` (or
    /// a section that no longer exists) falls back to the default section. Uses a
    /// multipart create when `resources` is non-empty, else a plain HTML create.
    fn create_page(
        &self,
        section_id: Option<&str>,
        html: &[u8],
        resources: &[OneNoteResourcePart],
    ) -> Result<String, String>;
    /// Find a page whose content contains `marker` by listing pages (all pages) and
    /// scanning each page's `/content`; returns its cloud id if present.
    fn find_by_marker(&self, marker: &str) -> Result<Option<String>, String>;

    fn create_page_for_restore(
        &self,
        section_id: Option<&str>,
        html: &[u8],
        resources: &[OneNoteResourcePart],
    ) -> RestoreResult<String> {
        self.create_page(section_id, html, resources)
            .map_err(RestoreError::internal)
    }
    fn find_by_marker_for_restore(&self, marker: &str) -> RestoreResult<Option<String>> {
        self.find_by_marker(marker).map_err(RestoreError::internal)
    }
}

impl OneNoteApi for isyncyou_graph::GraphClient {
    fn create_page(
        &self,
        section_id: Option<&str>,
        html: &[u8],
        resources: &[OneNoteResourcePart],
    ) -> Result<String, String> {
        let parts: Vec<_> = resources
            .iter()
            .map(|r| isyncyou_graph::http::OneNotePagePart {
                name: r.part_name.clone(),
                content_type: r.content_type.clone(),
                bytes: r.bytes.clone(),
            })
            .collect();
        // Create in `sec` (None = default section), JSON-or-error from the graph layer.
        let do_create = |sec: Option<&str>| match (sec, parts.is_empty()) {
            (Some(s), true) => self.create_onenote_page_in_section(s, html),
            (Some(s), false) => self.create_onenote_page_in_section_multipart(s, html, &parts),
            (None, true) => self.create_onenote_page(html),
            (None, false) => self.create_onenote_page_multipart(html, &parts),
        };
        let created = match section_id {
            Some(sec) => match do_create(Some(sec)) {
                Ok(v) => v,
                // the original section is gone -> restore into the default section
                Err(isyncyou_graph::http::UploadError::Http { status: 404, .. }) => {
                    do_create(None).map_err(|e| e.to_string())?
                }
                Err(e) => return Err(e.to_string()),
            },
            None => do_create(None).map_err(|e| e.to_string())?,
        };
        created
            .get("id")
            .and_then(|i| i.as_str())
            .map(String::from)
            .ok_or_else(|| "created page response has no id".to_string())
    }
    fn find_by_marker(&self, marker: &str) -> Result<Option<String>, String> {
        // No content $filter exists, so page through every page and scan its /content.
        // Follow @odata.nextLink to the end — never cap silently.
        let mut url = "/me/onenote/pages?$select=id&$top=100".to_string();
        loop {
            let page = self.get_json(&url).map_err(|e| e.to_string())?;
            if let Some(pages) = page.get("value").and_then(|v| v.as_array()) {
                for p in pages {
                    let Some(id) = p.get("id").and_then(|i| i.as_str()) else {
                        continue;
                    };
                    let content = self
                        .get_bytes(&format!("/me/onenote/pages/{id}/content"))
                        .map_err(|e| e.to_string())?;
                    if String::from_utf8_lossy(&content).contains(marker) {
                        return Ok(Some(id.to_string()));
                    }
                }
            }
            match page.get("@odata.nextLink").and_then(|l| l.as_str()) {
                Some(next) => url = next.to_string(),
                None => return Ok(None),
            }
        }
    }

    fn create_page_for_restore(
        &self,
        section_id: Option<&str>,
        html: &[u8],
        resources: &[OneNoteResourcePart],
    ) -> RestoreResult<String> {
        let parts: Vec<_> = resources
            .iter()
            .map(|resource| isyncyou_graph::http::OneNotePagePart {
                name: resource.part_name.clone(),
                content_type: resource.content_type.clone(),
                bytes: resource.bytes.clone(),
            })
            .collect();
        let do_create = |section: Option<&str>| match (section, parts.is_empty()) {
            (Some(id), true) => self.create_onenote_page_in_section(id, html),
            (Some(id), false) => self.create_onenote_page_in_section_multipart(id, html, &parts),
            (None, true) => self.create_onenote_page(html),
            (None, false) => self.create_onenote_page_multipart(html, &parts),
        };
        let created = match section_id {
            Some(section) => match do_create(Some(section)) {
                Ok(value) => value,
                Err(isyncyou_graph::http::UploadError::Http { status: 404, .. }) => {
                    do_create(None).map_err(RestoreError::from_graph)?
                }
                Err(error) => return Err(RestoreError::from_graph(error)),
            },
            None => do_create(None).map_err(RestoreError::from_graph)?,
        };
        created
            .get("id")
            .and_then(|id| id.as_str())
            .map(String::from)
            .ok_or_else(|| RestoreError::invalid("created page response has no id"))
    }

    fn find_by_marker_for_restore(&self, marker: &str) -> RestoreResult<Option<String>> {
        let mut url = "/me/onenote/pages?$select=id&$top=100".to_string();
        loop {
            let page = self.get_json(&url).map_err(RestoreError::from_graph)?;
            if let Some(pages) = page.get("value").and_then(|value| value.as_array()) {
                for page in pages {
                    let Some(id) = page.get("id").and_then(|value| value.as_str()) else {
                        continue;
                    };
                    let content = self
                        .get_bytes(&format!("/me/onenote/pages/{id}/content"))
                        .map_err(RestoreError::from_graph)?;
                    if String::from_utf8_lossy(&content).contains(marker) {
                        return Ok(Some(id.to_string()));
                    }
                }
            }
            match page.get("@odata.nextLink").and_then(|link| link.as_str()) {
                Some(next) => url = next.to_string(),
                None => return Ok(None),
            }
        }
    }
}

/// A [`RestoreSink`] for OneNote. `create` rewrites resource URLs to `name:<part>`,
/// stamps the invisible HTML-comment marker, then posts (multipart if it has resources);
/// `find_by_marker` scans page content.
pub struct OneNoteSink<'a, A: OneNoteApi> {
    pub api: &'a A,
    /// (source_url, part_name) rewrites derived from the page's resource manifest.
    pub rewrites: Vec<(String, String)>,
    /// The binary resource parts to upload alongside the page.
    pub resources: Vec<OneNoteResourcePart>,
    /// The page's original section id (its `parent_remote_id`); restore targets it,
    /// falling back to the default section if it's gone (#568). `None` = default.
    pub section: Option<String>,
}

/// Rewrite each archived resource URL to its multipart `name:<part>` reference and
/// stamp the marker as an HTML comment just before `</body>` (or appended if absent).
fn prepare_html(html: &[u8], rewrites: &[(String, String)], marker: &str) -> Vec<u8> {
    let mut s = String::from_utf8_lossy(html).into_owned();
    for (url, part) in rewrites {
        s = s.replace(url, &format!("name:{part}"));
    }
    let comment = format!("<!--{marker}-->");
    match find_ascii_case_insensitive(&s, "</body>") {
        Some(idx) => s.insert_str(idx, &comment),
        None => s.push_str(&comment),
    }
    s.into_bytes()
}

/// Find the byte index of a case-insensitive ASCII needle in `haystack`.
fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

impl<A: OneNoteApi> RestoreSink for OneNoteSink<'_, A> {
    fn create(&self, marker: &str, payload: &[u8]) -> RestoreResult<String> {
        let html = prepare_html(payload, &self.rewrites, marker);
        self.api
            .create_page_for_restore(self.section.as_deref(), &html, &self.resources)
    }
    fn find_by_marker(&self, marker: &str) -> RestoreResult<Option<String>> {
        self.api.find_by_marker_for_restore(marker)
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The binary parts to upload plus the (source_url -> part_name) rewrites for a page.
type PageResources = (Vec<OneNoteResourcePart>, Vec<(String, String)>);

/// Load the page's resource manifest (`<page>.resources.json`, next to the archived
/// HTML), returning the binary parts to upload and the (url -> part) rewrites. A page
/// with no manifest restores as plain HTML (empty vectors).
fn load_resources(archive_root: &Path, html_local_path: &str) -> Result<PageResources, String> {
    let manifest_path = archive_root
        .join(html_local_path)
        .with_extension("resources.json");
    let raw = match std::fs::read(&manifest_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((vec![], vec![])),
        Err(e) => return Err(e.to_string()),
    };
    let manifest: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|e| format!("resource manifest is not JSON: {e}"))?;
    let entries = manifest
        .get("resources")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    let (mut parts, mut rewrites) = (Vec::new(), Vec::new());
    for (i, e) in entries.iter().enumerate() {
        let url = e
            .get("source_url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "resource manifest entry has no source_url".to_string())?;
        let rel = e
            .get("local_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "resource manifest entry has no local_path".to_string())?;
        let content_type = e
            .get("content_type")
            .and_then(|v| v.as_str())
            .unwrap_or("application/octet-stream")
            .to_string();
        let bytes = std::fs::read(archive_root.join(rel)).map_err(|e| e.to_string())?;
        let part_name = format!("r{i}");
        rewrites.push((url.to_string(), part_name.clone()));
        parts.push(OneNoteResourcePart {
            part_name,
            content_type,
            bytes,
        });
    }
    Ok((parts, rewrites))
}

/// Restore one archived OneNote page to the cloud **through the operation ledger**.
/// Idempotent: a repeat of the same content recognises the existing operation and
/// either returns the committed id or reconciles an interrupted one (by scanning page
/// content for the marker) — never a duplicate. Returns the new cloud id.
pub fn restore_onenote_via_ledger(
    cfg: &Config,
    account: &str,
    id: &str,
    token: String,
) -> Result<String, String> {
    restore_onenote_via_ledger_classified(cfg, account, id, token)
        .map_err(|error| error.to_string())
}

pub(crate) fn restore_onenote_via_ledger_classified(
    cfg: &Config,
    account: &str,
    id: &str,
    token: String,
) -> RestoreResult<String> {
    let acc = cfg
        .accounts
        .iter()
        .find(|a| a.id == account)
        .ok_or_else(|| RestoreError::invalid(format!("no account '{account}' in config")))?;
    let (item, bytes) =
        crate::read_archived_body(cfg, account, "onenote", id).map_err(RestoreError::invalid)?;
    let html_local_path = item.local_path.clone().ok_or_else(|| {
        RestoreError::invalid(format!("onenote item '{id}' has no archived body"))
    })?;
    let section = item.parent_remote_id.clone();
    let (resources, rewrites) =
        load_resources(&acc.archive_root, &html_local_path).map_err(RestoreError::invalid)?;
    let secret = load_or_create_secret(&acc.archive_root.join(".isyncyou-restore-secret"))
        .map_err(RestoreError::internal)?;
    let key = idempotency_key(&secret, account, "onenote", id, &bytes);
    let op_id = format!("{account}:{key}");
    let marker = onenote_marker(&key);
    let store = Store::open(acc.archive_root.join(".isyncyou-store.db"))
        .map_err(|e| RestoreError::internal(e.to_string()))?;
    let client = isyncyou_graph::GraphClient::new(token);
    let sink = OneNoteSink {
        api: &client,
        rewrites,
        resources,
        section,
    };
    finish_onenote_restore(
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
fn finish_onenote_restore<S: RestoreSink>(
    store: &Store,
    op_id: &str,
    account: &str,
    source_id: &str,
    key: &str,
    marker: &str,
    payload: &[u8],
    sink: &S,
    now: i64,
) -> RestoreResult<String> {
    match store
        .get_restore_operation(op_id)
        .map_err(|e| RestoreError::internal(e.to_string()))?
    {
        Some(op) if op.state == RestoreState::Committed => op
            .new_cloud_id
            .ok_or_else(|| RestoreError::internal("committed operation has no cloud id")),
        Some(_) => {
            recover_restore_op(store, op_id, payload, sink, now)?;
            store
                .get_restore_operation(op_id)
                .map_err(|e| RestoreError::internal(e.to_string()))?
                .and_then(|o| o.new_cloud_id)
                .ok_or_else(|| RestoreError::internal("recovery did not record a cloud id"))
        }
        None => {
            store
                .create_restore_operation(op_id, account, "onenote", source_id, key, now)
                .map_err(|e| RestoreError::internal(e.to_string()))?;
            let (new_id, _) = run_restore_op(store, op_id, marker, payload, sink, now)?;
            Ok(new_id)
        }
    }
}

/// How many non-terminal **onenote** restore operations are pending for `account`.
pub fn pending_onenote_restore_count(cfg: &Config, account: &str) -> Result<usize, String> {
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
        .filter(|o| o.service == "onenote")
        .count())
}

/// Drive every pending onenote restore operation for `account` to a terminal state
/// using `api` (a sink is built per op with that page's resources) — the boot-recovery
/// core, with the cloud abstracted so it is testable. Returns `(recovered, failing)`.
pub fn recover_pending_onenote_restores_with<A: OneNoteApi>(
    cfg: &Config,
    account: &str,
    api: &A,
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
    for op in ops.into_iter().filter(|o| o.service == "onenote") {
        let res = (|| {
            let item = store
                .get_item(account, "onenote", &op.source_item_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no archived onenote item '{}'", op.source_item_id))?;
            let section = item.parent_remote_id.clone();
            let rel = item
                .local_path
                .ok_or_else(|| format!("item '{}' has no archived body", op.source_item_id))?;
            let bytes = std::fs::read(acc.archive_root.join(&rel)).map_err(|e| e.to_string())?;
            let (resources, rewrites) = load_resources(&acc.archive_root, &rel)?;
            let sink = OneNoteSink {
                api,
                rewrites,
                resources,
                section,
            };
            recover_restore_op(&store, &op.op_id, &bytes, &sink, now)
                .map(|_| ())
                .map_err(|e| e.to_string())
        })();
        match res {
            Ok(()) => ok += 1,
            Err(_) => failed += 1,
        }
    }
    Ok((ok, failed))
}

/// Boot recovery against the live Graph using `token`. Thin wrapper over
/// [`recover_pending_onenote_restores_with`].
pub fn recover_pending_onenote_restores(
    cfg: &Config,
    account: &str,
    token: String,
) -> Result<(usize, usize), String> {
    let client = isyncyou_graph::GraphClient::new(token);
    recover_pending_onenote_restores_with(cfg, account, &client)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A fake OneNote. `create_page` stores the page keyed by the marker scanned out of
    /// the posted HTML (so it exercises the real `prepare_html` stamping), and is
    /// deliberately **non-idempotent**. Flags simulate the two crash interleavings.
    #[derive(Default)]
    struct FakeOneNoteApi {
        pages: RefCell<Vec<(String, String, usize)>>, // (html, id, resource count)
        sections: RefCell<Vec<Option<String>>>,       // section id each create targeted
        seq: RefCell<u32>,
        creates: RefCell<u32>,
        crash_after_store: RefCell<bool>,
        fail_before_store: RefCell<bool>,
    }
    impl FakeOneNoteApi {
        fn count(&self) -> usize {
            self.pages.borrow().len()
        }
        fn creates(&self) -> u32 {
            *self.creates.borrow()
        }
        fn last_section(&self) -> Option<String> {
            self.sections.borrow().last().cloned().flatten()
        }
    }
    impl OneNoteApi for FakeOneNoteApi {
        fn create_page(
            &self,
            section_id: Option<&str>,
            html: &[u8],
            resources: &[OneNoteResourcePart],
        ) -> Result<String, String> {
            self.sections
                .borrow_mut()
                .push(section_id.map(String::from));
            *self.creates.borrow_mut() += 1;
            if *self.fail_before_store.borrow() {
                return Err("network failed before reaching Graph".into());
            }
            let mut seq = self.seq.borrow_mut();
            *seq += 1;
            let id = format!("page-{}", *seq);
            self.pages.borrow_mut().push((
                String::from_utf8_lossy(html).into_owned(),
                id.clone(),
                resources.len(),
            ));
            if *self.crash_after_store.borrow() {
                return Err("network dropped after create".into());
            }
            Ok(id)
        }
        fn find_by_marker(&self, marker: &str) -> Result<Option<String>, String> {
            Ok(self
                .pages
                .borrow()
                .iter()
                .find(|(html, _, _)| html.contains(marker))
                .map(|(_, id, _)| id.clone()))
        }
    }

    const HTML: &[u8] =
        b"<!DOCTYPE html><html><head><title>Notes</title></head><body><p>hi</p></body></html>";

    fn key_marker() -> (String, String) {
        let key = idempotency_key(b"secret", "acc", "onenote", "src1", HTML);
        let marker = onenote_marker(&key);
        (key, marker)
    }

    fn sink(api: &FakeOneNoteApi) -> OneNoteSink<'_, FakeOneNoteApi> {
        OneNoteSink {
            api,
            rewrites: vec![],
            resources: vec![],
            section: None,
        }
    }

    #[test]
    fn restore_targets_the_pages_original_section() {
        let api = FakeOneNoteApi::default();
        let s = OneNoteSink {
            api: &api,
            rewrites: vec![],
            resources: vec![],
            section: Some("S-orig".into()),
        };
        let (_key, marker) = key_marker();
        s.create(&marker, HTML).unwrap();
        assert_eq!(
            api.last_section().as_deref(),
            Some("S-orig"),
            "the page is created in its original section, not the default"
        );
    }

    #[test]
    fn prepare_html_inserts_comment_before_body_close_and_rewrites_urls() {
        let html = b"<html><body><img src=\"https://graph/res/1\"></body></html>";
        let out = prepare_html(html, &[("https://graph/res/1".into(), "r0".into())], "MARK");
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("src=\"name:r0\""), "url rewritten: {s}");
        assert!(
            s.contains("<!--MARK--></body>"),
            "comment before </body>: {s}"
        );
    }

    #[test]
    fn prepare_html_appends_comment_when_no_body_close() {
        let out = prepare_html(b"<p>x</p>", &[], "MARK");
        assert!(String::from_utf8(out).unwrap().ends_with("<!--MARK-->"));
    }

    #[test]
    fn happy_path_creates_one_and_is_idempotent_on_repeat() {
        let s = Store::open_in_memory().unwrap();
        let api = FakeOneNoteApi::default();
        let sink = sink(&api);
        let (key, marker) = key_marker();
        let op = format!("acc:{key}");

        let id1 =
            finish_onenote_restore(&s, &op, "acc", "src1", &key, &marker, HTML, &sink, 10).unwrap();
        let id2 =
            finish_onenote_restore(&s, &op, "acc", "src1", &key, &marker, HTML, &sink, 20).unwrap();
        assert_eq!(id1, id2);
        assert_eq!(api.count(), 1);
        assert_eq!(api.creates(), 1);
    }

    #[test]
    fn create_stamps_invisible_marker_found_by_scan() {
        let api = FakeOneNoteApi::default();
        let sink = sink(&api);
        let (_key, marker) = key_marker();
        let id = sink.create(&marker, HTML).unwrap();
        assert_eq!(api.find_by_marker(&marker).unwrap(), Some(id));
        // the stored html carries the comment marker
        assert!(api.pages.borrow()[0]
            .0
            .contains(&format!("<!--{marker}-->")));
    }

    #[test]
    fn crash_after_post_landed_does_not_duplicate() {
        let s = Store::open_in_memory().unwrap();
        let api = FakeOneNoteApi::default();
        *api.crash_after_store.borrow_mut() = true;
        let sink = sink(&api);
        let (key, marker) = key_marker();
        let op = format!("acc:{key}");

        let first = finish_onenote_restore(&s, &op, "acc", "src1", &key, &marker, HTML, &sink, 10);
        assert!(first.is_err());
        assert_eq!(api.count(), 1, "the POST landed");

        *api.crash_after_store.borrow_mut() = false;
        let id =
            finish_onenote_restore(&s, &op, "acc", "src1", &key, &marker, HTML, &sink, 20).unwrap();
        assert!(!id.is_empty());
        assert_eq!(
            api.count(),
            1,
            "no duplicate after recovery (found by content scan)"
        );
        assert_eq!(api.creates(), 1, "create was not called a second time");
    }

    #[test]
    fn crash_before_post_landed_creates_exactly_one_on_recovery() {
        let s = Store::open_in_memory().unwrap();
        let api = FakeOneNoteApi::default();
        *api.fail_before_store.borrow_mut() = true;
        let sink = sink(&api);
        let (key, marker) = key_marker();
        let op = format!("acc:{key}");

        let first = finish_onenote_restore(&s, &op, "acc", "src1", &key, &marker, HTML, &sink, 10);
        assert!(first.is_err());
        assert_eq!(api.count(), 0, "nothing was created");

        *api.fail_before_store.borrow_mut() = false;
        let id =
            finish_onenote_restore(&s, &op, "acc", "src1", &key, &marker, HTML, &sink, 20).unwrap();
        assert!(!id.is_empty());
        assert_eq!(api.count(), 1);
    }

    #[test]
    fn load_resources_builds_parts_and_rewrites_from_manifest() {
        let dir = std::env::temp_dir().join(format!("isyncyou-on-res-{}", std::process::id()));
        let arch = dir.join("arch");
        std::fs::create_dir_all(arch.join("onenote/aa")).unwrap();
        std::fs::create_dir_all(arch.join("onenote_resources/aa")).unwrap();
        std::fs::write(arch.join("onenote_resources/aa/img.png"), b"PNGDATA").unwrap();
        let html_rel = "onenote/aa/p.html";
        std::fs::write(arch.join(html_rel), HTML).unwrap();
        let manifest = serde_json::json!({
            "page_id": "p1",
            "resources": [
                { "source_url": "https://graph/res/1", "content_type": "image/png",
                  "local_path": "onenote_resources/aa/img.png" }
            ]
        });
        std::fs::write(
            arch.join("onenote/aa/p.resources.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        let (parts, rewrites) = load_resources(&arch, html_rel).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].part_name, "r0");
        assert_eq!(parts[0].content_type, "image/png");
        assert_eq!(parts[0].bytes, b"PNGDATA");
        assert_eq!(
            rewrites,
            vec![("https://graph/res/1".to_string(), "r0".to_string())]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_manifest_means_plain_html_restore() {
        let dir = std::env::temp_dir().join(format!("isyncyou-on-nores-{}", std::process::id()));
        let arch = dir.join("arch");
        std::fs::create_dir_all(arch.join("onenote/aa")).unwrap();
        std::fs::write(arch.join("onenote/aa/p.html"), HTML).unwrap();
        let (parts, rewrites) = load_resources(&arch, "onenote/aa/p.html").unwrap();
        assert!(parts.is_empty() && rewrites.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn boot_recovery_reconciles_a_pending_op_without_creating() {
        let dir = std::env::temp_dir().join(format!("isyncyou-on-recover-{}", std::process::id()));
        let arch = dir.join("arch");
        std::fs::create_dir_all(arch.join("onenote/aa")).unwrap();
        std::fs::write(arch.join("onenote/aa/p.html"), HTML).unwrap();
        let (key, marker) = key_marker();
        let op_id = format!("acc:{key}");
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut it = isyncyou_store::Item::new("acc", "onenote", "src1", "Notes", "page");
            it.local_path = Some("onenote/aa/p.html".into());
            store.upsert_item(&it).unwrap();
            store
                .create_restore_operation(&op_id, "acc", "onenote", "src1", &key, 1)
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
        // the POST had landed -> the fake already holds the page content with the marker
        let api = FakeOneNoteApi::default();
        api.pages
            .borrow_mut()
            .push((format!("<body><!--{marker}--></body>"), "page-1".into(), 0));
        assert_eq!(pending_onenote_restore_count(&cfg, "acc").unwrap(), 1);
        let (ok, failed) = recover_pending_onenote_restores_with(&cfg, "acc", &api).unwrap();
        assert_eq!((ok, failed), (1, 0));
        assert_eq!(
            api.creates(),
            0,
            "recovery reconciled by content scan; no new create"
        );
        assert_eq!(pending_onenote_restore_count(&cfg, "acc").unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
