//! Content archive — write each item's canonical body/resources to disk (plan §6/§9).
//!
//! The delta connectors store an *index* (metadata) in SQLite; the canonical
//! body — the JSON a restore re-creates from — lives on disk. This module
//! fetches each stored item's JSON by id and writes it (atomically, in the same
//! sharded layout as mail `.eml`) under the account's `archive_root`, recording
//! the relative path as `local_path`. It is the on-disk source the restore
//! engine reads (`restore_event`/`restore_task`/`restore_contact`).

use crate::common::shard_path;
use crate::onedrive::SyncError;
use isyncyou_store::{Item, Store};
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;

/// Fetches a Graph resource's canonical JSON by URL. Abstracted so the archive
/// driver is unit-testable with a mock and live-tested with the real client.
pub trait JsonFetcher {
    fn fetch_json(&self, url: &str) -> Result<Value, String>;
}

#[cfg(feature = "http")]
impl JsonFetcher for isyncyou_graph::GraphClient {
    fn fetch_json(&self, url: &str) -> Result<Value, String> {
        self.get_json(url).map_err(|e| e.to_string())
    }
}

/// Fetches a Graph resource's raw bytes by URL (for non-JSON bodies such as
/// OneNote page HTML). Abstracted so the archive driver is mockable.
pub trait BytesFetcher {
    fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, String>;
}

#[cfg(feature = "http")]
impl BytesFetcher for isyncyou_graph::GraphClient {
    fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
        self.get_bytes(url).map_err(|e| e.to_string())
    }
}

/// What one archive pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArchiveReport {
    /// Items whose JSON was fetched and written this pass.
    pub archived: usize,
    /// Items skipped because their body was already on disk.
    pub skipped: usize,
    /// Total bytes written this pass.
    pub bytes: u64,
}

/// What one OneNote page-resource archive pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OneNoteResourceReport {
    /// Archived OneNote pages inspected for resource references.
    pub pages: usize,
    /// Resource URLs fetched and written this pass.
    pub resources: usize,
    /// Resource files already present on disk.
    pub skipped: usize,
    /// Per-page resource manifests written this pass.
    pub manifests: usize,
    /// Total resource bytes written this pass.
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OneNoteResourceRef {
    url: String,
    content_type: Option<String>,
}

/// Shared archive core: for each stored `(service, item_type)` item without a
/// local body, fetch its bytes via `fetch`, write `<archive_root>/<service>/
/// aa/bb/<hash>.<ext>` atomically (tmp+rename), record `local_path`, skip
/// already-archived items, and honor `limit` (`0` = no limit) so it resumes.
#[allow(clippy::too_many_arguments)]
fn archive_bodies<G>(
    store: &Store,
    account: &str,
    service: &str,
    item_type: &str,
    archive_root: &Path,
    ext: &str,
    limit: usize,
    fetch: G,
) -> Result<ArchiveReport, SyncError>
where
    G: Fn(&Item) -> Result<Vec<u8>, String>,
{
    let mut report = ArchiveReport::default();
    for item in store.items_by_type(account, service, item_type)? {
        if item.local_path.is_some() {
            report.skipped += 1;
            continue;
        }
        if limit != 0 && report.archived >= limit {
            break;
        }
        let bytes = fetch(&item).map_err(SyncError::Remote)?;
        let abs = shard_path(archive_root, service, &item.remote_id, ext);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = abs.with_extension(format!("{ext}.part"));
        std::fs::write(&tmp, isyncyou_core::envelope::seal_for_disk(&bytes))?;
        std::fs::rename(&tmp, &abs)?;
        let rel = abs.strip_prefix(archive_root).unwrap_or(&abs);
        store.set_local_path(
            account,
            service,
            &item.remote_id,
            Some(&rel.to_string_lossy()),
        )?;
        report.archived += 1;
        report.bytes += bytes.len() as u64;
    }
    Ok(report)
}

