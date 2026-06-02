//! `isyncyou-webui` — the local web UI's request router (plan §25).
//!
//! The daemon serves a browser-based full-control UI on a local socket; this
//! crate is the **pure** request→response logic, independent of any HTTP server
//! or socket, so it is fully unit-testable. A thin server adapter (added with the
//! daemon) binds a listener and forwards each request to [`Router::route`].
//!
//! Endpoints (read-only for now; restore/job actions land with the daemon's
//! capability-token auth):
//! - `GET /`                      → the static UI page
//! - `GET /api/v1/accounts`       → configured accounts
//! - `GET /api/v1/status?account`            → per-service archive counts overview
//! - `GET /api/v1/items?account&service`     → archived items of a service
//! - `GET /api/v1/item?account&service&id`   → one item's metadata
//! - `GET /api/v1/body?account&service&id`   → archived body bytes (inert)
//! - `GET /api/v1/view?account&service&id`   → rendered safe HTML viewer page
//! - `GET /api/v1/search?account&q`          → full-text search over item names

use isyncyou_core::Config;
use isyncyou_store::{Item, Store};
use serde_json::{json, Value};
use std::path::PathBuf;

mod serve;
mod view;
pub use serve::{format_http, parse_request_line, serve};

/// The embedded single-page UI (served at `/`). Talks to the JSON API via fetch.
pub const INDEX_HTML: &str = include_str!("index.html");

/// Services that can hold archived items (mirrors the CLI's `status`).
const STATUS_SERVICES: &[&str] = &[
    "onedrive", "mail", "calendar", "contacts", "todo", "onenote", "shared",
];

/// A parsed inbound request (method + path + decoded query pairs).
#[derive(Debug, Clone)]
pub struct ApiRequest {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
}

impl ApiRequest {
    /// Parse `method` + a raw `target` like `/api/v1/items?account=a&service=mail`.
    pub fn new(method: &str, target: &str) -> Self {
        let (path, qs) = match target.split_once('?') {
            Some((p, q)) => (p, q),
            None => (target, ""),
        };
        let query = qs
            .split('&')
            .filter(|s| !s.is_empty())
            .map(|pair| match pair.split_once('=') {
                Some((k, v)) => (decode(k), decode(v)),
                None => (decode(pair), String::new()),
            })
            .collect();
        ApiRequest {
            method: method.to_string(),
            path: path.to_string(),
            query,
        }
    }

    /// Convenience constructor for `GET target`.
    pub fn get(target: &str) -> Self {
        ApiRequest::new("GET", target)
    }

    fn q(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// Minimal percent-decoding for query values (`%XX` + `+` → space).
fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = |b: u8| match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                };
                match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push(h << 4 | l);
                        i += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A response ready to be written by any server adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    /// Extra response headers (beyond the always-present content-type / nosniff),
    /// e.g. a per-response `Content-Security-Policy` for a rendered viewer page.
    pub headers: Vec<(String, String)>,
}

impl ApiResponse {
    fn json(status: u16, v: &Value) -> Self {
        ApiResponse {
            status,
            content_type: "application/json".into(),
            body: serde_json::to_vec(v).unwrap_or_default(),
            headers: Vec::new(),
        }
    }
    fn ok_json(v: &Value) -> Self {
        Self::json(200, v)
    }
    fn html(body: &str) -> Self {
        ApiResponse {
            status: 200,
            content_type: "text/html; charset=utf-8".into(),
            body: body.as_bytes().to_vec(),
            headers: Vec::new(),
        }
    }
    /// An HTML page locked down with a strict `Content-Security-Policy` header —
    /// used by the rendered item viewers, which must never load anything.
    fn html_locked(body: &str) -> Self {
        let mut r = Self::html(body);
        r.headers
            .push(("Content-Security-Policy".into(), view::VIEWER_CSP.into()));
        r
    }
    fn error(status: u16, message: &str) -> Self {
        Self::json(status, &json!({ "error": message }))
    }
}

/// Routes requests against the configured accounts and their stores.
pub struct Router {
    config: Config,
}

impl Router {
    pub fn new(config: Config) -> Self {
        Router { config }
    }

