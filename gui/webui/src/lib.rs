//! `isyncyou-webui` — the local web UI's request router (plan §25).
//!
//! The daemon serves a browser-based full-control UI on a local socket; this
//! crate is the **pure** request→response logic, independent of any HTTP server
//! or socket, so it is fully unit-testable. A thin server adapter (added with the
//! daemon) binds a listener and forwards each request to [`Router::route`].
//!
//! Endpoints:
//! - `GET /`                      → the static UI page
//! - `GET /api/v1/accounts`       → configured accounts
//! - `GET /api/v1/settings`                  → effective sync settings + account roots
//! - `GET /api/v1/activity?account[&limit]`  → recent engine runs (activity log)
//! - `GET /api/v1/status?account`            → per-service archive counts overview
//! - `GET /api/v1/items?account&service`     → archived items of a service
//! - `GET /api/v1/item?account&service&id`   → one item's metadata
//! - `GET /api/v1/body?account&service&id`   → archived body bytes (inert)
//! - `GET /api/v1/view?account&service&id`   → rendered safe HTML viewer page
//! - `GET /api/v1/open-external?url=…`        → explicit external-link confirmation
//! - `GET /api/v1/search?account&q`          → full-text search over item names
//! - `GET /api/v1/sync/state`                → scheduled-sync state
//! - `POST /api/v1/restore?account&service&id` → capability-token cloud restore
//! - `POST /api/v1/sync/{pause,resume,now}`  → capability-token sync control

use isyncyou_core::Config;
use isyncyou_store::{Item, Store};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

mod serve;
mod view;
#[cfg(unix)]
pub use serve::{default_unix_socket_path, serve_unix};
pub use serve::{format_http, parse_request_line, serve};

/// The embedded single-page UI (served at `/`). Talks to the JSON API via fetch.
pub const INDEX_HTML: &str = include_str!("index.html");
/// The redesigned UI's stylesheet and script, embedded + served same-origin from
/// `/app.css` and `/app.js`. Single-binary, no build step. `app.js` carries the
/// capability-token placeholders (injected per request, like the old inline script).
const APP_CSS: &str = include_str!("app.css");
const APP_JS: &str = include_str!("app.js");
/// Embedded Inter variable font (SIL OFL 1.1), served same-origin from `/app.woff2`
/// so the premium typography needs no web-font request (CSP `font-src 'self'`).
const APP_FONT: &[u8] = include_bytes!("assets/inter-var.woff2");

/// CSP for the app shell (`/`). `script-src 'self'` (no inline script) is the key
/// defense; only our own same-origin `/app.js` runs. Allows our stylesheet + inline
/// `style=` attributes (low-risk), data:-SVG (icons/charts/noise/favicon),
/// same-origin fetches, and the sandboxed object-viewer iframe (`frame-src 'self'`).
const APP_SHELL_CSP: &str = "default-src 'none'; script-src 'self'; \
     style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; \
     font-src 'self'; frame-src 'self'; base-uri 'none'; form-action 'none'; \
     frame-ancestors 'none'";

/// Services that can hold archived items (mirrors the CLI's `status`).
const STATUS_SERVICES: &[&str] = &[
    "onedrive", "mail", "calendar", "contacts", "todo", "onenote", "shared",
];