/// Archive the canonical **JSON** of every stored `(service, item_type)` item
/// lacking a local body. `url_for(item)` builds the GET URL for the item's
/// canonical resource; the response is pretty-printed to a `.json` file.
#[allow(clippy::too_many_arguments)]
pub fn backup_json_bodies<F, U>(
    fetcher: &F,
    store: &Store,
    account: &str,
    service: &str,
    item_type: &str,
    archive_root: &Path,
    url_for: U,
    limit: usize,
) -> Result<ArchiveReport, SyncError>
where
    F: JsonFetcher,
    U: Fn(&Item) -> String,
{
    archive_bodies(
        store,
        account,
        service,
        item_type,
        archive_root,
        "json",
        limit,
        |item| {
            let json = fetcher.fetch_json(&url_for(item))?;
            serde_json::to_vec_pretty(&json).map_err(|e| e.to_string())
        },
    )
}

/// Archive the raw **bytes** of every stored `(service, item_type)` item lacking
/// a local body, written with extension `ext` (e.g. OneNote page HTML).
#[allow(clippy::too_many_arguments)]
pub fn backup_byte_bodies<F, U>(
    fetcher: &F,
    store: &Store,
    account: &str,
    service: &str,
    item_type: &str,
    archive_root: &Path,
    ext: &str,
    url_for: U,
    limit: usize,
) -> Result<ArchiveReport, SyncError>
where
    F: BytesFetcher,
    U: Fn(&Item) -> String,
{
    archive_bodies(
        store,
        account,
        service,
        item_type,
        archive_root,
        ext,
        limit,
        |item| fetcher.fetch_bytes(&url_for(item)),
    )
}

/// Archive calendar-event JSON (`GET /me/events/{id}`).
pub fn backup_calendar_bodies<F: JsonFetcher>(
    fetcher: &F,
    store: &Store,
    account: &str,
    archive_root: &Path,
    limit: usize,
) -> Result<ArchiveReport, SyncError> {
    backup_json_bodies(
        fetcher,
        store,
        account,
        "calendar",
        "event",
        archive_root,
        |it| format!("/me/events/{}", it.remote_id),
        limit,
    )
}

/// Archive contact JSON (`GET /me/contacts/{id}`).
pub fn backup_contacts_bodies<F: JsonFetcher>(
    fetcher: &F,
    store: &Store,
    account: &str,
    archive_root: &Path,
    limit: usize,
) -> Result<ArchiveReport, SyncError> {
    backup_json_bodies(
        fetcher,
        store,
        account,
        "contacts",
        "contact",
        archive_root,
        |it| format!("/me/contacts/{}", it.remote_id),
        limit,
    )
}

/// Archive todo-task JSON (`GET /me/todo/lists/{list}/tasks/{id}`). A task's
/// parent list id is its `parent_remote_id`.
pub fn backup_todo_bodies<F: JsonFetcher>(
    fetcher: &F,
    store: &Store,
    account: &str,
    archive_root: &Path,
    limit: usize,
) -> Result<ArchiveReport, SyncError> {
    backup_json_bodies(
        fetcher,
        store,
        account,
        "todo",
        "task",
        archive_root,
        |it| {
            let list = it.parent_remote_id.as_deref().unwrap_or_default();
            format!("/me/todo/lists/{list}/tasks/{}", it.remote_id)
        },
        limit,
    )
}

/// Archive OneNote page **HTML** (`GET /me/onenote/pages/{id}/content`).
pub fn backup_onenote_bodies<F: BytesFetcher>(
    fetcher: &F,
    store: &Store,
    account: &str,
    archive_root: &Path,
    limit: usize,
) -> Result<ArchiveReport, SyncError> {
    backup_byte_bodies(
        fetcher,
        store,
        account,
        "onenote",
        "page",
        archive_root,
        "html",
        |it| format!("/me/onenote/pages/{}/content", it.remote_id),
        limit,
    )
}