    /// Dispatch one request to a response. Never panics; unknown routes → 404.
    pub fn route(&self, req: &ApiRequest) -> ApiResponse {
        if req.method != "GET" {
            return ApiResponse::error(405, "method not allowed");
        }
        match req.path.as_str() {
            "/" => ApiResponse::html(INDEX_HTML),
            "/api/v1/accounts" => self.accounts(),
            "/api/v1/status" => self.status(req),
            "/api/v1/items" => self.items(req),
            "/api/v1/item" => self.item(req),
            "/api/v1/body" => self.body(req),
            "/api/v1/view" => self.view(req),
            "/api/v1/search" => self.search(req),
            _ => ApiResponse::error(404, "not found"),
        }
    }

    fn store_path(&self, account: &str) -> Option<PathBuf> {
        self.config
            .accounts
            .iter()
            .find(|a| a.id == account)
            .map(|a| a.archive_root.join(".isyncyou-store.db"))
    }

    fn accounts(&self) -> ApiResponse {
        let accounts: Vec<Value> = self
            .config
            .accounts
            .iter()
            .map(|a| json!({ "id": a.id, "username": a.username }))
            .collect();
        ApiResponse::ok_json(&json!({ "accounts": accounts }))
    }

    /// Per-account archive overview: for each non-empty service, the tracked-item
    /// count and how many have an archived body, plus whether a OneDrive delta
    /// cursor exists. Mirrors the CLI's `status` for the browser dashboard.
    fn status(&self, req: &ApiRequest) -> ApiResponse {
        let account = match req.q("account") {
            Some(a) => a,
            None => return ApiResponse::error(400, "missing 'account'"),
        };
        let store = match self.open(Some(account)) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let mut services = Vec::new();
        let (mut total_items, mut total_archived) = (0usize, 0usize);
        for &svc in STATUS_SERVICES {
            match store.items_by_service(account, svc) {
                Ok(items) if !items.is_empty() => {
                    let archived = items.iter().filter(|i| i.local_path.is_some()).count();
                    total_items += items.len();
                    total_archived += archived;
                    services.push(json!({
                        "service": svc, "items": items.len(), "archived": archived,
                    }));
                }
                Ok(_) => {}
                Err(e) => return ApiResponse::error(500, &format!("query: {e}")),
            }
        }
        let onedrive_cursor = store
            .get_delta_cursor(account, "onedrive", "")
            .map(|c| c.is_some())
            .unwrap_or(false);
        ApiResponse::ok_json(&json!({
            "account": account,
            "services": services,
            "totals": { "items": total_items, "archived": total_archived },
            "onedrive_cursor": onedrive_cursor,
        }))
    }

    fn open(&self, account: Option<&str>) -> Result<Store, ApiResponse> {
        let account = account.ok_or_else(|| ApiResponse::error(400, "missing 'account'"))?;
        let path = self
            .store_path(account)
            .ok_or_else(|| ApiResponse::error(404, "unknown account"))?;
        Store::open(path).map_err(|e| ApiResponse::error(500, &format!("store: {e}")))
    }

    fn items(&self, req: &ApiRequest) -> ApiResponse {
        let service = match req.q("service") {
            Some(s) => s,
            None => return ApiResponse::error(400, "missing 'service'"),
        };
        let store = match self.open(req.q("account")) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let account = req.q("account").unwrap_or_default();
        match store.items_by_service(account, service) {
            Ok(items) => {
                let arr: Vec<Value> = items.iter().map(item_json).collect();
                ApiResponse::ok_json(&json!({ "items": arr, "count": arr.len() }))
            }
            Err(e) => ApiResponse::error(500, &format!("query: {e}")),
        }
    }

    fn item(&self, req: &ApiRequest) -> ApiResponse {
        let (service, id) = match (req.q("service"), req.q("id")) {
            (Some(s), Some(i)) => (s, i),
            _ => return ApiResponse::error(400, "missing 'service' or 'id'"),
        };
        let store = match self.open(req.q("account")) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let account = req.q("account").unwrap_or_default();
        match store.get_item(account, service, id) {
            Ok(Some(it)) => ApiResponse::ok_json(&item_json(&it)),
            Ok(None) => ApiResponse::error(404, "item not found"),
            Err(e) => ApiResponse::error(500, &format!("query: {e}")),
        }
    }