/// A parsed inbound request (method + path + decoded query pairs + an optional
/// capability token captured from the `X-Capability-Token` header).
#[derive(Debug, Clone)]
pub struct ApiRequest {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    /// The `X-Capability-Token` header value, required for destructive POSTs.
    pub cap_token: Option<String>,
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
            cap_token: None,
        }
    }

    /// Attach a captured capability token (builder style, used by the server).
    pub fn with_cap_token(mut self, token: Option<String>) -> Self {
        self.cap_token = token;
        self
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

/// Performs a destructive cloud action on behalf of a POST request. Injected by
/// the daemon (which owns the Graph/engine stack) so the router itself stays a
/// pure read surface. Returns the new cloud id on success.
pub trait RestoreHandler: Send + Sync {
    fn restore(&self, account: &str, service: &str, id: &str) -> Result<String, String>;
}

/// Creates an outbound sharing link for a OneDrive item on behalf of a POST
/// request (#494). Injected by the daemon (which owns the Graph stack). Returns
/// the link's `webUrl`.
pub trait ShareHandler: Send + Sync {
    fn share(
        &self,
        account: &str,
        service: &str,
        id: &str,
        link_type: &str,
        scope: &str,
    ) -> Result<String, String>;
    /// Invite named people to a OneDrive item by email (#504). `role` is `read` or
    /// `write`. Returns a short human summary (e.g. how many were invited).
    fn invite(
        &self,
        account: &str,
        service: &str,
        id: &str,
        emails: &[String],
        role: &str,
    ) -> Result<String, String>;
}

/// Controls the daemon's background scheduled sync from the UI: pause/resume the
/// scheduler and trigger an immediate pass. Injected by the daemon.
pub trait SyncControl: Send + Sync {
    fn pause(&self);
    fn resume(&self);
    /// Request an immediate sync pass (wakes the scheduler).
    fn trigger(&self);
    fn is_paused(&self) -> bool;
}

/// Reports in-flight FUSE placeholder hydrations (on-demand downloads), so the
/// status bar can show "downloading N file(s)". Implemented by the daemon's
/// hydration tracker.
pub trait HydrationStatus: Send + Sync {
    /// Display names of files currently materializing.
    fn active(&self) -> Vec<String>;
}

/// Runs an archive integrity verify pass for an account on behalf of a POST
/// (re-hashes every archived body, persists per-item status). Injected by the
/// daemon (which owns the engine); the read-only CLI `serve` does not set it.
/// Returns a short human summary.
pub trait VerifyHandler: Send + Sync {
    fn verify(&self, account: &str) -> Result<String, String>;
}

/// Persists mutable sync settings from the UI (currently the cloud-poll interval)
/// and applies them to the running daemon. Injected by the daemon; the read-only
/// CLI `serve` does not set it, so the settings POST is refused there.
pub trait SettingsHandler: Send + Sync {
    /// Set the active cloud-poll interval (seconds); the impl clamps to a sane
    /// range, updates the live value, and persists it to the config file.
    fn set_poll_interval_secs(&self, secs: u64) -> Result<(), String>;
}

/// A tiny change broadcaster for Server-Sent Events: the daemon's cloud-poll loop
/// calls [`EventBus::notify`] when it detects cloud changes; each open SSE
/// connection waits on a generation counter and streams a frame. Thread-safe and
/// lock-cheap (one `Mutex<u64>` + `Condvar`), no per-subscriber state.
#[derive(Default)]
pub struct EventBus {
    generation: std::sync::Mutex<u64>,
    cv: std::sync::Condvar,
}

impl EventBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a change and wake every waiting SSE connection.
    pub fn notify(&self) {
        let mut g = self.generation.lock().unwrap_or_else(|e| e.into_inner());
        *g = g.wrapping_add(1);
        self.cv.notify_all();
    }

    /// Current generation — an SSE handler reads this once, then waits for it to move.
    pub fn generation(&self) -> u64 {
        *self.generation.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Block until the generation differs from `last` or `timeout` elapses; returns
    /// the current generation. The timeout drives periodic SSE heartbeats.
    pub fn wait_change(&self, last: u64, timeout: std::time::Duration) -> u64 {
        let g = self.generation.lock().unwrap_or_else(|e| e.into_inner());
        let (g, _) = self
            .cv
            .wait_timeout_while(g, timeout, |cur| *cur == last)
            .unwrap_or_else(|e| e.into_inner());
        *g
    }
}

/// Routes requests against the configured accounts and their stores.
pub struct Router {
    config: Config,
    /// Optional change broadcaster for the SSE `/api/v1/events` stream (the
    /// daemon's). `None` => the events endpoint reports no live push.
    events: Option<std::sync::Arc<EventBus>>,
    /// Optional store-access gate, shared with a background syncer (the daemon's
    /// scheduler). When set, the whole request is serialized against the syncer so
    /// the per-request `Store::open` never races the single-instance file lock the
    /// sync pass holds. `None` for the CLI's single-threaded `serve` (no syncer).
    gate: Option<std::sync::Arc<std::sync::Mutex<()>>>,
    /// Optional destructive-action handler (the daemon's restore). `None` => the
    /// API is read-only and POSTs are refused.
    restore: Option<std::sync::Arc<dyn RestoreHandler>>,
    /// Per-process capability token required for restore POSTs. A cross-site page
    /// can't read it (CSRF defense); paired with POST-only + an owner-only socket
    /// it gates cloud mutations (plan §11).
    restore_cap_token: Option<String>,
    /// Separate capability token for scheduled-sync control POSTs. Keeping this
    /// distinct from the restore token limits the blast radius of a leaked token.
    sync_cap_token: Option<String>,
    /// Optional outbound-sharing handler (the daemon's). `None` => the share POST
    /// is refused.
    share: Option<std::sync::Arc<dyn ShareHandler>>,
    /// Separate capability token for share POSTs (distinct blast radius).
    share_cap_token: Option<String>,
    /// Optional scheduled-sync controller (the daemon's). Enables the sync
    /// pause/resume/now POSTs + the state GET.
    sync_control: Option<std::sync::Arc<dyn SyncControl>>,
    /// Optional FUSE hydration status (the daemon's). Enables the in-flight
    /// download list GET.
    hydrations: Option<std::sync::Arc<dyn HydrationStatus>>,
    /// Optional integrity-verify handler (the daemon's). `None` => the verify
    /// POST is refused (the read-only CLI `serve`).
    verify: Option<std::sync::Arc<dyn VerifyHandler>>,
    /// Separate capability token for verify POSTs.
    verify_cap_token: Option<String>,
    /// Optional mutable-settings handler (the daemon's). `None` => the settings
    /// POST is refused (the read-only CLI `serve`).
    settings_handler: Option<std::sync::Arc<dyn SettingsHandler>>,
    /// Separate capability token for settings POSTs.
    settings_cap_token: Option<String>,
}

impl Router {
    pub fn new(config: Config) -> Self {
        Router {
            config,
            events: None,
            gate: None,
            restore: None,
            restore_cap_token: None,
            sync_cap_token: None,
            share: None,
            share_cap_token: None,
            sync_control: None,
            hydrations: None,
            verify: None,
            verify_cap_token: None,
            settings_handler: None,
            settings_cap_token: None,
        }
    }

    /// Build a router that serializes store access against an external syncer via
    /// the shared `gate` mutex (used by the daemon, which also runs scheduled syncs).
    pub fn with_gate(config: Config, gate: std::sync::Arc<std::sync::Mutex<()>>) -> Self {
        Router {
            config,
            events: None,
            gate: Some(gate),
            restore: None,
            restore_cap_token: None,
            sync_cap_token: None,
            share: None,
            share_cap_token: None,
            sync_control: None,
            hydrations: None,
            verify: None,
            verify_cap_token: None,
            settings_handler: None,
            settings_cap_token: None,
        }
    }

    /// Enable the destructive restore POST, guarded by `cap_token` (builder style).
    pub fn with_restore(
        mut self,
        handler: std::sync::Arc<dyn RestoreHandler>,
        cap_token: String,
    ) -> Self {
        self.restore = Some(handler);
        self.restore_cap_token = Some(cap_token);
        self
    }

    /// Enable the outbound-sharing POST, guarded by `cap_token` (builder style).
    pub fn with_share(
        mut self,
        handler: std::sync::Arc<dyn ShareHandler>,
        cap_token: String,
    ) -> Self {
        self.share = Some(handler);
        self.share_cap_token = Some(cap_token);
        self
    }

    /// Enable the integrity-verify POST, guarded by `cap_token` (builder style).
    pub fn with_verify(
        mut self,
        handler: std::sync::Arc<dyn VerifyHandler>,
        cap_token: String,
    ) -> Self {
        self.verify = Some(handler);
        self.verify_cap_token = Some(cap_token);
        self
    }

    /// Enable the scheduled-sync control POSTs (pause/resume/now), guarded by the
    /// capability token, plus the read-only state GET (builder style).
    pub fn with_sync_control(
        mut self,
        control: std::sync::Arc<dyn SyncControl>,
        cap_token: String,
    ) -> Self {
        self.sync_control = Some(control);
        self.sync_cap_token = Some(cap_token);
        self
    }

    /// Enable the read-only FUSE hydration status GET (builder style).
    pub fn with_hydrations(mut self, hydrations: std::sync::Arc<dyn HydrationStatus>) -> Self {
        self.hydrations = Some(hydrations);
        self
    }

    /// Enable the SSE `/api/v1/events` change stream (builder style).
    pub fn with_events(mut self, events: std::sync::Arc<EventBus>) -> Self {
        self.events = Some(events);
        self
    }

    /// The injected SSE change bus, if any (used by the streaming server).
    pub(crate) fn events_bus(&self) -> Option<&std::sync::Arc<EventBus>> {
        self.events.as_ref()
    }

    /// Enable the mutable-settings POST, guarded by `cap_token` (builder style).
    pub fn with_settings(
        mut self,
        handler: std::sync::Arc<dyn SettingsHandler>,
        cap_token: String,
    ) -> Self {
        self.settings_handler = Some(handler);
        self.settings_cap_token = Some(cap_token);
        self
    }

    /// Whether the request carries the configured capability token.
    fn cap_ok(expected: &Option<String>, req: &ApiRequest) -> bool {
        matches!((expected, &req.cap_token), (Some(w), Some(g)) if w == g)
    }

    /// Append a durable audit entry to the account activity log. Destructive
    /// actions call this before invoking the injected handler so the intent is
    /// recorded even if the process dies after the remote mutation starts.
    fn audit_account(
        &self,
        account: &str,
        kind: &str,
        status: &str,
        summary: &str,
    ) -> Result<(), String> {
        let path = self
            .store_path(account)
            .ok_or_else(|| format!("unknown account '{account}'"))?;
        let store = Store::open(path).map_err(|e| e.to_string())?;
        let now = audit_timestamp();
        store
            .add_run(account, kind, &now, &now, status, &audit_summary(summary))
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Dispatch one request to a response. Never panics; unknown routes → 404.
    pub fn route(&self, req: &ApiRequest) -> ApiResponse {
        // Hold the store-access gate (if any) for the whole request so a concurrent
        // sync pass and this request never both hold the store's single-instance lock.
        let _gate = self
            .gate
            .as_ref()
            .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()));
        if req.method == "POST" {
            return match req.path.as_str() {
                "/api/v1/restore" => self.restore(req),
                "/api/v1/share" => self.share_link(req),
                "/api/v1/sync/pause" => self.sync_command(req, |c| c.pause()),
                "/api/v1/sync/resume" => self.sync_command(req, |c| c.resume()),
                "/api/v1/sync/now" => self.sync_command(req, |c| c.trigger()),
                "/api/v1/verify" => self.verify_run(req),
                "/api/v1/settings" => self.update_settings(req),
                _ => ApiResponse::error(405, "method not allowed"),
            };
        }
        if req.method != "GET" {
            return ApiResponse::error(405, "method not allowed");
        }
        match req.path.as_str() {
            // The shell is static; the strict app CSP header locks it to our assets.
            "/" => ApiResponse::html_with_csp(INDEX_HTML, APP_SHELL_CSP),
            // app.js carries the (same-origin) capability tokens so the UI can POST
            // restore/share/sync; empty when an action is disabled, hiding its UI.
            "/app.js" => ApiResponse {
                status: 200,
                content_type: "application/javascript; charset=utf-8".into(),
                body: APP_JS
                    .replace(
                        "__RESTORE_CAP_TOKEN__",
                        self.restore_cap_token.as_deref().unwrap_or(""),
                    )
                    .replace(
                        "__SYNC_CAP_TOKEN__",
                        self.sync_cap_token.as_deref().unwrap_or(""),
                    )
                    .replace(
                        "__SHARE_CAP_TOKEN__",
                        self.share_cap_token.as_deref().unwrap_or(""),
                    )
                    .replace(
                        "__VERIFY_CAP_TOKEN__",
                        self.verify_cap_token.as_deref().unwrap_or(""),
                    )
                    .replace(
                        "__SETTINGS_CAP_TOKEN__",
                        self.settings_cap_token.as_deref().unwrap_or(""),
                    )
                    .into_bytes(),
                // embedded assets change only on a binary upgrade; never let the
                // browser serve a stale copy across versions.
                headers: vec![("Cache-Control".into(), "no-store".into())],
            },
            "/app.css" => ApiResponse {
                status: 200,
                content_type: "text/css; charset=utf-8".into(),
                body: APP_CSS.as_bytes().to_vec(),
                headers: vec![("Cache-Control".into(), "no-store".into())],
            },
            "/app.woff2" => ApiResponse {
                status: 200,
                content_type: "font/woff2".into(),
                body: APP_FONT.to_vec(),
                // immutable binary asset → cache hard within a version
                headers: vec![("Cache-Control".into(), "max-age=31536000".into())],
            },
            "/api/v1/accounts" => self.accounts(),
            "/api/v1/settings" => self.settings(),
            "/api/v1/activity" => self.activity(req),
            "/api/v1/status" => self.status(req),
            "/api/v1/items" => self.items(req),
            "/api/v1/item" => self.item(req),
            "/api/v1/body" => self.body(req),
            "/api/v1/view" => self.view(req),
            "/api/v1/open-external" => self.open_external(req),
            "/api/v1/search" => self.search(req),
            "/api/v1/sync/state" => self.sync_state(),
            "/api/v1/hydrations" => self.hydrations_state(),
            _ => ApiResponse::error(404, "not found"),
        }
    }

    /// `POST /api/v1/settings?poll_interval_secs=N` — persist + apply a mutable
    /// sync setting. Requires the capability token; the work is the injected handler.
    fn update_settings(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match &self.settings_handler {
            Some(h) => h,
            None => return ApiResponse::error(404, "settings are not editable on this server"),
        };
        if !Self::cap_ok(&self.settings_cap_token, req) {
            return ApiResponse::error(401, "missing or invalid capability token");
        }
        let secs = match req
            .q("poll_interval_secs")
            .and_then(|v| v.parse::<u64>().ok())
        {
            Some(s) if (1..=3600).contains(&s) => s,
            _ => {
                return ApiResponse::error(400, "poll_interval_secs must be an integer in 1..=3600")
            }
        };
        match handler.set_poll_interval_secs(secs) {
            Ok(()) => ApiResponse::ok_json(&serde_json::json!({ "poll_interval_secs": secs })),
            Err(e) => ApiResponse::error(500, &format!("settings: {e}")),
        }
    }

    /// `POST /api/v1/restore?account&service&id` — re-create an archived item in the
    /// cloud. Requires the capability token; the actual work is the injected handler.
    fn restore(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match &self.restore {
            Some(h) => h,
            None => return ApiResponse::error(404, "restore is not enabled on this server"),
        };
        if !Self::cap_ok(&self.restore_cap_token, req) {
            return ApiResponse::error(401, "missing or invalid capability token");
        }
        let (account, service, id) = match (req.q("account"), req.q("service"), req.q("id")) {
            (Some(a), Some(s), Some(i)) if !a.is_empty() && !s.is_empty() && !i.is_empty() => {
                (a, s, i)
            }
            _ => return ApiResponse::error(400, "account, service and id are required"),
        };
        if let Err(e) = self.audit_account(
            account,
            "audit:restore",
            "started",
            &format!("restore requested service={service} id={id}"),
        ) {
            return ApiResponse::error(500, &format!("audit: {e}"));
        }
        match handler.restore(account, service, id) {
            Ok(new_id) => {
                let _ = self.audit_account(
                    account,
                    "audit:restore",
                    "ok",
                    &format!("restore ok service={service} id={id} new_id={new_id}"),
                );
                ApiResponse::ok_json(
                    &json!({ "restored": id, "service": service, "new_id": new_id }),
                )
            }
            Err(e) => {
                let _ = self.audit_account(
                    account,
                    "audit:restore",
                    "error",
                    &format!("restore error service={service} id={id}: {e}"),
                );
                ApiResponse::error(500, &e)
            }
        }
    }

    /// `POST /api/v1/verify?account` — re-hash every archived body and persist the
    /// per-item integrity status. Requires the verify capability token; the work
    /// is the injected handler (the daemon's engine verify pass, which records its
    /// own `verify` run). Returns the fresh integrity counts.
    fn verify_run(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match &self.verify {
            Some(h) => h,
            None => return ApiResponse::error(404, "verify is not enabled on this server"),
        };
        if !Self::cap_ok(&self.verify_cap_token, req) {
            return ApiResponse::error(401, "missing or invalid capability token");
        }
        let account = match req.q("account") {
            Some(a) if !a.is_empty() => a,
            _ => return ApiResponse::error(400, "account is required"),
        };
        match handler.verify(account) {
            Ok(summary) => {
                let (checked, verified) = self
                    .store_path(account)
                    .and_then(|p| Store::open(p).ok())
                    .and_then(|s| s.verify_counts(account).ok())
                    .unwrap_or((0, 0));
                ApiResponse::ok_json(&json!({
                    "account": account, "summary": summary,
                    "checked": checked, "verified": verified,
                }))
            }
            Err(e) => ApiResponse::error(500, &e),
        }
    }

    /// `POST /api/v1/share?account&service&id[&type&scope]` — create an outbound
    /// sharing link for a OneDrive item. Requires the share capability token; the
    /// actual Graph call is the injected handler. `type` defaults to `view`, `scope`
    /// to `anonymous`.
    fn share_link(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match &self.share {
            Some(h) => h,
            None => return ApiResponse::error(404, "sharing is not enabled on this server"),
        };
        if !Self::cap_ok(&self.share_cap_token, req) {
            return ApiResponse::error(401, "missing or invalid capability token");
        }
        let (account, service, id) = match (req.q("account"), req.q("service"), req.q("id")) {
            (Some(a), Some(s), Some(i)) if !a.is_empty() && !s.is_empty() && !i.is_empty() => {
                (a, s, i)
            }
            _ => return ApiResponse::error(400, "account, service and id are required"),
        };
        // Invite mode (#504): an `email` param (comma/space-separated) invites named
        // people instead of creating an anonymous link. `role` = read|write.
        if let Some(emails_raw) = req.q("email").filter(|e| !e.is_empty()) {
            let emails: Vec<String> = emails_raw
                .split([',', ' '])
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .map(String::from)
                .collect();
            if emails.is_empty() {
                return ApiResponse::error(400, "no valid email address");
            }
            let role = req.q("role").filter(|r| !r.is_empty()).unwrap_or("read");
            if let Err(e) = self.audit_account(
                account,
                "audit:share",
                "started",
                &format!(
                    "invite requested service={service} id={id} role={role} n={}",
                    emails.len()
                ),
            ) {
                return ApiResponse::error(500, &format!("audit: {e}"));
            }
            return match handler.invite(account, service, id, &emails, role) {
                Ok(summary) => {
                    let _ = self.audit_account(
                        account,
                        "audit:share",
                        "ok",
                        &format!("invite ok service={service} id={id} role={role}"),
                    );
                    ApiResponse::ok_json(
                        &json!({ "invited": emails, "service": service, "role": role, "summary": summary }),
                    )
                }
                Err(e) => {
                    let _ = self.audit_account(
                        account,
                        "audit:share",
                        "error",
                        &format!("invite error service={service} id={id}: {e}"),
                    );
                    ApiResponse::error(500, &e)
                }
            };
        }
        let link_type = req.q("type").filter(|t| !t.is_empty()).unwrap_or("view");
        let scope = req
            .q("scope")
            .filter(|s| !s.is_empty())
            .unwrap_or("anonymous");
        if let Err(e) = self.audit_account(
            account,
            "audit:share",
            "started",
            &format!("share requested service={service} id={id} type={link_type} scope={scope}"),
        ) {
            return ApiResponse::error(500, &format!("audit: {e}"));
        }
        match handler.share(account, service, id, link_type, scope) {
            Ok(web_url) => {
                let _ = self.audit_account(
                    account,
                    "audit:share",
                    "ok",
                    &format!("share ok service={service} id={id} type={link_type}"),
                );
                ApiResponse::ok_json(
                    &json!({ "shared": id, "service": service, "type": link_type, "webUrl": web_url }),
                )
            }
            Err(e) => {
                let _ = self.audit_account(
                    account,
                    "audit:share",
                    "error",
                    &format!("share error service={service} id={id}: {e}"),
                );
                ApiResponse::error(500, &e)
            }
        }
    }

    /// Apply a capability-token-guarded scheduled-sync command (pause/resume/now),
    /// then report the resulting paused state.
    fn sync_command(&self, req: &ApiRequest, apply: impl Fn(&dyn SyncControl)) -> ApiResponse {
        let control = match &self.sync_control {
            Some(c) => c,
            None => return ApiResponse::error(404, "scheduled sync is not enabled on this server"),
        };
        if !Self::cap_ok(&self.sync_cap_token, req) {
            return ApiResponse::error(401, "missing or invalid capability token");
        }
        apply(control.as_ref());
        ApiResponse::ok_json(&json!({ "paused": control.is_paused() }))
    }

    /// `GET /api/v1/sync/state` — whether scheduled sync is enabled and paused.
    fn sync_state(&self) -> ApiResponse {
        match &self.sync_control {
            Some(c) => ApiResponse::ok_json(&json!({ "enabled": true, "paused": c.is_paused() })),
            None => ApiResponse::ok_json(&json!({ "enabled": false, "paused": false })),
        }
    }

    /// In-flight FUSE placeholder hydrations (on-demand downloads). `active` is the
    /// list of file names currently materializing; `count` its length.
    fn hydrations_state(&self) -> ApiResponse {
        let active = self
            .hydrations
            .as_ref()
            .map(|h| h.active())
            .unwrap_or_default();
        ApiResponse::ok_json(&json!({ "count": active.len(), "active": active }))
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
        // Real integrity signal: how many archived bodies last passed the verify
        // hash-check, and when verify last ran (backs "Integrity verified N%").
        let (checked, verified) = store.verify_counts(account).unwrap_or((0, 0));
        let last_verified = store.recent_runs(account, 50).ok().and_then(|runs| {
            runs.into_iter()
                .find(|r| r.kind == "verify")
                .map(|r| r.finished_at)
        });
        ApiResponse::ok_json(&json!({
            "account": account,
            "services": services,
            "totals": { "items": total_items, "archived": total_archived },
            "onedrive_cursor": onedrive_cursor,
            "verify": { "checked": checked, "verified": verified, "last_verified": last_verified },
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
        // Folder navigation for the file explorer: `?parent=<id>` lists that
        // folder's direct children; `?parent=root` (or empty) lists the top-level
        // items (those under the untracked drive root). Un-paginated — a single
        // folder is bounded — and additive: the call without `parent` keeps the
        // flat paginated behaviour every other view relies on.
        if let Some(parent) = req.q("parent") {
            let kids = if parent.is_empty() || parent == "root" {
                store.roots(account, service)
            } else {
                store.children(account, service, Some(parent))
            };
            return match kids {
                Ok(items) => {
                    let arr: Vec<Value> = items.iter().map(item_json).collect();
                    ApiResponse::ok_json(&json!({
                        "items": arr,
                        "count": arr.len(),
                        "total": arr.len(),
                        "limit": arr.len(),
                        "offset": 0,
                        "parent": parent,
                    }))
                }
                Err(e) => ApiResponse::error(500, &format!("query: {e}")),
            };
        }
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
                // Rows are enriched with a read-only `preview` parsed from the
                // archived body on disk, so the bespoke views render richly without an
                // extra request per item. Additive + best-effort: items without a
                // readable body simply carry no `preview`. Bounded by the page size.
                // mail = sender/snippet/date/has-html (.eml); the rest parse the
                // archived JSON (calendar/contacts/todo).
                let arr: Vec<Value> =
                    if matches!(service, "mail" | "calendar" | "contacts" | "todo") {
                        let archive_root = self
                            .config
                            .accounts
                            .iter()
                            .find(|a| a.id == account)
                            .map(|a| a.archive_root.clone());
                        items
                            .iter()
                            .map(|it| {
                                let mut v = item_json(it);
                                if let (Some(root), Some(rel)) =
                                    (archive_root.as_ref(), it.local_path.as_ref())
                                {
                                    if let Some(bytes) = read_under_root(root, rel) {
                                        if service == "mail" {
                                            let p = isyncyou_connectors::mail_preview(&bytes);
                                            v["preview"] = json!({
                                                "from": p.from,
                                                "to": p.to,
                                                "subject": p.subject,
                                                "snippet": p.body_snippet,
                                                "date": p.date,
                                                "has_html": p.has_html,
                                                "attachments": p.attachment_count,
                                                "size": p.size_bytes,
                                            });
                                        } else if let Ok(o) =
                                            serde_json::from_slice::<Value>(&bytes)
                                        {
                                            v["preview"] = match service {
                                                "calendar" => json!({
                                                    "start": o["start"]["dateTime"],
                                                    "start_tz": o["start"]["timeZone"],
                                                    "end": o["end"]["dateTime"],
                                                    "end_tz": o["end"]["timeZone"],
                                                    "all_day": o["isAllDay"],
                                                    "location": o["location"]["displayName"],
                                                }),
                                                "contacts" => json!({
                                                    "company": o["companyName"],
                                                    "job": o["jobTitle"],
                                                    "email": o["emailAddresses"][0]["address"],
                                                }),
                                                _ => json!({
                                                    "status": o["status"],
                                                    "importance": o["importance"],
                                                    "due": o["dueDateTime"]["dateTime"],
                                                    "has_note": o["body"]["content"]
                                                        .as_str()
                                                        .map(|s| !s.trim().is_empty())
                                                        .unwrap_or(false),
                                                }),
                                            };
                                        }
                                    }
                                }
                                v
                            })
                            .collect()
                    } else {
                        items.iter().map(item_json).collect()
                    };
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

    /// Read an item's body bytes, path-safely. Returns `(relative_path, bytes,
    /// item_name)` or the `ApiResponse` to send on failure. The resolved file must
    /// stay under the item's root (defense against `..`/symlink traversal): an
    /// archived service (mail/calendar/…) stores its body under `archive_root`,
    /// whereas a OneDrive file is the materialized file under `sync_root`. Shared
    /// by [`Self::body`] and [`Self::view`].
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
        // OneDrive `local_path` is relative to the synced folder; every archived
        // service stores its body under the archive root.
        let body_root = if service == "onedrive" {
            acc.sync_root.clone()
        } else {
            acc.archive_root.clone()
        };
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
        let path = body_root.join(&rel);
        match (path.canonicalize(), body_root.canonicalize()) {
            (Ok(p), Ok(root)) if p.starts_with(&root) => match std::fs::read(&p) {
                Ok(bytes) => Ok((rel, bytes, name)),
                Err(e) => Err(ApiResponse::error(500, &format!("read: {e}"))),
            },
            (Ok(_), Ok(_)) => Err(ApiResponse::error(400, "body path escapes its root")),
            _ => Err(ApiResponse::error(404, "body file missing")),
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
            if let Some(html) = isyncyou_connectors::extract_html_with_inline_images(&bytes) {
                let subject = if name.is_empty() { "Message" } else { &name };
                // external content (remote images + web fonts) is opt-in (the
                // reader's "Load external content" button adds ?external=1) —
                // default blocks it (tracking pixels + privacy)
                let external = req.q("external") == Some("1");
                let inline_images: Vec<_> = html
                    .inline_images
                    .iter()
                    .map(|img| view::InlineImageRef {
                        cid: &img.cid,
                        content_type: &img.content_type,
                        data: &img.data,
                    })
                    .collect();
                return ApiResponse::html_with_csp(
                    &view::mail_page_with_inline_images(
                        subject,
                        &html.html,
                        &inline_images,
                        external,
                    ),
                    view::mail_csp(external),
                );
            }
        }
        // A OneNote page is archived raw HTML → render through the same allowlist
        // sanitizer (scripts removed, remote resources blocked) under MAIL_CSP.
        if service == "onenote" {
            let title = if name.is_empty() { "Note" } else { &name };
            return ApiResponse::html_with_csp(
                &view::note_page(title, &String::from_utf8_lossy(&bytes)),
                view::MAIL_CSP,
            );
        }
        ApiResponse::html_with_csp(
            &view::source_page(service, &String::from_utf8_lossy(&bytes)),
            view::VIEWER_CSP,
        )
    }

    /// Confirm before navigating to a URL that came from archived mail. The page
    /// contains a normal link only after the target has passed a small `http(s)`
    /// allowlist; it never fetches or opens the target automatically.
    fn open_external(&self, req: &ApiRequest) -> ApiResponse {
        let url = match req.q("url") {
            Some(url) => url,
            None => return ApiResponse::error(400, "missing 'url'"),
        };
        match view::external_link_dialog_page(url) {
            Some(page) => ApiResponse::html_with_csp(&page, view::VIEWER_CSP),
            None => ApiResponse::error(400, "unsafe external URL"),
        }
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

fn audit_timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}

fn audit_summary(summary: &str) -> String {
    const MAX: usize = 400;
    let mut out: String = summary.chars().take(MAX).collect();
    if summary.chars().count() > MAX {
        out.push_str("...");
    }
    out
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
    let lower = rel.to_ascii_lowercase();
    // Raster images get their real type so the explorer can show thumbnails;
    // `nosniff` (always emitted) keeps them inert. SVG is deliberately excluded —
    // it can carry scripts — and falls through to inert text/plain.
    if lower.ends_with(".json") {
        "application/json; charset=utf-8"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".bmp") {
        "image/bmp"
    } else if lower.ends_with(".ico") {
        "image/x-icon"
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
        "etag": it.etag,
        "has_body": it.local_path.is_some(),
        "verify_status": it.verify_status,
        "verified_at": it.verified_at,
    })
}

/// Best-effort read of an archived body file that must stay under `archive_root`
/// (defends against `..`/symlink traversal, like [`Router::read_archived`]).
/// Returns `None` on any failure so callers can degrade gracefully. Used to
/// enrich the mail listing with previews.
fn read_under_root(archive_root: &std::path::Path, rel: &str) -> Option<Vec<u8>> {
    let root = archive_root.canonicalize().ok()?;
    let p = archive_root.join(rel).canonicalize().ok()?;
    if !p.starts_with(&root) {
        return None;
    }
    std::fs::read(&p).ok()
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
                mount_point: None,
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
    fn open_external_confirms_only_safe_http_urls() {
        let router = Router::new(Config::default());
        let ok = router.route(&ApiRequest::get(
            "/api/v1/open-external?url=https%3A%2F%2Fexample.test%2Fa%3Fx%3D1",
        ));
        assert_eq!(ok.status, 200);
        assert!(ok.content_type.starts_with("text/html"));
        let body = String::from_utf8_lossy(&ok.body);
        assert!(
            body.contains("href=\"https://example.test/a?x=1\""),
            "confirmed external link missing: {body}"
        );
        assert!(
            ok.headers.iter().any(
                |(k, val)| k == "Content-Security-Policy" && val.contains("default-src 'none'")
            ),
            "dialog must carry a strict CSP header"
        );

        let js = router.route(&ApiRequest::get(
            "/api/v1/open-external?url=javascript%3Aalert%281%29",
        ));
        assert_eq!(js.status, 400);
        assert_eq!(
            router
                .route(&ApiRequest::get("/api/v1/open-external"))
                .status,
            400
        );
        assert_eq!(
            router
                .route(&ApiRequest::new(
                    "POST",
                    "/api/v1/open-external?url=https%3A%2F%2Fexample.test"
                ))
                .status,
            405
        );
    }

    #[test]
    fn gated_router_serves_and_releases_the_gate() {
        // a router built with a shared store-access gate acquires it per request
        // and releases it afterwards, so the daemon's sync thread and the web UI
        // never hold the store's single-instance lock at the same time.
        let gate = std::sync::Arc::new(std::sync::Mutex::new(()));
        let router = Router::with_gate(Config::default(), gate.clone());
        let resp = router.route(&ApiRequest::get("/api/v1/accounts"));
        assert_eq!(resp.status, 200);
        assert!(
            gate.try_lock().is_ok(),
            "the gate must be free again once the request returns"
        );
    }

    struct OkRestore;
    impl RestoreHandler for OkRestore {
        fn restore(&self, _a: &str, _s: &str, _i: &str) -> Result<String, String> {
            Ok("new-cloud-id".into())
        }
    }

    struct ErrRestore;
    impl RestoreHandler for ErrRestore {
        fn restore(&self, _a: &str, _s: &str, _i: &str) -> Result<String, String> {
            Err("graph refused restore".into())
        }
    }

    #[test]
    fn restore_post_requires_a_valid_capability_token() {
        let (_d, router) = setup();
        let router = router.with_restore(std::sync::Arc::new(OkRestore), "secret".into());
        let q = "/api/v1/restore?account=a&service=mail&id=x";
        // no token / wrong token -> 401
        assert_eq!(router.route(&ApiRequest::new("POST", q)).status, 401);
        assert_eq!(
            router
                .route(&ApiRequest::new("POST", q).with_cap_token(Some("nope".into())))
                .status,
            401
        );
        // correct token -> 200 + the new cloud id
        let ok = router.route(&ApiRequest::new("POST", q).with_cap_token(Some("secret".into())));
        assert_eq!(ok.status, 200);
        assert!(String::from_utf8_lossy(&ok.body).contains("new-cloud-id"));
        // valid token but missing params -> 400
        let bad = ApiRequest::new("POST", "/api/v1/restore?account=a")
            .with_cap_token(Some("secret".into()));
        assert_eq!(router.route(&bad).status, 400);
    }

    struct OkSettings(std::sync::Arc<std::sync::atomic::AtomicU64>);
    impl SettingsHandler for OkSettings {
        fn set_poll_interval_secs(&self, secs: u64) -> Result<(), String> {
            self.0.store(secs, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn settings_post_requires_cap_token_and_validates_interval() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let (_d, router) = setup();
        // read-only router (no handler injected) refuses the settings POST
        assert_eq!(
            router
                .route(&ApiRequest::new("POST", "/api/v1/settings?poll_interval_secs=10"))
                .status,
            404
        );
        let seen = std::sync::Arc::new(AtomicU64::new(0));
        let router = router.with_settings(std::sync::Arc::new(OkSettings(seen.clone())), "secret".into());
        let q = "/api/v1/settings?poll_interval_secs=10";
        // no / wrong token -> 401
        assert_eq!(router.route(&ApiRequest::new("POST", q)).status, 401);
        assert_eq!(
            router
                .route(&ApiRequest::new("POST", q).with_cap_token(Some("nope".into())))
                .status,
            401
        );
        // valid token but out-of-range value -> 400, handler not called
        let oob = ApiRequest::new("POST", "/api/v1/settings?poll_interval_secs=99999")
            .with_cap_token(Some("secret".into()));
        assert_eq!(router.route(&oob).status, 400);
        assert_eq!(seen.load(Ordering::SeqCst), 0);
        // valid token + valid value -> 200, handler applied
        let ok = router.route(&ApiRequest::new("POST", q).with_cap_token(Some("secret".into())));
        assert_eq!(ok.status, 200);
        assert_eq!(seen.load(Ordering::SeqCst), 10);
    }

    struct OkVerify;
    impl VerifyHandler for OkVerify {
        fn verify(&self, _a: &str) -> Result<String, String> {
            Ok("224 verified, 0 changed, 0 failed of 224".into())
        }
    }

    #[test]
    fn verify_post_requires_token_and_is_disabled_without_handler() {
        let (_d, router) = setup();
        let q = "/api/v1/verify?account=a";
        // not enabled (read-only serve) -> 404
        assert_eq!(router.route(&ApiRequest::new("POST", q)).status, 404);
        let router = router.with_verify(std::sync::Arc::new(OkVerify), "secret".into());
        // no / wrong token -> 401
        assert_eq!(router.route(&ApiRequest::new("POST", q)).status, 401);
        assert_eq!(
            router
                .route(&ApiRequest::new("POST", q).with_cap_token(Some("nope".into())))
                .status,
            401
        );
        // correct token -> 200 + summary
        let ok = router.route(&ApiRequest::new("POST", q).with_cap_token(Some("secret".into())));
        assert_eq!(ok.status, 200);
        assert!(String::from_utf8_lossy(&ok.body).contains("verified"));
        // missing account -> 400
        let bad = ApiRequest::new("POST", "/api/v1/verify").with_cap_token(Some("secret".into()));
        assert_eq!(router.route(&bad).status, 400);
    }

    struct OkShare;
    impl ShareHandler for OkShare {
        fn share(
            &self,
            _a: &str,
            _s: &str,
            _i: &str,
            _t: &str,
            _sc: &str,
        ) -> Result<String, String> {
            Ok("https://1drv.ms/x/abc".into())
        }
        fn invite(
            &self,
            _a: &str,
            _s: &str,
            _i: &str,
            emails: &[String],
            role: &str,
        ) -> Result<String, String> {
            Ok(format!("invited {} ({role})", emails.len()))
        }
    }

    #[test]
    fn share_post_requires_token_returns_weburl_and_is_disabled_without_handler() {
        let (_d, router) = setup();
        let router = router.with_share(std::sync::Arc::new(OkShare), "secret".into());
        let q = "/api/v1/share?account=a&service=onedrive&id=x";
        // no token / wrong token -> 401
        assert_eq!(router.route(&ApiRequest::new("POST", q)).status, 401);
        assert_eq!(
            router
                .route(&ApiRequest::new("POST", q).with_cap_token(Some("nope".into())))
                .status,
            401
        );
        // correct token -> 200 + the webUrl
        let ok = router.route(&ApiRequest::new("POST", q).with_cap_token(Some("secret".into())));
        assert_eq!(ok.status, 200);
        assert!(String::from_utf8_lossy(&ok.body).contains("https://1drv.ms/x/abc"));
        // valid token but missing params -> 400
        let bad = ApiRequest::new("POST", "/api/v1/share?account=a")
            .with_cap_token(Some("secret".into()));
        assert_eq!(router.route(&bad).status, 400);
        // a router without a share handler refuses the POST -> 404
        let (_d2, plain) = setup();
        assert_eq!(
            plain
                .route(&ApiRequest::new("POST", q).with_cap_token(Some("secret".into())))
                .status,
            404
        );
    }

    #[test]
    fn share_post_invite_mode_routes_to_invite_not_link() {
        let (_d, router) = setup();
        let router = router.with_share(std::sync::Arc::new(OkShare), "secret".into());
        // an `email` param switches to invite mode: response has no webUrl, role echoed
        let q = "/api/v1/share?account=a&service=onedrive&id=x&email=p%40e.com&role=write";
        let ok = router.route(&ApiRequest::new("POST", q).with_cap_token(Some("secret".into())));
        assert_eq!(ok.status, 200);
        let body = String::from_utf8_lossy(&ok.body);
        assert!(
            body.contains("\"invited\""),
            "expected invited list: {body}"
        );
        assert!(body.contains("p@e.com") && body.contains("write"));
        assert!(!body.contains("webUrl"), "invite must not create a link");
        // invite still needs the capability token
        assert_eq!(router.route(&ApiRequest::new("POST", q)).status, 401);
    }

    #[test]
    fn restore_post_writes_a_durable_audit_log() {
        let (_d, router) = setup();
        let router = router.with_restore(std::sync::Arc::new(OkRestore), "secret".into());
        let q = "/api/v1/restore?account=a&service=mail&id=m1";

        let ok = router.route(&ApiRequest::new("POST", q).with_cap_token(Some("secret".into())));
        assert_eq!(ok.status, 200);

        let audit =
            body_json(&router.route(&ApiRequest::get("/api/v1/activity?account=a&limit=5")));
        assert_eq!(audit["runs"][0]["kind"], "audit:restore");
        assert_eq!(audit["runs"][0]["status"], "ok");
        assert!(audit["runs"][0]["summary"]
            .as_str()
            .unwrap()
            .contains("new_id=new-cloud-id"));
        assert_eq!(audit["runs"][1]["kind"], "audit:restore");
        assert_eq!(audit["runs"][1]["status"], "started");
        assert!(audit["runs"][1]["summary"]
            .as_str()
            .unwrap()
            .contains("service=mail id=m1"));
        assert!(
            !audit["runs"][0]["summary"]
                .as_str()
                .unwrap()
                .contains("secret"),
            "capability tokens must never be logged"
        );
    }

    #[test]
    fn restore_post_audits_handler_errors() {
        let (_d, router) = setup();
        let router = router.with_restore(std::sync::Arc::new(ErrRestore), "secret".into());
        let q = "/api/v1/restore?account=a&service=mail&id=m1";

        let err = router.route(&ApiRequest::new("POST", q).with_cap_token(Some("secret".into())));
        assert_eq!(err.status, 500);

        let audit =
            body_json(&router.route(&ApiRequest::get("/api/v1/activity?account=a&limit=5")));
        assert_eq!(audit["runs"][0]["kind"], "audit:restore");
        assert_eq!(audit["runs"][0]["status"], "error");
        assert!(audit["runs"][0]["summary"]
            .as_str()
            .unwrap()
            .contains("graph refused restore"));
        assert_eq!(audit["runs"][1]["status"], "started");
    }

    #[test]
    fn app_js_injects_separate_capability_tokens_when_enabled() {
        // the capability tokens are injected into the same-origin /app.js (served as
        // a script), not the static shell. read-only router: placeholders blanked.
        let ro = Router::new(Config::default()).route(&ApiRequest::get("/app.js"));
        assert!(ro.content_type.starts_with("application/javascript"));
        let ro_body = String::from_utf8_lossy(&ro.body);
        assert!(
            !ro_body.contains("__RESTORE_CAP_TOKEN__") && !ro_body.contains("__SYNC_CAP_TOKEN__"),
            "placeholder must be replaced"
        );
        assert!(
            ro_body.contains("restore: \"\"") && ro_body.contains("sync: \"\""),
            "no tokens when read-only"
        );
        // restore/sync-enabled router: distinct real tokens are injected.
        let sync = std::sync::Arc::new(MockSync {
            paused: false.into(),
            triggered: false.into(),
        });
        let rw = Router::new(Config::default())
            .with_restore(std::sync::Arc::new(OkRestore), "restore123".into())
            .with_sync_control(sync, "sync123".into())
            .route(&ApiRequest::get("/app.js"));
        let rw_body = String::from_utf8_lossy(&rw.body);
        assert!(rw_body.contains("restore: \"restore123\""));
        assert!(rw_body.contains("sync: \"sync123\""));
    }

    #[test]
    fn app_css_served_with_css_content_type() {
        let resp = Router::new(Config::default()).route(&ApiRequest::get("/app.css"));
        assert_eq!(resp.status, 200);
        assert!(resp.content_type.starts_with("text/css"));
    }

    #[test]
    fn app_shell_carries_strict_csp_header() {
        // the shell `/` must lock script execution to same-origin (no inline script).
        let resp = Router::new(Config::default()).route(&ApiRequest::get("/"));
        assert!(resp.content_type.starts_with("text/html"));
        assert!(
            resp.headers
                .iter()
                .any(|(k, v)| k == "Content-Security-Policy"
                    && v.contains("script-src 'self'")
                    && v.contains("default-src 'none'")),
            "app shell must carry a strict CSP header"
        );
    }

    struct MockSync {
        paused: std::sync::atomic::AtomicBool,
        triggered: std::sync::atomic::AtomicBool,
    }
    impl SyncControl for MockSync {
        fn pause(&self) {
            self.paused.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        fn resume(&self) {
            self.paused
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
        fn trigger(&self) {
            self.triggered
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        fn is_paused(&self) -> bool {
            self.paused.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[test]
    fn sync_control_state_and_token_guarded_commands() {
        let m = std::sync::Arc::new(MockSync {
            paused: false.into(),
            triggered: false.into(),
        });
        let router = Router::new(Config::default()).with_sync_control(m.clone(), "s".into());
        // state is read-only (no token)
        let st = router.route(&ApiRequest::get("/api/v1/sync/state"));
        assert!(String::from_utf8_lossy(&st.body).contains("\"enabled\":true"));
        // pause needs the token, then flips the flag
        assert_eq!(
            router
                .route(&ApiRequest::new("POST", "/api/v1/sync/pause"))
                .status,
            401
        );
        let ok = router
            .route(&ApiRequest::new("POST", "/api/v1/sync/pause").with_cap_token(Some("s".into())));
        assert_eq!(ok.status, 200);
        assert!(m.is_paused());
        router.route(
            &ApiRequest::new("POST", "/api/v1/sync/resume").with_cap_token(Some("s".into())),
        );
        assert!(!m.is_paused());
        router.route(&ApiRequest::new("POST", "/api/v1/sync/now").with_cap_token(Some("s".into())));
        assert!(m.triggered.load(std::sync::atomic::Ordering::SeqCst));
        // a router without a controller reports disabled and refuses the POST
        let ro = Router::new(Config::default());
        assert!(
            String::from_utf8_lossy(&ro.route(&ApiRequest::get("/api/v1/sync/state")).body)
                .contains("\"enabled\":false")
        );
        assert_eq!(
            ro.route(&ApiRequest::new("POST", "/api/v1/sync/now").with_cap_token(Some("s".into())))
                .status,
            404
        );
    }

    #[test]
    fn hydrations_endpoint_lists_in_flight_downloads() {
        struct MockHydrations(Vec<String>);
        impl HydrationStatus for MockHydrations {
            fn active(&self) -> Vec<String> {
                self.0.clone()
            }
        }
        // without a provider: empty + count 0 (read-only, no token)
        let bare = Router::new(Config::default());
        let r0 = bare.route(&ApiRequest::get("/api/v1/hydrations"));
        let s0 = String::from_utf8_lossy(&r0.body);
        assert!(s0.contains("\"count\":0"), "got {s0}");
        // with a provider: reports the active file names
        let router = Router::new(Config::default()).with_hydrations(std::sync::Arc::new(
            MockHydrations(vec!["a.pdf".into(), "b.docx".into()]),
        ));
        let r = router.route(&ApiRequest::get("/api/v1/hydrations"));
        let s = String::from_utf8_lossy(&r.body);
        assert!(s.contains("\"count\":2"), "got {s}");
        assert!(s.contains("a.pdf") && s.contains("b.docx"), "got {s}");
    }

    #[test]
    fn destructive_capability_tokens_are_action_scoped() {
        let (_d, router) = setup();
        let sync = std::sync::Arc::new(MockSync {
            paused: false.into(),
            triggered: false.into(),
        });
        let router = router
            .with_restore(std::sync::Arc::new(OkRestore), "restore-secret".into())
            .with_sync_control(sync.clone(), "sync-secret".into());
        let restore_q = "/api/v1/restore?account=a&service=mail&id=m1";

        // The sync token must not authorize a cloud restore.
        assert_eq!(
            router
                .route(
                    &ApiRequest::new("POST", restore_q).with_cap_token(Some("sync-secret".into()))
                )
                .status,
            401
        );
        let audit =
            body_json(&router.route(&ApiRequest::get("/api/v1/activity?account=a&limit=5")));
        assert_eq!(
            audit["count"], 0,
            "a rejected cross-token restore must not write an audit entry"
        );

        // The restore token must not authorize scheduler controls.
        assert_eq!(
            router
                .route(
                    &ApiRequest::new("POST", "/api/v1/sync/pause")
                        .with_cap_token(Some("restore-secret".into()))
                )
                .status,
            401
        );
        assert!(!sync.is_paused());

        // Each action still succeeds with its own token.
        assert_eq!(
            router
                .route(
                    &ApiRequest::new("POST", restore_q)
                        .with_cap_token(Some("restore-secret".into()))
                )
                .status,
            200
        );
        assert_eq!(
            router
                .route(
                    &ApiRequest::new("POST", "/api/v1/sync/pause")
                        .with_cap_token(Some("sync-secret".into()))
                )
                .status,
            200
        );
        assert!(sync.is_paused());
    }

    #[test]
    fn restore_post_refused_when_not_enabled() {
        // a read-only router (no handler) refuses the POST but still serves GETs
        let router = Router::new(Config::default());
        let req = ApiRequest::new("POST", "/api/v1/restore?account=a&service=mail&id=x")
            .with_cap_token(Some("x".into()));
        assert_eq!(router.route(&req).status, 404);
        assert_eq!(
            router.route(&ApiRequest::get("/api/v1/accounts")).status,
            200
        );
    }

    #[test]
    fn app_js_lists_browsable_services() {
        // every backed-up service is a browsable view. 'shared' (inbound
        // shared-with-me) is intentionally omitted — that capability is deprecated
        // by Microsoft and was closed not-planned (#332), so it never holds data.
        for svc in [
            "onedrive", "mail", "calendar", "contacts", "todo", "onenote",
        ] {
            assert!(
                APP_JS.contains(&format!("\"{svc}\"")),
                "web UI is missing the '{svc}' service view"
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
    fn app_js_wires_pagination() {
        for needle in ["loadMore", "offset", "limit"] {
            assert!(
                APP_JS.contains(needle),
                "web UI is missing '{needle}' (pagination wiring)"
            );
        }
    }

    #[test]
    fn app_js_wires_overview_dashboard() {
        // the UI must call the overview endpoints + expose the panels, so the
        // front-end wiring can't silently regress.
        for needle in [
            "/api/v1/status",
            "/api/v1/settings",
            "/api/v1/activity",
            "Overview",
            "Recent runs",
        ] {
            assert!(
                APP_JS.contains(needle),
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
                mount_point: None,
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
                mount_point: None,
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
    fn items_mail_carries_preview_from_archived_eml() {
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("arch");
        std::fs::create_dir_all(arch.join("mail/aa/bb")).unwrap();
        // a realistic multipart/alternative message (plain + html)
        let eml = b"From: Ada Lovelace <ada@example.com>\r\n\
To: Bob Builder <bob@example.com>\r\n\
Subject: Quarterly report\r\n\
Date: Mon, 02 Jun 2025 10:00:00 +0000\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/alternative; boundary=\"BOUND\"\r\n\
\r\n\
--BOUND\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Numbers look great this quarter.\r\n\
--BOUND\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<html><body><p>Numbers look great this quarter.</p></body></html>\r\n\
--BOUND--\r\n";
        std::fs::write(arch.join("mail/aa/bb/m.eml"), eml).unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut m = Item::new("a", "mail", "m1", "Quarterly report", "message");
            m.local_path = Some("mail/aa/bb/m.eml".into());
            store.upsert_item(&m).unwrap();
            // an item whose body file is absent → still listed, just without a preview
            let mut m2 = Item::new("a", "mail", "m2", "No body", "message");
            m2.local_path = Some("mail/zz/zz/missing.eml".into());
            store.upsert_item(&m2).unwrap();
        }
        let cfg = Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@outlook.com".into(),
                sync_root: dir.path().join("od"),
                archive_root: arch,
                mount_point: None,
            }],
            ..Default::default()
        };
        let router = Router::new(cfg);
        let v = body_json(&router.route(&ApiRequest::get("/api/v1/items?account=a&service=mail")));
        let items = v["items"].as_array().unwrap();
        let m1 = items.iter().find(|i| i["remote_id"] == "m1").unwrap();
        let p = &m1["preview"];
        assert_eq!(p["from"], "Ada Lovelace <ada@example.com>");
        assert_eq!(p["subject"], "Quarterly report");
        assert_eq!(p["has_html"], true);
        assert_eq!(p["to"][0], "Bob Builder <bob@example.com>");
        assert_eq!(p["date"], "Mon, 02 Jun 2025 10:00:00 +0000");
        assert!(p["snippet"]
            .as_str()
            .unwrap()
            .contains("Numbers look great this quarter"));
        // the item with a missing body file is still listed but carries no preview
        let m2 = items.iter().find(|i| i["remote_id"] == "m2").unwrap();
        assert!(m2.get("preview").is_none());
    }

    #[test]
    fn items_calendar_carries_preview_from_archived_json() {
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("arch");
        std::fs::create_dir_all(arch.join("calendar/aa/bb")).unwrap();
        std::fs::write(
            arch.join("calendar/aa/bb/ev.json"),
            br#"{"subject":"Team-Standup",
                 "start":{"dateTime":"2026-02-04T09:00:00.0000000","timeZone":"UTC"},
                 "end":{"dateTime":"2026-02-04T10:00:00.0000000","timeZone":"UTC"},
                 "isAllDay":false,"location":{"displayName":"Room 1"}}"#,
        )
        .unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut e = Item::new("a", "calendar", "e1", "Team-Standup", "event");
            e.local_path = Some("calendar/aa/bb/ev.json".into());
            store.upsert_item(&e).unwrap();
        }
        let cfg = Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@outlook.com".into(),
                sync_root: dir.path().join("od"),
                archive_root: arch,
                mount_point: None,
            }],
            ..Default::default()
        };
        let router = Router::new(cfg);
        let v =
            body_json(&router.route(&ApiRequest::get("/api/v1/items?account=a&service=calendar")));
        let p = &v["items"][0]["preview"];
        assert_eq!(p["start"], "2026-02-04T09:00:00.0000000");
        assert_eq!(p["start_tz"], "UTC");
        assert_eq!(p["end"], "2026-02-04T10:00:00.0000000");
        assert_eq!(p["all_day"], false);
        assert_eq!(p["location"], "Room 1");
    }

    #[test]
    fn items_contacts_and_todo_carry_preview_from_archived_json() {
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("arch");
        std::fs::create_dir_all(arch.join("contacts/aa")).unwrap();
        std::fs::create_dir_all(arch.join("todo/bb")).unwrap();
        std::fs::write(
            arch.join("contacts/aa/c.json"),
            br#"{"displayName":"Ada Lovelace","companyName":"Analytical Engines",
                 "jobTitle":"Mathematician",
                 "emailAddresses":[{"address":"ada@example.com","name":"Ada"}]}"#,
        )
        .unwrap();
        std::fs::write(
            arch.join("todo/bb/t.json"),
            br#"{"title":"Ship release","status":"inProgress","importance":"high",
                 "dueDateTime":{"dateTime":"2026-03-01T00:00:00.0000000","timeZone":"UTC"},
                 "body":{"content":"check the gate","contentType":"text"}}"#,
        )
        .unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut c = Item::new("a", "contacts", "c1", "Ada Lovelace", "contact");
            c.local_path = Some("contacts/aa/c.json".into());
            store.upsert_item(&c).unwrap();
            let mut t = Item::new("a", "todo", "t1", "Ship release", "task");
            t.local_path = Some("todo/bb/t.json".into());
            store.upsert_item(&t).unwrap();
        }
        let cfg = Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@outlook.com".into(),
                sync_root: dir.path().join("od"),
                archive_root: arch,
                mount_point: None,
            }],
            ..Default::default()
        };
        let router = Router::new(cfg);
        let c =
            body_json(&router.route(&ApiRequest::get("/api/v1/items?account=a&service=contacts")));
        let cp = &c["items"][0]["preview"];
        assert_eq!(cp["company"], "Analytical Engines");
        assert_eq!(cp["job"], "Mathematician");
        assert_eq!(cp["email"], "ada@example.com");
        let t = body_json(&router.route(&ApiRequest::get("/api/v1/items?account=a&service=todo")));
        let tp = &t["items"][0]["preview"];
        assert_eq!(tp["status"], "inProgress");
        assert_eq!(tp["importance"], "high");
        assert_eq!(tp["due"], "2026-03-01T00:00:00.0000000");
        assert_eq!(tp["has_note"], true);
    }

    #[test]
    fn items_parent_navigates_onedrive_folders() {
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("arch");
        std::fs::create_dir_all(&arch).unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            // "DR" is the untracked drive root; Folder + a.txt hang off it.
            let mut folder = Item::new("a", "onedrive", "F1", "Folder One", "folder");
            folder.parent_remote_id = Some("DR".into());
            let mut top = Item::new("a", "onedrive", "top", "a.txt", "file");
            top.parent_remote_id = Some("DR".into());
            let mut nested = Item::new("a", "onedrive", "n1", "nested.txt", "file");
            nested.parent_remote_id = Some("F1".into());
            store.upsert_item(&folder).unwrap();
            store.upsert_item(&top).unwrap();
            store.upsert_item(&nested).unwrap();
        }
        let cfg = Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@outlook.com".into(),
                sync_root: dir.path().join("od"),
                archive_root: arch,
                mount_point: None,
            }],
            ..Default::default()
        };
        let router = Router::new(cfg);
        // root view → the two items under the untracked drive root
        let root = body_json(&router.route(&ApiRequest::get(
            "/api/v1/items?account=a&service=onedrive&parent=root",
        )));
        let root_names: Vec<&str> = root["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["name"].as_str().unwrap())
            .collect();
        assert_eq!(root_names, ["Folder One", "a.txt"]);
        assert_eq!(root["parent"], "root");
        // descending into the folder shows only its child
        let inside = body_json(&router.route(&ApiRequest::get(
            "/api/v1/items?account=a&service=onedrive&parent=F1",
        )));
        assert_eq!(inside["count"], 1);
        assert_eq!(inside["items"][0]["name"], "nested.txt");
        // without `parent` the flat paginated listing is unchanged (all 3 items)
        let flat =
            body_json(&router.route(&ApiRequest::get("/api/v1/items?account=a&service=onedrive")));
        assert_eq!(flat["total"], 3);
        assert!(flat.get("parent").is_none());
    }

    #[test]
    fn safe_content_type_serves_raster_images_but_not_svg() {
        assert_eq!(safe_content_type("onedrive/aa/bb/photo.PNG"), "image/png");
        assert_eq!(safe_content_type("x.jpg"), "image/jpeg");
        assert_eq!(safe_content_type("x.jpeg"), "image/jpeg");
        assert_eq!(safe_content_type("x.gif"), "image/gif");
        assert_eq!(safe_content_type("x.webp"), "image/webp");
        // SVG can carry scripts → kept inert as text/plain
        assert_eq!(safe_content_type("x.svg"), "text/plain; charset=utf-8");
        assert!(safe_content_type("x.json").starts_with("application/json"));
        assert!(safe_content_type("x.eml").starts_with("text/plain"));
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
                mount_point: None,
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
                mount_point: None,
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
    fn body_serves_onedrive_file_from_sync_root() {
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("arch");
        let sync = dir.path().join("od");
        std::fs::create_dir_all(&arch).unwrap();
        std::fs::create_dir_all(sync.join("Pictures")).unwrap();
        // a OneDrive file lives under the *sync* root, not the archive root
        std::fs::write(sync.join("notes.txt"), b"hello onedrive").unwrap();
        std::fs::write(sync.join("Pictures/logo.png"), b"\x89PNG\r\n\x1a\nfake").unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut f = Item::new("a", "onedrive", "f1", "notes.txt", "file");
            f.local_path = Some("notes.txt".into());
            store.upsert_item(&f).unwrap();
            let mut img = Item::new("a", "onedrive", "f2", "logo.png", "file");
            img.local_path = Some("Pictures/logo.png".into());
            store.upsert_item(&img).unwrap();
        }
        let cfg = Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@outlook.com".into(),
                sync_root: sync,
                archive_root: arch,
                mount_point: None,
            }],
            ..Default::default()
        };
        let router = Router::new(cfg);
        // text file → served inertly from the sync root
        let t = router.route(&ApiRequest::get(
            "/api/v1/body?account=a&service=onedrive&id=f1",
        ));
        assert_eq!(t.status, 200);
        assert!(t.content_type.starts_with("text/plain"));
        assert_eq!(t.body, b"hello onedrive");
        // image → real raster content-type so the explorer can show a thumbnail
        let i = router.route(&ApiRequest::get(
            "/api/v1/body?account=a&service=onedrive&id=f2",
        ));
        assert_eq!(i.status, 200);
        assert_eq!(i.content_type, "image/png");
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
                mount_point: None,
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