/// Archive binary OneNote page resources referenced by already-archived page HTML.
///
/// Microsoft Graph returns OneNote resource URLs in the page HTML (`img` `src` /
/// `data-fullres-src`, and `object` `data`). This pass reads the archived `.html`
/// files, fetches those resource URLs, writes the binary data under
/// `onenote_resources/`, and writes a `<page>.resources.json` manifest that maps
/// the original URL to the local resource path. `limit` caps fetched resources
/// (`0` = all), so the pass is resumable.
pub fn backup_onenote_resources<F: BytesFetcher>(
    fetcher: &F,
    store: &Store,
    account: &str,
    archive_root: &Path,
    limit: usize,
) -> Result<OneNoteResourceReport, SyncError> {
    let mut report = OneNoteResourceReport::default();
    for page in store.items_by_type(account, "onenote", "page")? {
        let Some(local_path) = page.local_path.as_deref() else {
            continue;
        };
        let html_path = archive_root.join(local_path);
        let html = match std::fs::read_to_string(&html_path) {
            Ok(html) => html,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(SyncError::Io(e)),
        };
        report.pages += 1;
        let resources = extract_onenote_resources(&html);
        if resources.is_empty() {
            continue;
        }
        let mut manifest_entries = Vec::new();
        for resource in resources {
            if limit != 0 && report.resources >= limit {
                break;
            }
            let ext = resource_ext(resource.content_type.as_deref(), &resource.url);
            let abs = shard_path(
                archive_root,
                "onenote_resources",
                &format!("{}:{}", page.remote_id, resource.url),
                ext,
            );
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let rel = abs
                .strip_prefix(archive_root)
                .unwrap_or(&abs)
                .to_string_lossy()
                .to_string();
            if abs.exists() {
                report.skipped += 1;
            } else {
                let bytes = fetcher
                    .fetch_bytes(&resource.url)
                    .map_err(SyncError::Remote)?;
                let tmp = abs.with_extension(format!("{ext}.part"));
                std::fs::write(&tmp, isyncyou_core::envelope::seal_for_disk(&bytes))?;
                std::fs::rename(&tmp, &abs)?;
                report.resources += 1;
                report.bytes += bytes.len() as u64;
            }
            manifest_entries.push(serde_json::json!({
                "source_url": resource.url,
                "content_type": resource.content_type,
                "local_path": rel,
            }));
        }
        if !manifest_entries.is_empty() {
            let manifest = serde_json::json!({
                "page_id": page.remote_id,
                "page_local_path": local_path,
                "resources": manifest_entries,
            });
            let manifest_path = html_path.with_extension("resources.json");
            let tmp = manifest_path.with_extension("resources.json.part");
            let bytes = serde_json::to_vec_pretty(&manifest)
                .map_err(|e| SyncError::Malformed(e.to_string()))?;
            std::fs::write(&tmp, isyncyou_core::envelope::seal_for_disk(&bytes))?;
            std::fs::rename(&tmp, &manifest_path)?;
            report.manifests += 1;
        }
    }
    Ok(report)
}

fn extract_onenote_resources(html: &str) -> Vec<OneNoteResourceRef> {
    let mut resources = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = 0;
    while let Some(rel) = find_ascii_case_insensitive(&html[cursor..], "<") {
        let tag_start = cursor + rel;
        let Some(tag_end) = find_tag_end(html, tag_start) else {
            break;
        };
        let tag = &html[tag_start..tag_end];
        let tag_name = tag_name(tag);
        let attrs = tag_attrs(tag);
        let candidate = match tag_name.as_deref() {
            Some("img") => attr(&attrs, "data-fullres-src")
                .or_else(|| attr(&attrs, "src"))
                .map(|url| {
                    let ty = attr(&attrs, "data-fullres-src-type")
                        .or_else(|| attr(&attrs, "data-src-type"))
                        .map(str::to_string);
                    (url.to_string(), ty)
                }),
            Some("object") => attr(&attrs, "data")
                .map(|url| (url.to_string(), attr(&attrs, "type").map(str::to_string))),
            _ => None,
        };
        if let Some((url, content_type)) = candidate {
            let url = html_attr_unescape(&url);
            if is_onenote_resource_url(&url) && seen.insert(url.clone()) {
                resources.push(OneNoteResourceRef { url, content_type });
            }
        }
        cursor = tag_end;
    }
    resources
}