    /// Serve an item's archived body bytes. The content-type is forced to a
    /// **non-executable** type (the browser must never run scripts/trackers
    /// embedded in a backed-up mail/page — the full sanitized viewer is later
    /// work), and the resolved path must stay under the account's archive_root.
    /// Read an item's archived body bytes, path-safely. Returns `(relative_path,
    /// bytes)` or the `ApiResponse` to send on failure. The resolved file must
    /// stay under the account's `archive_root` (defense against `..`/symlink
    /// traversal). Shared by [`Self::body`] and [`Self::view`].
    fn read_archived(
        &self,
        account: &str,
        service: &str,
        id: &str,
    ) -> Result<(String, Vec<u8>), ApiResponse> {
        let acc = self
            .config
            .accounts
            .iter()
            .find(|a| a.id == account)
            .ok_or_else(|| ApiResponse::error(404, "unknown account"))?;
        let archive_root = acc.archive_root.clone();
        let store = self.open(Some(account))?;
        let rel = match store.get_item(account, service, id) {
            Ok(Some(it)) => it
                .local_path
                .ok_or_else(|| ApiResponse::error(404, "item has no archived body"))?,
            Ok(None) => return Err(ApiResponse::error(404, "item not found")),
            Err(e) => return Err(ApiResponse::error(500, &format!("query: {e}"))),
        };
        let path = archive_root.join(&rel);
        match (path.canonicalize(), archive_root.canonicalize()) {
            (Ok(p), Ok(root)) if p.starts_with(&root) => match std::fs::read(&p) {
                Ok(bytes) => Ok((rel, bytes)),
                Err(e) => Err(ApiResponse::error(500, &format!("read: {e}"))),
            },
            (Ok(_), Ok(_)) => Err(ApiResponse::error(400, "body path escapes archive root")),
            _ => Err(ApiResponse::error(404, "archived body file missing")),
        }
    }

    /// Serve an item's archived body bytes inertly (forced non-executable
    /// content-type). For a *rendered* view use [`Self::view`].
    fn body(&self, req: &ApiRequest) -> ApiResponse {
        let (account, service, id) = match (req.q("account"), req.q("service"), req.q("id")) {
            (Some(a), Some(s), Some(i)) => (a, s, i),
            _ => return ApiResponse::error(400, "missing 'account', 'service' or 'id'"),
        };
        match self.read_archived(account, service, id) {
            Ok((rel, bytes)) => ApiResponse {
                status: 200,
                content_type: safe_content_type(&rel).into(),
                body: bytes,
                headers: Vec::new(),
            },
            Err(e) => e,
        }
    }

    /// Render an archived item as a **safe, self-contained HTML page**: our own
    /// canonical JSON (calendar/contacts/todo/onenote) is escaped into a fixed
    /// skeleton — no untrusted markup, no scripts, no external resources — and a
    /// raw `.eml`/non-JSON body is shown as escaped source. The response carries
    /// a strict `Content-Security-Policy` so a browser loads nothing. A rich,
    /// sanitized HTML mail *renderer* (allowlist parser) is deliberately separate
    /// follow-up work (it needs an HTML-sanitizer dependency).
    fn view(&self, req: &ApiRequest) -> ApiResponse {
        let (account, service, id) = match (req.q("account"), req.q("service"), req.q("id")) {
            (Some(a), Some(s), Some(i)) => (a, s, i),
            _ => return ApiResponse::error(400, "missing 'account', 'service' or 'id'"),
        };
        let (rel, bytes) = match self.read_archived(account, service, id) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let page = if rel.ends_with(".json") {
            match serde_json::from_slice::<Value>(&bytes) {
                Ok(v) => view::render_item(service, &v),
                Err(e) => view::page(
                    "Unreadable item",
                    &format!(
                        "<p>archived JSON could not be parsed: {}</p>",
                        view::escape(&e.to_string())
                    ),
                ),
            }
        } else {
            // .eml or other raw body: show escaped source (inert, never executed).
            view::source_page(service, &String::from_utf8_lossy(&bytes))
        };
        ApiResponse::html_locked(&page)
    }

    fn search(&self, req: &ApiRequest) -> ApiResponse {
        let q = match req.q("q") {
            Some(q) if !q.is_empty() => q,
            _ => return ApiResponse::error(400, "missing 'q'"),
        };
        let store = match self.open(req.q("account")) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let account = req.q("account").unwrap_or_default();
        // names (subjects/titles/filenames) ...
        let mut hits = match store.search_names(account, q) {
            Ok(h) => h,
            // An invalid FTS expression is a client error, not a server fault.
            Err(e) => return ApiResponse::error(400, &format!("invalid query: {e}")),
        };
        // ... merged with indexed bodies (e.g. mail text), de-duplicated.
        let mut seen: std::collections::HashSet<(String, String)> = hits
            .iter()
            .map(|i| (i.service.clone(), i.remote_id.clone()))
            .collect();
        match store.search_bodies(account, q) {
            Ok(body_hits) => {
                for (service, remote_id) in body_hits {
                    if seen.insert((service.clone(), remote_id.clone())) {
                        if let Ok(Some(it)) = store.get_item(account, &service, &remote_id) {
                            hits.push(it);
                        }
                    }
                }
            }
            Err(e) => return ApiResponse::error(400, &format!("invalid query: {e}")),
        }
        let arr: Vec<Value> = hits.iter().map(item_json).collect();
        ApiResponse::ok_json(&json!({ "query": q, "hits": arr, "count": arr.len() }))
    }
}

