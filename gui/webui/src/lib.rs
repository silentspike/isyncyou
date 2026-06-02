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
//! - `GET /api/v1/settings`                  → effective sync settings + account roots
//! - `GET /api/v1/activity?account[&limit]`  → recent engine runs (activity log)
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
    /// An HTML page locked down with a `Content-Security-Policy` **header** (a
    /// second layer beside the page's `<meta>` CSP) — used by the rendered item
    /// viewers, which must never load anything remote.
    fn html_with_csp(body: &str, csp: &str) -> Self {
        let mut r = Self::html(body);
        r.headers
            .push(("Content-Security-Policy".into(), csp.into()));
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
            "/api/v1/settings" => self.settings(),
            "/api/v1/activity" => self.activity(req),
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

    /// The effective configuration the UI's settings view reads: engine-wide sync
    /// settings + each account's id/username/roots. Fields are **explicitly
    /// whitelisted** (not a blanket serialize of `Config`) so a future secret-
    /// bearing config field can never leak here; tokens live in separate files,
    /// never in the config.
    fn settings(&self) -> ApiResponse {
        let sync = serde_json::to_value(&self.config.sync).unwrap_or(Value::Null);
        let accounts: Vec<Value> = self
            .config
            .accounts
            .iter()
            .map(|a| {
                json!({
                    "id": a.id,
                    "username": a.username,
                    "sync_root": a.sync_root,
                    "archive_root": a.archive_root,
                })
            })
            .collect();
        ApiResponse::ok_json(&json!({ "sync": sync, "accounts": accounts }))
    }

    /// Recent engine runs for an account (the activity history), newest first.
    fn activity(&self, req: &ApiRequest) -> ApiResponse {
        let account = match req.q("account") {
            Some(a) => a,
            None => return ApiResponse::error(400, "missing 'account'"),
        };
        let limit = req
            .q("limit")
            .and_then(|l| l.parse::<u32>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(50)
            .min(500);
        let store = match self.open(Some(account)) {
            Ok(s) => s,
            Err(e) => return e,
        };
        match store.recent_runs(account, limit) {
            Ok(runs) => {
                let arr: Vec<Value> = runs
                    .iter()
                    .map(|r| {
                        json!({
                            "id": r.id,
                            "kind": r.kind,
                            "started_at": r.started_at,
                            "finished_at": r.finished_at,
                            "status": r.status,
                            "summary": r.summary,
                        })
                    })
                    .collect();
                ApiResponse::ok_json(&json!({ "runs": arr, "count": arr.len() }))
            }
            Err(e) => ApiResponse::error(500, &format!("query: {e}")),
        }
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
        // Page the listing so a large mailbox is never loaded all at once.
        let limit = clamp_limit(req.q("limit"));
        let offset = req
            .q("offset")
            .and_then(|o| o.parse::<u32>().ok())
            .unwrap_or(0);
        let total = match store.count_by_service(account, service) {
            Ok(t) => t,
            Err(e) => return ApiResponse::error(500, &format!("query: {e}")),
        };
        match store.items_by_service_page(account, service, limit, offset) {
            Ok(items) => {
                let arr: Vec<Value> = items.iter().map(item_json).collect();
                ApiResponse::ok_json(&json!({
                    "items": arr,
                    "count": arr.len(),
                    "total": total,
                    "limit": limit,
                    "offset": offset,
                }))
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

    /// Read an item's archived body bytes, path-safely. Returns `(relative_path,
    /// bytes, item_name)` or the `ApiResponse` to send on failure. The resolved
    /// file must stay under the account's `archive_root` (defense against
    /// `..`/symlink traversal). Shared by [`Self::body`] and [`Self::view`].
    fn read_archived(
        &self,
        account: &str,
        service: &str,
        id: &str,
    ) -> Result<(String, Vec<u8>, String), ApiResponse> {
        let acc = self
            .config
            .accounts
            .iter()
            .find(|a| a.id == account)
            .ok_or_else(|| ApiResponse::error(404, "unknown account"))?;
        let archive_root = acc.archive_root.clone();
        let store = self.open(Some(account))?;
        let (rel, name) = match store.get_item(account, service, id) {
            Ok(Some(it)) => {
                let rel = it
                    .local_path
                    .ok_or_else(|| ApiResponse::error(404, "item has no archived body"))?;
                (rel, it.name)
            }
            Ok(None) => return Err(ApiResponse::error(404, "item not found")),
            Err(e) => return Err(ApiResponse::error(500, &format!("query: {e}"))),
        };
        let path = archive_root.join(&rel);
        match (path.canonicalize(), archive_root.canonicalize()) {
            (Ok(p), Ok(root)) if p.starts_with(&root) => match std::fs::read(&p) {
                Ok(bytes) => Ok((rel, bytes, name)),
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
            Ok((rel, bytes, _name)) => ApiResponse {
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
    /// skeleton — no untrusted markup, no scripts, no external resources. A mail
    /// `.eml` is rendered through an allowlist HTML **sanitizer** (scripts/handlers
    /// dropped, remote resources blocked); any other raw body is shown as escaped
    /// source. Every page carries a strict `Content-Security-Policy` so the browser
    /// loads nothing remote even if something slipped past.
    fn view(&self, req: &ApiRequest) -> ApiResponse {
        let (account, service, id) = match (req.q("account"), req.q("service"), req.q("id")) {
            (Some(a), Some(s), Some(i)) => (a, s, i),
            _ => return ApiResponse::error(400, "missing 'account', 'service' or 'id'"),
        };
        let (rel, bytes, name) = match self.read_archived(account, service, id) {
            Ok(v) => v,
            Err(e) => return e,
        };
        if rel.ends_with(".json") {
            let page = match serde_json::from_slice::<Value>(&bytes) {
                Ok(v) => view::render_item(service, &v),
                Err(e) => view::page(
                    "Unreadable item",
                    &format!(
                        "<p>archived JSON could not be parsed: {}</p>",
                        view::escape(&e.to_string())
                    ),
                ),
            };
            return ApiResponse::html_with_csp(&page, view::VIEWER_CSP);
        }
        // A mail `.eml` with an HTML part is rendered sanitized; otherwise (plain
        // mail, or any other raw body) we show escaped source.
        if service == "mail" {
            if let Some(html) = isyncyou_connectors::extract_html(&bytes) {
                let subject = if name.is_empty() { "Message" } else { &name };
                return ApiResponse::html_with_csp(
                    &view::mail_page(subject, &html),
                    view::MAIL_CSP,
                );
            }
        }
        ApiResponse::html_with_csp(
            &view::source_page(service, &String::from_utf8_lossy(&bytes)),
            view::VIEWER_CSP,
        )
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

/// Default and maximum page size for the items listing.
const DEFAULT_LIMIT: u32 = 200;
const MAX_LIMIT: u32 = 1000;

/// Parse a `?limit` query value into a sane page size: default when absent/0/bad,
/// capped so a client can't ask for an unbounded page.
fn clamp_limit(raw: Option<&str>) -> u32 {
    match raw.and_then(|l| l.parse::<u32>().ok()) {
        Some(0) | None => DEFAULT_LIMIT,
        Some(n) => n.min(MAX_LIMIT),
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
    fn settings_exposes_sync_config_and_account_roots_without_secrets() {
        let (_d, router) = setup();
        let resp = router.route(&ApiRequest::get("/api/v1/settings"));
        assert_eq!(resp.status, 200);
        let v = body_json(&resp);
        // engine-wide sync defaults
        assert_eq!(v["sync"]["trash_retention_days"], 30);
        assert_eq!(v["sync"]["body_index"], false);
        assert_eq!(v["sync"]["delete_guard"]["max_absolute"], 1000);
        assert_eq!(v["sync"]["change_source"], "inotify");
        // account roots are surfaced; id/username present
        assert_eq!(v["accounts"][0]["id"], "a");
        assert_eq!(v["accounts"][0]["username"], "a@outlook.com");
        assert!(v["accounts"][0]["sync_root"].is_string());
        assert!(v["accounts"][0]["archive_root"].is_string());
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
    fn index_wires_pagination() {
        for needle in ["loadMore", "&offset=", "&limit=", "more-btn"] {
            assert!(
                INDEX_HTML.contains(needle),
                "web UI is missing '{needle}' (pagination wiring)"
            );
        }
    }

    #[test]
    fn index_wires_overview_dashboard() {
        // the embedded UI must call the overview endpoints and expose the panel,
        // so the front-end wiring can't silently regress.
        for needle in [
            "/api/v1/status",
            "/api/v1/settings",
            "/api/v1/activity",
            "showOverview",
            "Overview",
            "Recent activity",
        ] {
            assert!(
                INDEX_HTML.contains(needle),
                "web UI is missing '{needle}' (overview dashboard wiring)"
            );
        }
    }

    #[test]
    fn activity_lists_recent_runs_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("arch");
        std::fs::create_dir_all(&arch).unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            store
                .add_run("a", "sync", "t1", "t2", "ok", "1 up")
                .unwrap();
            store
                .add_run("a", "backup", "t3", "t4", "ok", "mail 5")
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
        let v = body_json(&router.route(&ApiRequest::get("/api/v1/activity?account=a")));
        assert_eq!(v["count"], 2);
        assert_eq!(v["runs"][0]["kind"], "backup"); // newest first
        assert_eq!(v["runs"][0]["summary"], "mail 5");
        assert_eq!(v["runs"][1]["kind"], "sync");
        // missing account -> 400
        assert_eq!(
            router.route(&ApiRequest::get("/api/v1/activity")).status,
            400
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
    fn items_paginate_with_limit_and_offset() {
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("arch");
        std::fs::create_dir_all(&arch).unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            // five mail messages; stable order is by (item_type, name)
            for n in ["a", "b", "c", "d", "e"] {
                store
                    .upsert_item(&Item::new("a", "mail", n, format!("msg {n}"), "message"))
                    .unwrap();
            }
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

        // page 1: limit 2, offset 0 -> first two by name ("msg a", "msg b")
        let p1 = body_json(&router.route(&ApiRequest::get(
            "/api/v1/items?account=a&service=mail&limit=2&offset=0",
        )));
        assert_eq!(p1["total"], 5);
        assert_eq!(p1["count"], 2);
        assert_eq!(p1["limit"], 2);
        let names1: Vec<&str> = p1["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["name"].as_str().unwrap())
            .collect();
        assert_eq!(names1, ["msg a", "msg b"]);

        // page 2: offset 2 -> next two
        let p2 = body_json(&router.route(&ApiRequest::get(
            "/api/v1/items?account=a&service=mail&limit=2&offset=2",
        )));
        assert_eq!(p2["total"], 5);
        assert_eq!(p2["count"], 2);
        let names2: Vec<&str> = p2["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["name"].as_str().unwrap())
            .collect();
        assert_eq!(names2, ["msg c", "msg d"]);

        // last page: offset 4 -> one remaining
        let p3 = body_json(&router.route(&ApiRequest::get(
            "/api/v1/items?account=a&service=mail&limit=2&offset=4",
        )));
        assert_eq!(p3["count"], 1);
        assert_eq!(p3["items"][0]["name"], "msg e");

        // an over-large limit is capped; a bad limit falls back to the default
        assert_eq!(
            body_json(&router.route(&ApiRequest::get(
                "/api/v1/items?account=a&service=mail&limit=99999"
            )))["limit"],
            1000
        );
        assert_eq!(
            body_json(&router.route(&ApiRequest::get(
                "/api/v1/items?account=a&service=mail&limit=xyz"
            )))["limit"],
            200
        );
    }

    #[test]
    fn items_lists_a_service() {
        let (_d, router) = setup();
        let resp = router.route(&ApiRequest::get("/api/v1/items?account=a&service=mail"));
        assert_eq!(resp.status, 200);
        let v = body_json(&resp);
        assert_eq!(v["count"], 2);
        // pagination metadata: both items fit in the default page
        assert_eq!(v["total"], 2);
        assert_eq!(v["offset"], 0);
        assert_eq!(v["limit"], 200);
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