fn tag_name(tag: &str) -> Option<String> {
    let bytes = tag.as_bytes();
    if bytes.first() != Some(&b'<') || bytes.get(1) == Some(&b'/') {
        return None;
    }
    let start = 1;
    let end = bytes[start..]
        .iter()
        .position(|b| !b.is_ascii_alphanumeric())
        .map(|p| start + p)
        .unwrap_or(bytes.len());
    if end == start {
        None
    } else {
        Some(tag[start..end].to_ascii_lowercase())
    }
}

fn tag_attrs(tag: &str) -> Vec<(String, String)> {
    let bytes = tag.as_bytes();
    let mut out = Vec::new();
    let mut i = tag_name(tag).map(|n| n.len() + 1).unwrap_or(1);
    while i < bytes.len() {
        while i < bytes.len() && is_ascii_ws(bytes[i]) {
            i += 1;
        }
        if i >= bytes.len() || matches!(bytes[i], b'>' | b'/') {
            break;
        }
        let name_start = i;
        while i < bytes.len() && is_attr_name_byte(bytes[i]) {
            i += 1;
        }
        if i == name_start {
            i += 1;
            continue;
        }
        let name = tag[name_start..i].to_ascii_lowercase();
        while i < bytes.len() && is_ascii_ws(bytes[i]) {
            i += 1;
        }
        if bytes.get(i) != Some(&b'=') {
            out.push((name, String::new()));
            continue;
        }
        i += 1;
        while i < bytes.len() && is_ascii_ws(bytes[i]) {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let (value_start, value_end) = if matches!(bytes[i], b'"' | b'\'') {
            let quote = bytes[i];
            i += 1;
            let value_start = i;
            while i < bytes.len() && bytes[i] != quote {
                i += 1;
            }
            let value_end = i;
            if i < bytes.len() {
                i += 1;
            }
            (value_start, value_end)
        } else {
            let value_start = i;
            while i < bytes.len() && !is_ascii_ws(bytes[i]) && bytes[i] != b'>' {
                i += 1;
            }
            (value_start, i)
        };
        out.push((name, tag[value_start..value_end].to_string()));
    }
    out
}

fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

fn is_onenote_resource_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    (lower.starts_with("https://graph.microsoft.com/")
        || lower.starts_with("https://www.onenote.com/"))
        && lower.contains("/resources/")
        && (lower.contains("/content") || lower.contains("/$value"))
}

fn resource_ext(content_type: Option<&str>, url: &str) -> &'static str {
    match content_type
        .and_then(|t| t.split(';').next())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        "application/pdf" => "pdf",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        _ if url.to_ascii_lowercase().contains(".png") => "png",
        _ if url.to_ascii_lowercase().contains(".jpg")
            || url.to_ascii_lowercase().contains(".jpeg") =>
        {
            "jpg"
        }
        _ => "bin",
    }
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    haystack.as_bytes().windows(needle.len()).position(|w| {
        w.iter()
            .zip(needle.as_bytes())
            .all(|(l, r)| l.eq_ignore_ascii_case(r))
    })
}

fn find_tag_end(s: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    for (off, ch) in s[start..].char_indices() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (None, '"' | '\'') => quote = Some(ch),
            (None, '>') => return Some(start + off + 1),
            _ => {}
        }
    }
    None
}

fn html_attr_unescape(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn is_ascii_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
}