/// A deliberately non-executable content-type for archived bodies: `.json` is
/// served as JSON; everything else (incl. `.eml` and `.html`) as plain text so a
/// browser renders it inertly without running scripts, loading trackers, or
/// treating it as a page.
fn safe_content_type(rel: &str) -> &'static str {
    if rel.ends_with(".json") {
        "application/json; charset=utf-8"
    } else {
        "text/plain; charset=utf-8"
    }
}

/// Serialize an item's safe metadata (never the body bytes).
fn item_json(it: &Item) -> Value {
    json!({
        "service": it.service,
        "remote_id": it.remote_id,
        "name": it.name,
        "item_type": it.item_type,
        "parent_remote_id": it.parent_remote_id,
        "sync_state": it.sync_state,
        "remote_mtime": it.remote_mtime,
        "size": it.size,
        "has_body": it.local_path.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use isyncyou_core::config::AccountConfig;

    fn setup() -> (tempfile::TempDir, Router) {
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("arch");
        std::fs::create_dir_all(&arch).unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut m = Item::new("a", "mail", "m1", "Invoice March", "message");
            m.parent_remote_id = Some("F1".into());
            m.local_path = Some("mail/aa/bb/x.eml".into());
            m.remote_mtime = Some("2026-03-01T00:00:00Z".into());
            store.upsert_item(&m).unwrap();
            store
                .upsert_item(&Item::new("a", "mail", "m2", "Lunch plans", "message"))
                .unwrap();
            store
                .upsert_item(&Item::new("a", "calendar", "e1", "Standup", "event"))
                .unwrap();
        }
        let cfg = Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@outlook.com".into(),
                sync_root: dir.path().join("od"),
                archive_root: arch,
            }],
            ..Default::default()
        };
        (dir, Router::new(cfg))
    }

    fn body_json(resp: &ApiResponse) -> Value {
        serde_json::from_slice(&resp.body).unwrap()
    }

    #[test]
    fn request_parses_path_and_query() {
        let r = ApiRequest::get("/api/v1/items?account=a&service=mail");
        assert_eq!(r.path, "/api/v1/items");
        assert_eq!(r.q("account"), Some("a"));
        assert_eq!(r.q("service"), Some("mail"));
        // percent + plus decoding
        let r2 = ApiRequest::get("/api/v1/search?q=report+2026%2Fq1");
        assert_eq!(r2.q("q"), Some("report 2026/q1"));
    }

    #[test]
    fn root_serves_html() {
        let (_d, router) = setup();
        let resp = router.route(&ApiRequest::get("/"));
        assert_eq!(resp.status, 200);
        assert!(resp.content_type.starts_with("text/html"));
        assert!(String::from_utf8_lossy(&resp.body).contains("iSyncYou"));
    }

    #[test]
    fn index_lists_all_store_services() {
        // every service that can hold archived items is a browsable tab
        for svc in [
            "onedrive", "mail", "calendar", "contacts", "todo", "onenote", "shared",
        ] {
            assert!(
                INDEX_HTML.contains(&format!("\"{svc}\"")),
                "web UI is missing the '{svc}' service tab"
            );
        }
    }

    #[test]
    fn status_reports_per_service_counts_and_totals() {
        let (_d, router) = setup();
        let resp = router.route(&ApiRequest::get("/api/v1/status?account=a"));
        assert_eq!(resp.status, 200);
        let v = body_json(&resp);
        assert_eq!(v["account"], "a");
        // setup(): mail has m1 (with body) + m2 (no body); calendar has e1 (no body)
        let mail = v["services"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["service"] == "mail")
            .unwrap();
        assert_eq!(mail["items"], 2);
        assert_eq!(mail["archived"], 1);
        let cal = v["services"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["service"] == "calendar")
            .unwrap();
        assert_eq!(cal["items"], 1);
        assert_eq!(cal["archived"], 0);
        // empty services are omitted; totals aggregate across services
        assert!(v["services"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["service"] != "contacts"));
        assert_eq!(v["totals"]["items"], 3);
        assert_eq!(v["totals"]["archived"], 1);
        assert_eq!(v["onedrive_cursor"], false);

        // missing account -> 400, unknown account -> 404
        assert_eq!(router.route(&ApiRequest::get("/api/v1/status")).status, 400);
        assert_eq!(
            router
                .route(&ApiRequest::get("/api/v1/status?account=ghost"))
                .status,
            404
        );
    }

    #[test]
    fn accounts_lists_configured_accounts() {
        let (_d, router) = setup();
        let resp = router.route(&ApiRequest::get("/api/v1/accounts"));
        assert_eq!(resp.status, 200);
        let v = body_json(&resp);
        assert_eq!(v["accounts"][0]["id"], "a");
        assert_eq!(v["accounts"][0]["username"], "a@outlook.com");
    }

    #[test]
    fn items_lists_a_service() {
        let (_d, router) = setup();
        let resp = router.route(&ApiRequest::get("/api/v1/items?account=a&service=mail"));
        assert_eq!(resp.status, 200);
        let v = body_json(&resp);
        assert_eq!(v["count"], 2);
        // first by (item_type, name): message "Invoice March" before "Lunch plans"
        let names: Vec<&str> = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"Invoice March") && names.contains(&"Lunch plans"));
        let m1 = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["remote_id"] == "m1")
            .unwrap();
        assert_eq!(m1["has_body"], true);
        assert_eq!(m1["parent_remote_id"], "F1");
    }

    #[test]
    fn item_returns_one_or_404() {
        let (_d, router) = setup();
        let ok = router.route(&ApiRequest::get(
            "/api/v1/item?account=a&service=mail&id=m1",
        ));
        assert_eq!(ok.status, 200);
        assert_eq!(body_json(&ok)["name"], "Invoice March");
        let miss = router.route(&ApiRequest::get(
            "/api/v1/item?account=a&service=mail&id=nope",
        ));
        assert_eq!(miss.status, 404);
    }

    #[test]
    fn search_matches_names() {
        let (_d, router) = setup();
        let resp = router.route(&ApiRequest::get("/api/v1/search?account=a&q=invoice"));
        assert_eq!(resp.status, 200);
        let v = body_json(&resp);
        assert_eq!(v["count"], 1);
        assert_eq!(v["hits"][0]["remote_id"], "m1");
    }

    #[test]
    fn search_includes_indexed_body_hits() {
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("arch");
        std::fs::create_dir_all(&arch).unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            store
                .upsert_item(&Item::new("a", "mail", "m1", "Receipt", "message"))
                .unwrap();
            store
                .index_body("a", "mail", "m1", "the warranty covers two years")
                .unwrap();
        }
        let cfg = Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@outlook.com".into(),
                sync_root: dir.path().join("od"),
                archive_root: arch,
            }],
            ..Default::default()
        };
        let router = Router::new(cfg);
        // a term only in the mail body is found via the body index
        let v = body_json(&router.route(&ApiRequest::get("/api/v1/search?account=a&q=warranty")));
        assert_eq!(v["count"], 1);
        assert_eq!(v["hits"][0]["remote_id"], "m1");
    }

    #[test]
    fn body_serves_archived_bytes_with_safe_content_type() {
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("arch");
        std::fs::create_dir_all(arch.join("calendar/aa/bb")).unwrap();
        std::fs::create_dir_all(arch.join("mail/cc/dd")).unwrap();
        std::fs::write(
            arch.join("calendar/aa/bb/ev.json"),
            b"{\"id\":\"e1\",\"subject\":\"X\"}",
        )
        .unwrap();
        std::fs::write(
            arch.join("mail/cc/dd/m.eml"),
            b"From: a@b\r\nSubject: Hi\r\n",
        )
        .unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut e = Item::new("a", "calendar", "e1", "X", "event");
            e.local_path = Some("calendar/aa/bb/ev.json".into());
            store.upsert_item(&e).unwrap();
            let mut m = Item::new("a", "mail", "m1", "Hi", "message");
            m.local_path = Some("mail/cc/dd/m.eml".into());
            store.upsert_item(&m).unwrap();
            store
                .upsert_item(&Item::new("a", "calendar", "e2", "NoBody", "event"))
                .unwrap();
        }
        let cfg = Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@outlook.com".into(),
                sync_root: dir.path().join("od"),
                archive_root: arch,
            }],
            ..Default::default()
        };
        let router = Router::new(cfg);

        // JSON body -> application/json + the bytes
        let j = router.route(&ApiRequest::get(
            "/api/v1/body?account=a&service=calendar&id=e1",
        ));
        assert_eq!(j.status, 200);
        assert!(j.content_type.starts_with("application/json"));
        assert!(String::from_utf8_lossy(&j.body).contains("\"subject\":\"X\""));

        // .eml body -> served as inert text/plain (never text/html)
        let m = router.route(&ApiRequest::get(
            "/api/v1/body?account=a&service=mail&id=m1",
        ));
        assert_eq!(m.status, 200);
        assert!(m.content_type.starts_with("text/plain"));
        assert!(String::from_utf8_lossy(&m.body).contains("Subject: Hi"));

        // item without a body -> 404, missing params -> 400
        assert_eq!(
            router
                .route(&ApiRequest::get(
                    "/api/v1/body?account=a&service=calendar&id=e2"
                ))
                .status,
            404
        );
        assert_eq!(
            router
                .route(&ApiRequest::get("/api/v1/body?account=a&service=calendar"))
                .status,
            400
        );
    }

    #[test]
    fn view_renders_safe_html_with_csp_and_escapes_untrusted_values() {
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("arch");
        std::fs::create_dir_all(arch.join("calendar/aa/bb")).unwrap();
        std::fs::create_dir_all(arch.join("mail/cc/dd")).unwrap();
        // a calendar event whose subject carries a script payload
        std::fs::write(
            arch.join("calendar/aa/bb/ev.json"),
            br#"{"id":"e1","subject":"<script>alert(1)</script>","location":{"displayName":"Room 1"}}"#,
        )
        .unwrap();
        std::fs::write(
            arch.join("mail/cc/dd/m.eml"),
            b"From: a@b\r\nSubject: Hi\r\n\r\nbody",
        )
        .unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut e = Item::new("a", "calendar", "e1", "evt", "event");
            e.local_path = Some("calendar/aa/bb/ev.json".into());
            store.upsert_item(&e).unwrap();
            let mut m = Item::new("a", "mail", "m1", "Hi", "message");
            m.local_path = Some("mail/cc/dd/m.eml".into());
            store.upsert_item(&m).unwrap();
        }
        let cfg = Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@outlook.com".into(),
                sync_root: dir.path().join("od"),
                archive_root: arch,
            }],
            ..Default::default()
        };
        let router = Router::new(cfg);

        // calendar JSON -> rendered HTML, subject escaped, with a strict CSP header
        let v = router.route(&ApiRequest::get(
            "/api/v1/view?account=a&service=calendar&id=e1",
        ));
        assert_eq!(v.status, 200);
        assert!(v.content_type.starts_with("text/html"));
        let html = String::from_utf8_lossy(&v.body);
        assert!(html.contains("Calendar event"));
        assert!(html.contains("Room 1"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(
            !html.contains("<script>alert(1)"),
            "untrusted markup must not be live"
        );
        assert!(
            v.headers.iter().any(
                |(k, val)| k == "Content-Security-Policy" && val.contains("default-src 'none'")
            ),
            "viewer must carry a strict CSP header"
        );

        // .eml -> escaped inert source, also CSP-locked
        let m = router.route(&ApiRequest::get(
            "/api/v1/view?account=a&service=mail&id=m1",
        ));
        assert_eq!(m.status, 200);
        assert!(m.content_type.starts_with("text/html"));
        assert!(String::from_utf8_lossy(&m.body).contains("Subject: Hi"));
        assert!(m
            .headers
            .iter()
            .any(|(k, _)| k == "Content-Security-Policy"));

        // unknown item -> 404
        assert_eq!(
            router
                .route(&ApiRequest::get(
                    "/api/v1/view?account=a&service=mail&id=nope"
                ))
                .status,
            404
        );
    }

    #[test]
    fn missing_params_and_unknown_routes_are_errors() {
        let (_d, router) = setup();
        assert_eq!(
            router
                .route(&ApiRequest::get("/api/v1/items?account=a"))
                .status,
            400
        );
        assert_eq!(
            router
                .route(&ApiRequest::get("/api/v1/items?service=mail"))
                .status,
            400
        );
        assert_eq!(
            router
                .route(&ApiRequest::get("/api/v1/items?account=ghost&service=mail"))
                .status,
            404
        );
        assert_eq!(router.route(&ApiRequest::get("/nope")).status, 404);
        assert_eq!(
            router
                .route(&ApiRequest::new("POST", "/api/v1/accounts"))
                .status,
            405
        );
    }
}