fn is_attr_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-' | b'@')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    /// Records the GET urls and echoes each back inside the returned JSON.
    struct MockJson {
        urls: RefCell<Vec<String>>,
    }
    impl MockJson {
        fn new() -> Self {
            MockJson {
                urls: RefCell::new(Vec::new()),
            }
        }
    }
    impl JsonFetcher for MockJson {
        fn fetch_json(&self, url: &str) -> Result<Value, String> {
            self.urls.borrow_mut().push(url.to_string());
            Ok(json!({ "id": "echoed", "url": url }))
        }
    }

    fn event_item(store: &Store, id: &str) {
        let mut it = Item::new("acc", "calendar", id, "Event", "event");
        it.parent_remote_id = Some("C1".into());
        store.upsert_item(&it).unwrap();
    }

    #[test]
    fn archives_event_json_and_records_local_path() {
        let store = Store::open_in_memory().unwrap();
        event_item(&store, "e1");
        event_item(&store, "e2");
        let dir = tempfile::tempdir().unwrap();
        let m = MockJson::new();

        let r = backup_calendar_bodies(&m, &store, "acc", dir.path(), 0).unwrap();
        assert_eq!(r.archived, 2);
        assert_eq!(r.skipped, 0);
        assert!(r.bytes > 0);
        assert!(m.urls.borrow().contains(&"/me/events/e1".to_string()));

        let e1 = store.get_item("acc", "calendar", "e1").unwrap().unwrap();
        let rel = e1.local_path.expect("local_path set");
        assert!(rel.starts_with("calendar/") && rel.ends_with(".json"));
        let bytes = std::fs::read(dir.path().join(&rel)).unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["url"], "/me/events/e1");
        assert!(!dir.path().join(&rel).with_extension("json.part").exists());

        // second pass skips already-archived bodies
        let r2 = backup_calendar_bodies(&m, &store, "acc", dir.path(), 0).unwrap();
        assert_eq!(r2.archived, 0);
        assert_eq!(r2.skipped, 2);
    }

    #[test]
    fn contacts_use_the_contacts_url() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_item(&Item::new("acc", "contacts", "c1", "Ada", "contact"))
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let m = MockJson::new();
        let r = backup_contacts_bodies(&m, &store, "acc", dir.path(), 0).unwrap();
        assert_eq!(r.archived, 1);
        assert_eq!(m.urls.borrow().as_slice(), &["/me/contacts/c1".to_string()]);
        let rel = store
            .get_item("acc", "contacts", "c1")
            .unwrap()
            .unwrap()
            .local_path
            .unwrap();
        assert!(rel.starts_with("contacts/"));
    }

    #[test]
    fn limit_caps_archive_per_pass() {
        let store = Store::open_in_memory().unwrap();
        event_item(&store, "e1");
        event_item(&store, "e2");
        let dir = tempfile::tempdir().unwrap();
        let m = MockJson::new();
        assert_eq!(
            backup_calendar_bodies(&m, &store, "acc", dir.path(), 1)
                .unwrap()
                .archived,
            1
        );
        let second = backup_calendar_bodies(&m, &store, "acc", dir.path(), 1).unwrap();
        assert_eq!(second.archived, 1);
        assert_eq!(second.skipped, 1);
    }

    /// Echoes fixed HTML bytes and records the GET urls.
    struct MockBytes {
        urls: RefCell<Vec<String>>,
    }
    impl MockBytes {
        fn new() -> Self {
            MockBytes {
                urls: RefCell::new(Vec::new()),
            }
        }
    }
    impl BytesFetcher for MockBytes {
        fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
            self.urls.borrow_mut().push(url.to_string());
            Ok(b"<html><body>page</body></html>".to_vec())
        }
    }

    #[test]
    fn todo_uses_the_list_scoped_task_url() {
        let store = Store::open_in_memory().unwrap();
        let mut t = Item::new("acc", "todo", "t1", "Write report", "task");
        t.parent_remote_id = Some("LIST9".into());
        store.upsert_item(&t).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let m = MockJson::new();
        let r = backup_todo_bodies(&m, &store, "acc", dir.path(), 0).unwrap();
        assert_eq!(r.archived, 1);
        assert_eq!(
            m.urls.borrow().as_slice(),
            &["/me/todo/lists/LIST9/tasks/t1".to_string()]
        );
        let rel = store
            .get_item("acc", "todo", "t1")
            .unwrap()
            .unwrap()
            .local_path
            .unwrap();
        assert!(rel.starts_with("todo/") && rel.ends_with(".json"));
    }

    #[test]
    fn onenote_archives_page_html() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_item(&Item::new("acc", "onenote", "p1", "Ideas", "page"))
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let m = MockBytes::new();
        let r = backup_onenote_bodies(&m, &store, "acc", dir.path(), 0).unwrap();
        assert_eq!(r.archived, 1);
        assert_eq!(
            m.urls.borrow().as_slice(),
            &["/me/onenote/pages/p1/content".to_string()]
        );
        let rel = store
            .get_item("acc", "onenote", "p1")
            .unwrap()
            .unwrap()
            .local_path
            .unwrap();
        assert!(rel.starts_with("onenote/") && rel.ends_with(".html"));
        let bytes = std::fs::read(dir.path().join(&rel)).unwrap();
        assert!(bytes.starts_with(b"<html>"));
    }

    #[test]
    fn onenote_archives_resource_manifest_from_page_html() {
        let store = Store::open_in_memory().unwrap();
        let mut page = Item::new("acc", "onenote", "p1", "Ideas", "page");
        page.local_path = Some("onenote/aa/bb/p1.html".into());
        store.upsert_item(&page).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let html_path = dir.path().join("onenote/aa/bb/p1.html");
        std::fs::create_dir_all(html_path.parent().unwrap()).unwrap();
        std::fs::write(
            &html_path,
            r#"
            <html><body>
              <img
                data-fullres-src="https://graph.microsoft.com/v1.0/me/onenote/resources/r1/content?x=1&amp;y=2"
                data-fullres-src-type="image/png"
                src="https://graph.microsoft.com/v1.0/me/onenote/resources/r1/content?x=1&amp;y=2">
              <object
                data="https://www.onenote.com/api/v1.0/me/notes/resources/r2/$value"
                type="application/pdf"></object>
              <img src="https://tracker.example/pixel.gif">
            </body></html>
            "#,
        )
        .unwrap();

        let m = MockBytes::new();
        let r = backup_onenote_resources(&m, &store, "acc", dir.path(), 0).unwrap();
        assert_eq!(r.pages, 1);
        assert_eq!(r.resources, 2);
        assert_eq!(r.skipped, 0);
        assert_eq!(r.manifests, 1);
        assert!(r.bytes > 0);

        let urls = m.urls.borrow();
        assert_eq!(urls.len(), 2);
        assert!(urls.contains(
            &"https://graph.microsoft.com/v1.0/me/onenote/resources/r1/content?x=1&y=2".to_string()
        ));
        assert!(urls.contains(
            &"https://www.onenote.com/api/v1.0/me/notes/resources/r2/$value".to_string()
        ));
        assert!(!urls.iter().any(|url| url.contains("tracker.example")));

        let manifest_path = dir.path().join("onenote/aa/bb/p1.resources.json");
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(manifest_path).unwrap()).unwrap();
        assert_eq!(manifest["page_id"], "p1");
        let resources = manifest["resources"].as_array().unwrap();
        assert_eq!(resources.len(), 2);
        assert_eq!(resources[0]["content_type"], "image/png");
        assert_eq!(resources[1]["content_type"], "application/pdf");
        for resource in resources {
            let rel = resource["local_path"].as_str().unwrap();
            assert!(rel.starts_with("onenote_resources/"));
            assert!(dir.path().join(rel).is_file());
        }
    }

    /// Live: index the calendar, then archive a few events' canonical JSON and
    /// confirm each file is valid JSON with an `id`. Needs feature `http` +
    /// `ISYNCYOU_TEST_TOKEN` (`Calendars.Read`).
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_archive_calendar_bodies() {
        let _gate = crate::live_test_gate();
        let token = match std::env::var("ISYNCYOU_TEST_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!("skipping live_archive_calendar_bodies: ISYNCYOU_TEST_TOKEN not set");
                return;
            }
        };
        let store = Store::open_in_memory().unwrap();
        let mut client = isyncyou_graph::GraphClient::new(token);
        let idx = crate::incremental_sync_calendar(
            &mut client,
            &store,
            "testuser",
            "2019-01-01T00:00:00Z",
            "2030-01-01T00:00:00Z",
            "2026-06-02T00:00:00Z",
        )
        .expect("index sync should succeed");
        if idx.upserted == 0 {
            eprintln!("no events to archive; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let r = backup_calendar_bodies(&client, &store, "testuser", dir.path(), 3)
            .expect("archive should succeed");
        assert!(r.archived >= 1, "expected to archive at least one event");

        let one = store
            .items_by_type("testuser", "calendar", "event")
            .unwrap()
            .into_iter()
            .find(|i| i.local_path.is_some())
            .unwrap();
        let bytes = std::fs::read(dir.path().join(one.local_path.unwrap())).unwrap();
        let v: Value = serde_json::from_slice(&bytes).expect("archived file is valid JSON");
        assert!(
            v.get("id").and_then(Value::as_str).is_some(),
            "event JSON has an id"
        );
        eprintln!(
            "live calendar archive: archived={} bytes={}",
            r.archived, r.bytes
        );
    }

    /// Live: index ToDo, archive a few tasks' canonical JSON, confirm valid JSON
    /// with an `id`. Needs feature `http` + `ISYNCYOU_TEST_TOKEN` (`Tasks.Read`).
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_archive_todo_bodies() {
        let _gate = crate::live_test_gate();
        let token = match std::env::var("ISYNCYOU_TEST_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!("skipping live_archive_todo_bodies: ISYNCYOU_TEST_TOKEN not set");
                return;
            }
        };
        let store = Store::open_in_memory().unwrap();
        let mut client = isyncyou_graph::GraphClient::new(token);
        let idx =
            crate::incremental_sync_todo(&mut client, &store, "testuser", "2026-06-02T00:00:00Z")
                .expect("todo index sync should succeed");
        if idx.upserted == 0 {
            eprintln!("no tasks to archive; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let r = backup_todo_bodies(&client, &store, "testuser", dir.path(), 3)
            .expect("todo archive should succeed");
        assert!(r.archived >= 1);
        let one = store
            .items_by_type("testuser", "todo", "task")
            .unwrap()
            .into_iter()
            .find(|i| i.local_path.is_some())
            .unwrap();
        let bytes = std::fs::read(dir.path().join(one.local_path.unwrap())).unwrap();
        let v: Value = serde_json::from_slice(&bytes).expect("archived task is valid JSON");
        assert!(v.get("id").and_then(Value::as_str).is_some());
        eprintln!(
            "live todo archive: archived={} bytes={}",
            r.archived, r.bytes
        );
    }

    /// Live: index OneNote, archive page HTML. The throwaway account has no
    /// notebook, so this proves the walk runs (0 pages is a valid outcome).
    /// Needs feature `http` + `ISYNCYOU_TEST_TOKEN` (`Notes.Read`).
    #[cfg(feature = "http")]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_archive_onenote_bodies() {
        let _gate = crate::live_test_gate();
        let token = match std::env::var("ISYNCYOU_TEST_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!("skipping live_archive_onenote_bodies: ISYNCYOU_TEST_TOKEN not set");
                return;
            }
        };
        let store = Store::open_in_memory().unwrap();
        let mut client = isyncyou_graph::GraphClient::new(token);
        crate::incremental_sync_onenote(
            &mut client,
            &store,
            "testuser",
            "2026-06-02T00:00:00Z",
            None,
        )
        .expect("onenote index sync should succeed");
        let dir = tempfile::tempdir().unwrap();
        let r = backup_onenote_bodies(&client, &store, "testuser", dir.path(), 3)
            .expect("onenote archive should succeed");
        // every archived page must be a non-empty .html file
        if r.archived > 0 {
            let one = store
                .items_by_type("testuser", "onenote", "page")
                .unwrap()
                .into_iter()
                .find(|i| i.local_path.is_some())
                .unwrap();
            let bytes = std::fs::read(dir.path().join(one.local_path.unwrap())).unwrap();
            assert!(!bytes.is_empty());
        }
        eprintln!(
            "live onenote archive: archived={} bytes={}",
            r.archived, r.bytes
        );
    }
}
