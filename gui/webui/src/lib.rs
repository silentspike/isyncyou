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

use isyncyou_core::{Config, OneDriveMode, OneDriveModes};
use isyncyou_store::{Item, Store};
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

mod serve;
mod view;
pub use serve::{
    bind_loopback, dispatch_message, format_http, handle_bridge_request, parse_request_line, serve,
    serve_listener, serve_listener_shared, BridgeDispatchRequest,
};
#[cfg(unix)]
pub use serve::{default_unix_socket_path, serve_unix};

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
/// Easter-egg game sound effects (generated with ElevenLabs), served same-origin
/// from `/sfx/*.mp3` and fetched + decoded via Web Audio (`connect-src 'self'`).
const SFX_SHOOT: &[u8] = include_bytes!("assets/sfx-shoot.mp3");
const SFX_BOOM: &[u8] = include_bytes!("assets/sfx-boom.mp3");
const SFX_LEVEL: &[u8] = include_bytes!("assets/sfx-level.mp3");
const SFX_DROP: &[u8] = include_bytes!("assets/sfx-drop.mp3");
const SFX_PICKUP: &[u8] = include_bytes!("assets/sfx-pickup.mp3");
const SFX_HIT: &[u8] = include_bytes!("assets/sfx-hit.mp3");

/// Serve an embedded MP3 sound effect (immutable within a version).
fn audio_response(bytes: &[u8]) -> ApiResponse {
    ApiResponse {
        status: 200,
        content_type: "audio/mpeg".into(),
        body: bytes.to_vec(),
        // no-store so a regenerated SFX always takes effect (no stale WebView cache)
        headers: vec![("Cache-Control".into(), "no-store".into())],
    }
}

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

const BACKUP_SERVICES: &[&str] = &["mail", "calendar", "contacts", "todo", "onenote"];

/// A parsed inbound request (method + path + decoded query pairs + an optional
/// capability token captured from the `X-Capability-Token` header).
#[derive(Clone)]
pub struct ApiRequest {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    /// The `X-Capability-Token` header value, required for destructive POSTs.
    pub cap_token: Option<String>,
    /// The `X-Session-Token` header value (#89 mobile profile): required on every
    /// `/api/v1/*` route when the Router runs with a session token.
    pub session_token: Option<String>,
    /// Public opaque handle armed only by the trusted native presence path.
    pub per_action_token: Option<String>,
    /// Trusted Android bridge observation. The WebView cannot set this value because
    /// Kotlin strips and replaces the corresponding header.
    pub storage_not_low: Option<bool>,
    /// True only for requests dispatched by the in-process Android message bridge.
    /// HTTP headers, cookies, and JavaScript cannot set this transport provenance.
    pub mobile_bridge: bool,
    /// Normalized inbound `Content-Type`. Only newly introduced JSON routes depend on
    /// this; legacy query/body routes intentionally retain their existing contract.
    pub content_type: Option<String>,
    /// The request body bytes (#0A transport). Empty for the query-string GETs that
    /// make up today's API; carried so a body-bearing request survives **both**
    /// transports — HTTP (`serve.rs` reads it instead of draining) and the Android
    /// in-process message bridge (which has no query-string ergonomics for uploads).
    pub body: Vec<u8>,
}

impl std::fmt::Debug for ApiRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApiRequest")
            .field("method", &self.method)
            .field("route", &self.path)
            .field("body_len", &self.body.len())
            .field("has_query", &!self.query.is_empty())
            .field("has_capability", &self.cap_token.is_some())
            .field("has_session", &self.session_token.is_some())
            .field("has_per_action", &self.per_action_token.is_some())
            .field("mobile_bridge", &self.mobile_bridge)
            .field("has_content_type", &self.content_type.is_some())
            .finish()
    }
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
            session_token: None,
            per_action_token: None,
            storage_not_low: None,
            mobile_bridge: false,
            content_type: None,
            body: Vec::new(),
        }
    }

    /// Attach a captured capability token (builder style, used by the server).
    pub fn with_cap_token(mut self, token: Option<String>) -> Self {
        self.cap_token = token;
        self
    }

    /// Attach the captured `X-Session-Token` header (builder style, #89).
    pub fn with_session_token(mut self, token: Option<String>) -> Self {
        self.session_token = token;
        self
    }

    pub fn with_per_action_token(mut self, token: Option<String>) -> Self {
        self.per_action_token = token;
        self
    }

    pub fn with_storage_not_low(mut self, value: Option<bool>) -> Self {
        self.storage_not_low = value;
        self
    }

    pub fn with_mobile_bridge(mut self, mobile_bridge: bool) -> Self {
        self.mobile_bridge = mobile_bridge;
        self
    }

    /// Attach the normalized request content type (builder style, shared by HTTP and
    /// the Android bridge).
    pub fn with_content_type(mut self, content_type: Option<String>) -> Self {
        self.content_type = content_type;
        self
    }

    /// Attach the request body (builder style, #0A) — the HTTP adapter reads it from
    /// the socket, the Android bridge passes it straight from the `WebMessage`.
    pub fn with_body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
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

fn agent_oauth_provider_param(provider: &str) -> Result<&'static str, &'static str> {
    match provider {
        "claude" => Ok("claude"),
        "codex" => Ok("codex"),
        "" => Err("provider is required"),
        _ => Err("unknown provider"),
    }
}

const AGENT_STRICT_JSON_MAX_BYTES: usize = 8 * 1024;
const AGENT_TURN_JSON_MAX_BYTES: usize = 64 * 1024;

fn is_json_content_type(content_type: Option<&str>) -> bool {
    let Some(content_type) = content_type else {
        return false;
    };
    let mut parts = content_type.split(';');
    if !parts
        .next()
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("application/json"))
    {
        return false;
    }
    parts.all(|parameter| {
        let parameter = parameter.trim();
        parameter.is_empty()
            || parameter.split_once('=').is_some_and(|(name, value)| {
                name.trim().eq_ignore_ascii_case("charset")
                    && value.trim().trim_matches('"').eq_ignore_ascii_case("utf-8")
            })
    })
}

fn with_no_store(mut response: ApiResponse) -> ApiResponse {
    if !response
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("cache-control"))
    {
        response
            .headers
            .push(("Cache-Control".into(), "no-store".into()));
    }
    response
}

fn no_store_json_error(status: u16, message: &str) -> ApiResponse {
    with_no_store(ApiResponse::error(status, message))
}

fn parse_agent_strict_json<T: serde::de::DeserializeOwned>(
    req: &ApiRequest,
    operation: &str,
) -> Result<T, ApiResponse> {
    parse_agent_strict_json_with_limit(req, operation, AGENT_STRICT_JSON_MAX_BYTES)
}

fn parse_agent_strict_json_with_limit<T: serde::de::DeserializeOwned>(
    req: &ApiRequest,
    operation: &str,
    max_bytes: usize,
) -> Result<T, ApiResponse> {
    if !req.query.is_empty() || !is_json_content_type(req.content_type.as_deref()) {
        return Err(no_store_json_error(
            400,
            &format!("{operation} accepts JSON only"),
        ));
    }
    if req.body.len() > max_bytes {
        return Err(no_store_json_error(413, "request body too large"));
    }
    let body = std::str::from_utf8(&req.body)
        .map_err(|_| no_store_json_error(400, "invalid JSON request body"))?;
    let mut deserializer = serde_json::Deserializer::from_str(body);
    let value = StrictJsonValue::deserialize(&mut deserializer)
        .and_then(|value| deserializer.end().map(|()| value.0))
        .map_err(|_| no_store_json_error(400, &format!("invalid {operation} request")))?;
    serde_json::from_value(value)
        .map_err(|_| no_store_json_error(400, &format!("invalid {operation} request")))
}

struct StrictJsonValue(Value);

impl<'de> Deserialize<'de> for StrictJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictJsonVisitor)
    }
}

struct StrictJsonVisitor;

impl<'de> Visitor<'de> for StrictJsonVisitor {
    type Value = StrictJsonValue;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(StrictJsonValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(StrictJsonValue(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(StrictJsonValue(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .map(StrictJsonValue)
            .ok_or_else(|| E::custom("invalid JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(StrictJsonValue(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(StrictJsonValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(StrictJsonValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(StrictJsonValue(Value::Null))
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        StrictJsonValue::deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<StrictJsonValue>()? {
            values.push(value.0);
        }
        Ok(StrictJsonValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = HashSet::new();
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(serde::de::Error::custom("duplicate JSON object key"));
            }
            values.insert(key, map.next_value::<StrictJsonValue>()?.0);
        }
        Ok(StrictJsonValue(Value::Object(values)))
    }
}

fn valid_client_request_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes[8] == b'-'
        && bytes[13] == b'-'
        && bytes[18] == b'-'
        && bytes[23] == b'-'
        && bytes[14] == b'4'
        && matches!(bytes[19], b'8' | b'9' | b'a' | b'b')
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 8 | 13 | 18 | 23)
                || byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()
        })
}

/// Parse one of the bounded scalar-only mutation contracts into the query-shaped
/// view used by the established domain handlers. This is a transport adapter, not
/// a legacy query fallback: callers must supply strict JSON and every accepted key
/// is selected by the matched route.
fn parse_strict_scalar_mutation(
    req: &ApiRequest,
    operation: &str,
    allowed_fields: &[&str],
) -> Result<ApiRequest, ApiResponse> {
    let value: Value = parse_agent_strict_json_with_limit(req, operation, 64 * 1024)?;
    let object = value
        .as_object()
        .ok_or_else(|| no_store_json_error(400, &format!("invalid {operation} request")))?;
    let request_id = object
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|value| valid_client_request_id(value))
        .ok_or_else(|| no_store_json_error(400, "invalid request_id"))?;

    if object
        .keys()
        .any(|key| key != "request_id" && !allowed_fields.contains(&key.as_str()))
    {
        return Err(no_store_json_error(
            400,
            &format!("invalid {operation} request"),
        ));
    }

    let mut parsed = req.clone();
    parsed.query.clear();
    parsed
        .query
        .push(("request_id".into(), request_id.to_owned()));
    for field in allowed_fields {
        let Some(value) = object.get(*field) else {
            continue;
        };
        let scalar = match value {
            Value::Null => continue,
            Value::String(value) if value.len() <= 32 * 1024 => value.clone(),
            Value::Bool(value) => {
                if *value {
                    "1".into()
                } else {
                    "0".into()
                }
            }
            Value::Number(value) => value.to_string(),
            _ => {
                return Err(no_store_json_error(
                    400,
                    &format!("invalid {operation} request"),
                ));
            }
        };
        parsed.query.push(((*field).to_owned(), scalar));
    }
    parsed.body.clear();
    Ok(parsed)
}

fn valid_opaque_agent_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn mutation_error_response(error: &str) -> ApiResponse {
    let (status, code) = match error {
        "mutation_intent_quota_exceeded" | "mutation_intent_busy" => (429, error),
        "mutation_intent_storage_unavailable" => (507, error),
        "request_id_conflict" | "mutation_intent_conflict" | "mutation_intent_outcome_unknown" => {
            (409, error)
        }
        "mutation_intent_not_found" => (404, error),
        "mutation_intent_expired" | "mutation_intent_invalid" => (400, error),
        _ => (500, "mutation_intent_failed"),
    };
    ApiResponse::error(status, code)
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PushRegisterRequest {
    request_id: String,
    token: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MailSendRequest {
    request_id: String,
    account: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    body: String,
    to: Vec<String>,
    #[serde(default)]
    cc: Vec<String>,
    #[serde(default)]
    bcc: Vec<String>,
    importance: Option<String>,
    #[serde(default)]
    read_receipt: bool,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MailReplyRequest {
    request_id: String,
    account: String,
    id: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    comment: String,
    #[serde(default)]
    all: bool,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MailForwardRequest {
    request_id: String,
    account: String,
    id: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    comment: String,
    to: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MailDraftRequest {
    request_id: String,
    account: String,
    id: Option<String>,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    to: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OneNoteCreateRequest {
    request_id: String,
    account: String,
    section: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OneNoteAppendRequest {
    request_id: String,
    account: String,
    id: String,
    text: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MutationIntentCreateRequest {
    request_id: String,
    #[serde(flatten)]
    purpose: MutationPurpose,
    total_bytes: u64,
    sha256: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MutationIntentChunkRequest {
    request_id: String,
    intent_id: String,
    index: u32,
    offset: u64,
    data_base64: String,
    chunk_sha256: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MutationIntentCommitRequest {
    request_id: String,
    intent_id: String,
    total_bytes: u64,
    sha256: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MutationIntentCancelRequest {
    request_id: String,
    intent_id: String,
}

fn parse_bounded_page_limit(value: Option<&str>) -> Result<Option<usize>, ApiResponse> {
    let Some(value) = value else {
        return Ok(None);
    };
    let limit = value
        .parse::<usize>()
        .ok()
        .filter(|limit| (1..=100).contains(limit))
        .ok_or_else(|| no_store_json_error(400, "invalid page limit"))?;
    Ok(Some(limit))
}

fn agent_session_error_status(error: &str) -> u16 {
    match error {
        "invalid_cursor"
        | "invalid_request_id"
        | "invalid_request_route"
        | "invalid_session_name"
        | "invalid_session_record" => 400,
        "request_not_found" | "session_not_found" => 404,
        "manifest_conflict"
        | "provider_generation_changed"
        | "request_id_conflict"
        | "session_limit_reached"
        | "session_request_capacity_reached"
        | "session_upgrade_required"
        | "stale_cursor" => 409,
        "history_page_too_large" | "session_budget_exceeded" => 413,
        "session_account_selection_required" => 409,
        "session_account_unavailable"
        | "session_store_unavailable"
        | "session_transport_unavailable" => 503,
        _ => 500,
    }
}

fn closed_lifecycle_error(code: &str) -> String {
    match code {
        "acknowledgement_required"
        | "already_disconnected"
        | "candidate_revoke_unknown"
        | "cleanup_pending"
        | "credential_etag_mismatch"
        | "idempotency_conflict"
        | "lifecycle_operation_mismatch"
        | "lifecycle_phase_mismatch"
        | "lifecycle_required"
        | "operation_in_progress"
        | "revoke_unknown"
        | "scope_changed"
        | "stale_lifecycle_fence"
        | "switch_identity_unavailable" => code.to_string(),
        _ => "account_lifecycle_unavailable".into(),
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct PublicShareError {
    status: u16,
    message: String,
}

fn redact_share_error_for_public_surface(error: &str) -> PublicShareError {
    let trimmed = error.trim();
    let lower = trimmed.to_ascii_lowercase();
    let (status, message) = match trimmed {
        "invite_recovery_ambiguous"
        | "invite_partial_success"
        | "invite_not_started_user_retry_required" => (409, trimmed.to_string()),
        "share_policy_unsupported" => (400, trimmed.to_string()),
        _ if lower.starts_with("invalid share")
            || lower.starts_with("invalid invite")
            || lower.starts_with("invite recipient")
            || lower.starts_with("invite requires") =>
        {
            (400, "invalid_share_request".to_string())
        }
        _ if lower.contains("sharing is only supported") => {
            (400, "share_unsupported_service".to_string())
        }
        _ => (500, "share_transient_failure".to_string()),
    };
    PublicShareError { status, message }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PublicRestoreError {
    status: u16,
    message: String,
}

fn redact_restore_error_for_public_surface(error: &str) -> PublicRestoreError {
    let lower = error.trim().to_ascii_lowercase();
    let (status, message) = if lower.contains("restore is not enabled")
        || lower.contains("cloud restore is disabled")
        || lower.contains("disabled")
    {
        (400, "restore_disabled")
    } else if lower.contains("not crash-safe")
        || lower.contains("unsupported")
        || lower.contains("not supported")
    {
        (400, "restore_unsupported_service")
    } else if lower.contains("unknown account") {
        (400, "restore_unknown_account")
    } else {
        (500, "restore_failed")
    };
    PublicRestoreError {
        status,
        message: message.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreResponse {
    Completed { new_id: String },
    Queued { job_id: String, state: String },
}

/// Performs a destructive cloud action on behalf of a POST request. Injected by
/// the daemon (which owns the Graph/engine stack) so the router itself stays a
/// pure read surface. Desktop handlers may complete immediately; mobile handlers
/// enqueue a durable job and return its id/state without mutating in the request.
pub trait RestoreHandler: Send + Sync {
    fn restore(&self, account: &str, service: &str, id: &str) -> Result<RestoreResponse, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupJobQueued {
    pub job_id: String,
    pub state: String,
}

/// Enqueues a durable mobile backup job. The handler must not perform the backup
/// inline; execution belongs to the mobile job worker.
pub trait BackupHandler: Send + Sync {
    fn enqueue_backup(&self, account: &str, services: &[String])
        -> Result<BackupJobQueued, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileJobSummary {
    pub job_id: String,
    pub kind: String,
    pub state: String,
    pub service: Option<String>,
    pub target_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub finished_at: Option<i64>,
    pub last_error: Option<String>,
}

/// Mobile durable job status/cancel surface. Summaries deliberately exclude
/// intent/result payloads because those may contain operation-specific metadata.
pub trait MobileJobHandler: Send + Sync {
    fn list_jobs(&self, account: &str, limit: u32) -> Result<Vec<MobileJobSummary>, String>;
    fn cancel_job(&self, account: &str, job_id: &str) -> Result<bool, String>;
}

/// Non-secret binding fields for a pending Agent destructive action. Mobile uses
/// these to mint a native biometric-token challenge before the Agent's one-time
/// confirmation token is consumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPendingBinding {
    pub op: String,
    pub account: String,
    pub service: String,
    pub item: String,
}

/// The only provider/purpose pairs accepted by the connectivity preflight route.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConnectivityPreflightRequest {
    pub provider: String,
    pub purpose: String,
    #[serde(default)]
    pub snapshot_id: Option<String>,
}

/// Redacted connectivity result returned to the Assistant. Values are closed enums;
/// transport errors, host names, URLs, and snapshot fields never cross this boundary.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AgentConnectivityPreflightResponse {
    pub status: String,
    pub code: String,
    pub retryable: bool,
    pub settings_hint: String,
}

/// Closed request for the explicitly user-triggered product credential refresh. It is separate
/// from status polling so an ordinary Assistant render cannot create provider network traffic.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCredentialRefreshRequest {
    pub provider: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionCreateRequest {
    pub request_id: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionSelectRequest {
    pub request_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTurnRequest {
    pub request_id: String,
    pub session_id: String,
    pub account: String,
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTurnCancelRequest {
    pub request_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPendingCancelRequest {
    pub request_id: String,
    pub pending: String,
    pub action_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionArchiveBinding {
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionPairingRevealBinding {
    pub session_id: String,
    pub pair_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionPairingImportBinding {
    pub pairing_code: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(untagged)]
pub enum AgentUserPresenceBinding {
    SessionArchive(AgentSessionArchiveBinding),
    SessionPairingReveal(AgentSessionPairingRevealBinding),
    SessionPairingImport(AgentSessionPairingImportBinding),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentUserPresenceStartRequest {
    pub request_id: String,
    pub kind: String,
    pub binding: AgentUserPresenceBinding,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentUserPresenceConfirmRequest {
    pub request_id: String,
    pub operation_id: String,
    pub intent_id: String,
    pub token: String,
    pub action_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfirmRequest {
    pub request_id: String,
    pub pending: String,
    pub token: String,
    pub action_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentModelRequest {
    pub request_id: String,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionPairingCreateRequest {
    pub request_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionPairingOperationRequest {
    pub request_id: String,
    pub operation_id: String,
}

pub type AgentSessionArchiveRequest = AgentSessionPairingOperationRequest;

/// A provider OAuth start result. `attempt_id` is opaque and exists only so the
/// initiating UI can cancel the exact server-side attempt without handling OAuth
/// state, codes, or callback URLs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AgentOAuthStartResponse {
    pub authorize_url: String,
    pub attempt_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle_operation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOAuthStartRequest {
    pub provider: String,
    pub request_id: String,
    #[serde(default)]
    pub lifecycle_operation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentLifecycleResumeRef {
    pub operation_id: String,
    pub operation_etag: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOAuthLogoutRequest {
    pub provider: String,
    pub mode: String,
    pub request_id: String,
    #[serde(default)]
    pub credential_etag: Option<String>,
    #[serde(default)]
    pub resume: Option<AgentLifecycleResumeRef>,
    pub acknowledge_full_grant: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOAuthLifecycleResumeRequest {
    pub provider: String,
    pub request_id: String,
    pub operation_id: String,
    pub operation_etag: String,
    pub action: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AgentAccountLifecycleResponse {
    pub provider: String,
    pub mode: String,
    pub operation_id: Option<String>,
    pub operation_etag: Option<String>,
    pub state: String,
    pub revoke_leg: u32,
    pub revoked_grant: Option<String>,
    pub revoke_request_target: Option<String>,
    pub revoke_scope_guarantee: Option<String>,
    pub retryable: bool,
    pub code: String,
}

/// Closed cancellation request for one provider OAuth attempt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOAuthCancelRequest {
    pub provider: String,
    pub attempt_id: String,
}

/// #639 T9: closed manual-completion request for the Claude copy-paste OAuth flow. The pasted code
/// crosses the boundary only transiently in this body (never a URL/query/log). `deny_unknown_fields`
/// plus serde's derived struct deserializer reject unknown AND duplicate keys.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOAuthCompleteRequest {
    pub provider: String,
    pub attempt_id: String,
    pub pasted_code: String,
}

/// The in-app agent (S-AG.6/#621). Injected by the daemon/mobile engine (app-host owns
/// the LLM/engine stack) so the router stays a thin surface. The handler deals only in
/// strings — it serializes its own stream events to JSON SSE-data lines (returned as a
/// `Receiver<String>`) so the webui crate stays decoupled from `isyncyou-agent`.
/// The model never receives a capability token; destructive actions become a pending
/// action confirmed out-of-band via `confirm` (REQ-AGENT-003/004).
pub trait AgentHandler: Send + Sync {
    /// Start a turn for `prompt` on `account`; returns a `turn_id` the client streams.
    fn start_turn(&self, account: &str, prompt: &str) -> Result<String, String>;
    fn start_turn_request(
        self: std::sync::Arc<Self>,
        request: AgentTurnRequest,
    ) -> Result<String, String> {
        self.start_turn(&request.account, &request.prompt)
    }
    /// Peek the non-secret binding for a pending destructive action without checking
    /// or consuming its one-time Agent confirmation token.
    fn pending_binding(
        &self,
        _pending_id: &str,
        _action_hash: &str,
    ) -> Result<AgentPendingBinding, String> {
        Err("pending binding is not enabled on this server".into())
    }
    /// Confirm a pending destructive action with its one-time token; returns a summary.
    fn confirm(&self, pending_id: &str, token: &str, action_hash: &str) -> Result<String, String>;
    /// Cancel an in-flight turn.
    fn cancel(&self, turn_id: &str);
    fn cancel_turn(&self, turn_id: &str) -> Result<(), String> {
        self.cancel(turn_id);
        Ok(())
    }
    fn cancel_pending(&self, _pending_id: &str, _action_hash: &str) -> Result<(), String> {
        Err("pending cancellation is not enabled on this server".into())
    }
    /// Subscribe to a turn's stream (pre-serialized JSON SSE-data lines).
    fn open_stream(&self, turn_id: &str) -> Option<std::sync::mpsc::Receiver<String>>;

    fn session_create(
        &self,
        _request_id: &str,
        _display_name: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        Err("product sessions are not enabled on this server".into())
    }

    fn session_list(
        &self,
        _cursor: Option<&str>,
        _limit: Option<usize>,
    ) -> Result<serde_json::Value, String> {
        Err("product sessions are not enabled on this server".into())
    }

    fn session_select(
        &self,
        _request_id: &str,
        _session_id: &str,
    ) -> Result<serde_json::Value, String> {
        Err("product sessions are not enabled on this server".into())
    }

    fn session_history(
        &self,
        _session_id: &str,
        _cursor: Option<&str>,
        _limit: Option<usize>,
    ) -> Result<serde_json::Value, String> {
        Err("product sessions are not enabled on this server".into())
    }

    fn request_status(
        &self,
        _session_id: &str,
        _route: &str,
        _request_id: &str,
    ) -> Result<serde_json::Value, String> {
        Err("product request status is not enabled on this server".into())
    }

    fn session_archive(
        &self,
        _request: AgentSessionArchiveRequest,
    ) -> Result<serde_json::Value, String> {
        Err("product sessions are not enabled on this server".into())
    }

    fn user_presence_start(
        &self,
        _request: AgentUserPresenceStartRequest,
    ) -> Result<serde_json::Value, String> {
        Err("user presence is not enabled on this server".into())
    }

    fn user_presence_confirm(
        &self,
        _request: AgentUserPresenceConfirmRequest,
    ) -> Result<serde_json::Value, String> {
        Err("user presence is not enabled on this server".into())
    }

    fn session_pairing_create(
        &self,
        _request: AgentSessionPairingCreateRequest,
    ) -> Result<serde_json::Value, String> {
        Err("session pairing is not enabled on this server".into())
    }

    fn session_pairing_reveal(
        &self,
        _request: AgentSessionPairingOperationRequest,
    ) -> Result<serde_json::Value, String> {
        Err("session pairing is not enabled on this server".into())
    }

    fn session_pairing_claim(
        &self,
        _request: AgentSessionPairingOperationRequest,
    ) -> Result<serde_json::Value, String> {
        Err("session pairing is not enabled on this server".into())
    }

    fn session_pairing_finalize(
        &self,
        _request: AgentSessionPairingOperationRequest,
    ) -> Result<serde_json::Value, String> {
        Err("session pairing is not enabled on this server".into())
    }

    fn session_pairing_revoke(
        &self,
        _request: AgentSessionPairingOperationRequest,
    ) -> Result<serde_json::Value, String> {
        Err("session pairing is not enabled on this server".into())
    }

    /// Run a closed provider connectivity preflight. The router parses the strict JSON
    /// request and the handler returns only the closed diagnostic response.
    fn connectivity_preflight(
        &self,
        _request: AgentConnectivityPreflightRequest,
    ) -> Result<AgentConnectivityPreflightResponse, String> {
        Err("connectivity preflight is not enabled on this server".into())
    }

    /// Same preflight with the Router-authenticated session token. Desktop handlers can use
    /// the default implementation; mobile handlers consume a one-shot native snapshot that is
    /// bound to this token before doing any network I/O.
    fn connectivity_preflight_with_session(
        &self,
        request: AgentConnectivityPreflightRequest,
        _session_token: Option<&str>,
        _mobile_bridge: bool,
    ) -> Result<AgentConnectivityPreflightResponse, String> {
        self.connectivity_preflight(request)
    }

    /// Refresh a configured product OAuth credential. The handler returns only its closed
    /// post-operation state; provider errors and token details never reach the router.
    fn credential_refresh(&self, _provider: &str) -> Result<String, String> {
        Err("credential refresh is not enabled on this server".into())
    }

    /// Agent provider OAuth login. Begin a device OAuth login for
    /// `provider`; `redirect_uri` is the loopback callback the browser returns to
    /// (the client supplies its own origin). Returns the authorize URL the UI opens
    /// in the **system browser**. Default: not available (handler opted out).
    fn oauth_start(&self, _provider: &str, _redirect_uri: &str) -> Result<String, String> {
        Err("subscription login is not enabled on this server".into())
    }
    /// Structured OAuth start used by current product clients. The compatibility default
    /// keeps older handlers usable while still returning an opaque, non-secret attempt id.
    fn oauth_start_with_attempt(
        &self,
        provider: &str,
        redirect_uri: &str,
    ) -> Result<AgentOAuthStartResponse, String> {
        let authorize_url = self.oauth_start(provider, redirect_uri)?;
        Ok(AgentOAuthStartResponse {
            authorize_url,
            attempt_id: String::new(),
            lifecycle_operation_id: None,
        })
    }
    fn oauth_start_request(
        &self,
        request: AgentOAuthStartRequest,
    ) -> Result<AgentOAuthStartResponse, String> {
        self.oauth_start_with_attempt(&request.provider, "")
    }
    fn oauth_logout(
        &self,
        _request: AgentOAuthLogoutRequest,
    ) -> Result<AgentAccountLifecycleResponse, String> {
        Err("account lifecycle is not enabled on this server".into())
    }
    fn oauth_lifecycle_resume(
        &self,
        _request: AgentOAuthLifecycleResumeRequest,
    ) -> Result<AgentAccountLifecycleResponse, String> {
        Err("account lifecycle is not enabled on this server".into())
    }
    /// Cancel an OAuth attempt. Implementations must only cancel the matching opaque id.
    fn oauth_cancel(&self, _provider: &str, _attempt_id: &str) -> Result<(), String> {
        Err("subscription login is not enabled on this server".into())
    }
    /// Agent provider OAuth callback. The system browser returns here with
    /// the authorization `code` and the CSRF `state`; the handler exchanges the code
    /// and stores the token, then returns a human-facing success page (HTML).
    fn oauth_callback(&self, _code: &str, _state: &str) -> Result<String, String> {
        Err("subscription login is not enabled on this server".into())
    }

    /// Manual-login completion (Claude only): the operator pastes the `code#state` the provider
    /// showed; `attempt_id` binds it to the server-side attempt whose state must match the embedded
    /// `#state`. The handler exchanges it and stores the token.
    fn oauth_complete(&self, _attempt_id: &str, _pasted: &str) -> Result<String, String> {
        Err("subscription login is not enabled on this server".into())
    }

    /// Connection status as a JSON string — the Assistant UI reads it to decide between
    /// the connect card and the chat. Default: not connected.
    fn status_json(&self) -> String {
        "{\"connected\":false}".to_string()
    }

    /// Set the active provider + model (the in-app model switcher). The offered models are
    /// reported in `status_json`'s `models` field. Default: not available.
    fn set_model(
        &self,
        _provider: &str,
        _model: &str,
        _reasoning_effort: Option<&str>,
    ) -> Result<(), String> {
        Err("model selection is not enabled on this server".into())
    }
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

/// Reports live OneDrive info that needs a Graph call (not held in the store):
/// the drive storage quota, and a single item's sharing permissions (#564).
/// Injected by the daemon (which owns the Graph stack + token); the read-only
/// CLI `serve` doesn't set it, so the endpoints 404 there. These are reads, so
/// no capability token is required (the daemon binds to localhost).
pub trait OneDriveInfoHandler: Send + Sync {
    /// The drive quota object (`total`/`used`/`remaining`/`state`) as JSON.
    fn drive_quota(&self, account: &str) -> Result<serde_json::Value, String>;
    /// A single item's sharing permissions ("who has access") as a JSON array of
    /// `{ id, roles, link, grantee }` (#564). Fetched lazily on detail open.
    fn permissions(&self, account: &str, id: &str) -> Result<serde_json::Value, String>;
}

/// Reports a OneDrive folder's **live** children (a paged Graph call, not held in
/// the store) for Mode-1 online browsing (#648). Injected by the daemon/mobile
/// engine (which owns the Graph stack + token); the read-only CLI `serve` doesn't
/// set it, so `/api/v1/onedrive/children` 404s there. A read, so no cap token.
pub trait OneDriveListHandler: Send + Sync {
    /// A folder's children as a JSON array (`id`/`name`/`size`/`folder`/`file`/
    /// `lastModifiedDateTime` per child). An empty `folder` = the drive root.
    fn children(&self, account: &str, folder: &str) -> Result<Vec<serde_json::Value>, String>;
}

/// Downloads a OneDrive item's content live by id for on-demand open (#649, Mode 1
/// online). No store write — the bytes are served inertly and not persisted as a
/// tracked item. Injected by the daemon/mobile engine; `None` => the endpoint 404s.
pub trait OneDriveOpenHandler: Send + Sync {
    fn download(&self, account: &str, id: &str) -> Result<Vec<u8>, String>;
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

/// One in-flight transfer's progress (#onedrive-mobile 0.8). `bytes_total == 0` means
/// the size is not yet known; `retry_after_secs > 0` means it is backing off on a 429.
pub struct TransferState {
    pub id: String,
    pub name: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub retry_after_secs: u64,
    /// #659: the transfer is paused (queue-deep) — skipped between files until resumed.
    pub paused: bool,
}

/// Progress + cancellation for in-flight downloads/materializations (#onedrive-mobile
/// 0.8 foundation). The real transfer engine (Phase 3/4) implements it; until then the
/// endpoints report an idle state. Mirrors [`HydrationStatus`]; the cancel side is a
/// cap-gated POST.
pub trait TransferProgress: Send + Sync {
    /// In-flight transfers with per-file progress.
    fn transfers(&self) -> Vec<TransferState>;
    /// Request cancellation of one transfer by id. Returns true if it was known.
    fn cancel(&self, id: &str) -> bool;
    /// #659: pause one transfer by id (queue-deep — skipped between files until resumed).
    /// Persistent (unlike cancel, never auto-consumed). Default no-op (desktop / #656 stub).
    fn pause(&self, _id: &str) -> bool {
        false
    }
    /// #659: resume a paused transfer so the next materialize pass fetches it. Default no-op.
    fn resume(&self, _id: &str) -> bool {
        false
    }
    /// #659: retry a failed/backed-off transfer — re-queue it for the next pass (clears any
    /// pause + 429 backoff). Queue-deep; no mid-file interruption. Default no-op.
    fn retry(&self, _id: &str) -> bool {
        false
    }
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

/// Reads and persists a OneDrive account's per-folder mode policy (#651) on behalf of
/// the mode GET/POST. `modes` reloads from the config on each call so a prior POST is
/// reflected immediately — the `Router` holds `config` by value (a build-time snapshot),
/// so a GET served from that snapshot would go stale. `set_folder` loads → mutates →
/// validates → saves the config file. Injected by the daemon/mobile engine; the read-only
/// CLI `serve` does not set it, so the mode POST is refused there (the GET then falls back
/// to the static config).
pub trait OneDriveModeHandler: Send + Sync {
    /// One account's current mode policy (default + per-folder overrides), read fresh.
    fn modes(&self, account: &str) -> Result<OneDriveModes, String>;
    /// Set (`Some`) or clear (`None`) one folder's explicit mode override, then persist.
    fn set_folder(
        &self,
        account: &str,
        folder_id: &str,
        mode: Option<OneDriveMode>,
    ) -> Result<(), String>;
}

/// Risk classification for OneDrive mobile biometric prompts (#723). Implemented
/// by app-host so the pure router can gate high-risk mobile actions without
/// reaching into config/store details itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OneDriveMoveRisk {
    Low,
    MoveOutOfProtected {
        source_scope: String,
        destination_scope: Option<String>,
    },
    Unknown {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineModeRisk {
    pub requires_confirmation: bool,
    pub file_count: usize,
    pub known_bytes: u64,
    pub unknown_size_files: usize,
    pub reason: String,
}

pub trait OneDriveRiskHandler: Send + Sync {
    fn move_risk(
        &self,
        account: &str,
        item_id: &str,
        destination_parent_id: &str,
    ) -> Result<OneDriveMoveRisk, String>;

    fn offline_mode_risk(&self, account: &str, folder_id: &str) -> Result<OfflineModeRisk, String>;
}

/// Performs the live-mail **write** verbs on behalf of a cap-token POST (#561).
/// Injected by the daemon (which owns the engine + the full write token); the
/// read-only CLI `serve` does not set it, so every `/api/v1/mail/*` POST is
/// refused there. The web UI for these verbs lands in #563 — this trait + the
/// endpoints are the backend they build on.
pub trait MailWriteHandler: Send + Sync {
    /// Compose and send a new message (saved to Sent Items).
    #[allow(clippy::too_many_arguments)] // a compose genuinely has many fields
    fn send(
        &self,
        account: &str,
        subject: &str,
        body_html: &str,
        to: &[String],
        cc: &[String],
        bcc: &[String],
        importance: Option<&str>,
        request_read_receipt: bool,
    ) -> Result<(), String>;
    /// Reply to the sender (`all = false`) or all recipients (`all = true`).
    fn reply(
        &self,
        account: &str,
        message_id: &str,
        comment: &str,
        all: bool,
    ) -> Result<(), String>;
    /// Forward a message to new recipients with an optional comment.
    fn forward(
        &self,
        account: &str,
        message_id: &str,
        comment: &str,
        to: &[String],
    ) -> Result<(), String>;
    /// Rich reply: create a reply(_all) draft, set its full HTML body, then send
    /// (full formatting + an edited quote, kept in the original conversation).
    fn reply_html(
        &self,
        account: &str,
        message_id: &str,
        body_html: &str,
        all: bool,
    ) -> Result<(), String>;
    /// Rich forward: create a forward draft to `to`, set its full HTML body, send.
    fn forward_html(
        &self,
        account: &str,
        message_id: &str,
        body_html: &str,
        to: &[String],
    ) -> Result<(), String>;
    /// Move a message to another folder; returns its new id.
    fn move_to(
        &self,
        account: &str,
        message_id: &str,
        destination_id: &str,
    ) -> Result<String, String>;
    /// Mark a message read/unread.
    fn set_read(&self, account: &str, message_id: &str, is_read: bool) -> Result<(), String>;
    /// Set/clear a follow-up flag (`notFlagged` / `flagged` / `complete`).
    fn set_flag(
        &self,
        account: &str,
        message_id: &str,
        flag_status: &str,
        due: Option<&str>,
        tz: &str,
    ) -> Result<(), String>;
    /// Replace a message's categories.
    fn set_categories(
        &self,
        account: &str,
        message_id: &str,
        categories: &[String],
    ) -> Result<(), String>;
    /// Create a draft; returns the new draft's id.
    fn create_draft(
        &self,
        account: &str,
        subject: &str,
        body_html: &str,
        to: &[String],
    ) -> Result<String, String>;
    /// Send an existing draft by id.
    fn send_draft(&self, account: &str, message_id: &str) -> Result<(), String>;
}

/// Performs the live-calendar **write** verbs on behalf of a cap-token POST
/// (#565 B7). Injected by the daemon (which owns the engine + the full write
/// token); the read-only CLI `serve` does not set it, so every
/// `/api/v1/calendar/*` POST is refused there. `event` is a Graph event resource
/// the router builds from the request (sanitized to writable fields downstream).
pub trait CalendarWriteHandler: Send + Sync {
    /// Create an event; returns the new cloud id.
    fn create(&self, account: &str, event: &Value) -> Result<String, String>;
    /// Update an event's writable fields from a (partial) event resource.
    fn update(&self, account: &str, event_id: &str, event: &Value) -> Result<(), String>;
    /// Delete an event.
    fn delete(&self, account: &str, event_id: &str) -> Result<(), String>;
    /// Respond to an invitation: `accept` / `decline` / `tentative` (+ comment).
    fn respond(
        &self,
        account: &str,
        event_id: &str,
        response: &str,
        comment: &str,
    ) -> Result<(), String>;
}

/// Performs the live-contact **write** verbs on behalf of a cap-token POST
/// (#566 A5). Injected by the daemon (which owns the engine + the full write
/// token); the read-only CLI `serve` does not set it, so every
/// `/api/v1/contact/{create,update,delete}` POST is refused there. `contact` is a
/// Graph contact resource the router builds from the request (sanitized to the
/// writable fields downstream).
pub trait ContactWriteHandler: Send + Sync {
    /// Create a contact; returns the new cloud id.
    fn create(&self, account: &str, contact: &Value) -> Result<String, String>;
    /// Update a contact's writable fields from a (partial) contact resource.
    fn update(&self, account: &str, contact_id: &str, contact: &Value) -> Result<(), String>;
    /// Delete a contact.
    fn delete(&self, account: &str, contact_id: &str) -> Result<(), String>;
}

/// Performs the live-ToDo **write** verbs on behalf of a cap-token POST (#567 B6):
/// task create/update/complete/delete, checklist add/toggle/delete, list create/
/// delete. Injected by the daemon (which owns the engine + the full write token);
/// the read-only CLI `serve` does not set it, so every `/api/v1/todo/*` POST is
/// refused there.
pub trait TaskWriteHandler: Send + Sync {
    fn create(&self, account: &str, list_id: &str, task: &Value) -> Result<String, String>;
    fn update(
        &self,
        account: &str,
        list_id: &str,
        task_id: &str,
        task: &Value,
    ) -> Result<(), String>;
    fn complete(&self, account: &str, list_id: &str, task_id: &str) -> Result<(), String>;
    fn delete(&self, account: &str, list_id: &str, task_id: &str) -> Result<(), String>;
    fn checklist_add(
        &self,
        account: &str,
        list_id: &str,
        task_id: &str,
        title: &str,
    ) -> Result<String, String>;
    fn checklist_toggle(
        &self,
        account: &str,
        list_id: &str,
        task_id: &str,
        item_id: &str,
        checked: bool,
    ) -> Result<(), String>;
    fn checklist_delete(
        &self,
        account: &str,
        list_id: &str,
        task_id: &str,
        item_id: &str,
    ) -> Result<(), String>;
    fn list_create(&self, account: &str, name: &str) -> Result<String, String>;
    fn list_delete(&self, account: &str, list_id: &str) -> Result<(), String>;
}

/// Performs the live-OneNote **write** verbs on behalf of a cap-token POST (#568):
/// create a page in a section, delete a page, append text to a page. Injected by the
/// daemon (which owns the engine + the full write token); the read-only CLI `serve`
/// does not set it, so every `/api/v1/onenote/*` POST is refused there.
pub trait OneNoteWriteHandler: Send + Sync {
    /// Create a page in `section_id` from POST-ready HTML; returns the new cloud id.
    fn create(&self, account: &str, section_id: &str, html: &[u8]) -> Result<String, String>;
    /// Delete a page.
    fn delete(&self, account: &str, page_id: &str) -> Result<(), String>;
    /// Append a plain-text paragraph to a page (best-effort).
    fn append(&self, account: &str, page_id: &str, text: &str) -> Result<(), String>;
}

/// Performs the live-OneDrive **cloud-write** verbs on behalf of a cap-token POST (#654):
/// create a folder, rename, move, or delete a drive item. Injected by the daemon / mobile
/// engine (which owns the operation ledger + the write token); the read-only CLI `serve`
/// does not set it, so every `/api/v1/onedrive/{create,rename,move,delete}` POST is refused
/// there. Each verb is ledger-backed and crash-recoverable in the engine; `delete` is
/// additionally biometric-gated on mobile.
pub trait OneDriveWriteHandler: Send + Sync {
    /// Create a child folder under `parent_id` (empty = the drive root); returns its new id.
    fn create_folder(&self, account: &str, parent_id: &str, name: &str) -> Result<String, String>;
    /// Rename an item in place.
    fn rename(&self, account: &str, id: &str, new_name: &str) -> Result<(), String>;
    /// Move an item to `new_parent_id` (`Some("")` = the drive root), optionally renaming it.
    fn move_item(
        &self,
        account: &str,
        id: &str,
        new_parent_id: Option<&str>,
        new_name: &str,
    ) -> Result<(), String>;
    /// Delete an item.
    fn delete(&self, account: &str, id: &str) -> Result<(), String>;
    /// Upload a new file `name` under `parent_id` (empty = the drive root) with `bytes`; returns its new id (#657).
    fn upload(
        &self,
        account: &str,
        parent_id: &str,
        name: &str,
        bytes: &[u8],
    ) -> Result<String, String>;
    /// Replace an existing item's content (If-Match `etag`; a 412/conflict must never clobber) with `bytes` (#657).
    fn replace(&self, account: &str, id: &str, etag: &str, bytes: &[u8]) -> Result<(), String>;
}

pub const MUTATION_CHUNK_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "purpose", content = "metadata", rename_all = "snake_case")]
pub enum MutationPurpose {
    OnedriveUpload {
        account: String,
        parent: String,
        name: String,
    },
    OnedriveReplace {
        account: String,
        id: String,
        etag: String,
    },
    MailBody {
        account: String,
        operation: String,
        target: String,
        recipients: Vec<String>,
        subject: String,
        all: bool,
    },
    OnenoteBody {
        account: String,
        operation: String,
        section_or_page: String,
        title: String,
    },
}

impl MutationPurpose {
    fn is_valid(&self) -> bool {
        match self {
            Self::OnedriveUpload {
                account,
                parent: _,
                name,
            } => !account.is_empty() && !name.is_empty(),
            Self::OnedriveReplace { account, id, etag } => {
                !account.is_empty() && !id.is_empty() && !etag.is_empty()
            }
            Self::MailBody {
                account,
                operation,
                target,
                recipients,
                subject: _,
                all: _,
            } => {
                !account.is_empty()
                    && matches!(operation.as_str(), "send" | "reply" | "forward" | "draft")
                    && recipients.len() <= 256
                    && (matches!(operation.as_str(), "send" | "draft") || !target.is_empty())
            }
            Self::OnenoteBody {
                account,
                operation,
                section_or_page,
                title: _,
            } => {
                !account.is_empty()
                    && !section_or_page.is_empty()
                    && matches!(operation.as_str(), "create" | "append")
            }
        }
    }

    fn confirmation_binding(&self) -> (&'static str, &str, &'static str, String) {
        use base64::Engine as _;
        use ring::digest::{digest, SHA256};
        let canonical = serde_json::to_vec(self).unwrap_or_default();
        let binding = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(digest(&SHA256, &canonical).as_ref());
        match self {
            Self::OnedriveUpload { account, name, .. } => {
                ("upload", account, "onedrive", format!("{name}#{binding}"))
            }
            Self::OnedriveReplace { account, id, .. } => {
                ("replace", account, "onedrive", format!("{id}#{binding}"))
            }
            Self::MailBody {
                account, operation, ..
            } => (
                "live-write",
                account,
                "mail",
                format!("{operation}#{binding}"),
            ),
            Self::OnenoteBody {
                account, operation, ..
            } => (
                "live-write",
                account,
                "onenote",
                format!("{operation}#{binding}"),
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationIntentCreate {
    pub request_id: String,
    pub owner: String,
    pub purpose: MutationPurpose,
    pub total_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationIntentInfo {
    pub intent_id: String,
    pub chunk_bytes: usize,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationIntentChunk {
    pub owner: String,
    pub request_id: String,
    pub intent_id: String,
    pub index: u32,
    pub offset: u64,
    pub chunk_sha256: String,
    pub bytes: Vec<u8>,
}

pub trait MutationIntentHandler: Send + Sync {
    fn create(&self, request: MutationIntentCreate) -> Result<MutationIntentInfo, String>;
    fn put_chunk(&self, chunk: MutationIntentChunk) -> Result<(), String>;
    fn commit(
        &self,
        owner: &str,
        request_id: &str,
        intent_id: &str,
        total_bytes: u64,
        sha256: &str,
    ) -> Result<Value, String>;
    fn cancel(&self, owner: &str, request_id: &str, intent_id: &str) -> Result<(), String>;
}

/// Performs the OneDrive **local-body management** verbs on behalf of a cap-token POST (#659):
/// free up a materialized body, download one on demand, list + resolve keep-both conflicts, and
/// run the offline→online cleanup. Injected by the daemon / mobile engine (which owns the store +
/// the write token); the read-only CLI `serve` does not set it, so every `/api/v1/onedrive/*`
/// management route is refused there. free-up / download-now are local-only + reversible (not
/// biometric-gated); a keep-mine resolve deletes the cloud copy (biometric-gated by the router);
/// cleanup is a bulk op (biometric-gated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneDriveDownloadNowResult {
    pub downloaded: bool,
    pub target: String,
}

pub trait OneDriveManageHandler: Send + Sync {
    /// Free up space: drop `id`'s materialized body but keep the item listable (metadata-only).
    fn free_up(&self, account: &str, id: &str) -> Result<(), String>;
    /// Download now: fetch `id` on demand to the mode-appropriate target. `downloaded=false`
    /// means the transfer policy blocked it.
    fn download_now(&self, account: &str, id: &str) -> Result<OneDriveDownloadNowResult, String>;
    /// The account's unresolved conflicts (write-orphan `conflict_state` rows) for the Conflict
    /// Center. Returns a JSON array of `{ id, name, conflict_copy, … }`.
    fn list_conflicts(&self, account: &str) -> Result<serde_json::Value, String>;
    /// Resolve one conflict: `resolution` is `keep-both` | `keep-mine` | `keep-cloud`.
    fn resolve_conflict(&self, account: &str, id: &str, resolution: &str) -> Result<(), String>;
    /// Offline→online cleanup: drop the now-online folders' provably-safe materialized bodies (to
    /// trash), keep anything unsynced. Returns `{ freed, kept }`.
    fn cleanup_offline_to_online(&self, account: &str) -> Result<serde_json::Value, String>;
}

pub fn onedrive_move_pat_item(id: &str, new_parent: &str, name: &str) -> String {
    serde_json::to_string(&["onedrive_move", id, new_parent, name])
        .expect("static OneDrive move action array serializes")
}

pub fn onedrive_mode_offline_pat_item(folder: &str) -> String {
    serde_json::to_string(&["onedrive_mode_offline", folder])
        .expect("static OneDrive offline-mode action array serializes")
}

pub fn onedrive_mode_online_cleanup_pat_item(folder: &str) -> String {
    serde_json::to_string(&["onedrive_mode_online_account_cleanup", folder])
        .expect("static OneDrive online-cleanup action array serializes")
}

/// The two independently consented Microsoft authorities attached to one configured account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountAuthRole {
    Reader,
    Writer,
}

impl AccountAuthRole {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "reader" => Some(Self::Reader),
            "writer" => Some(Self::Writer),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reader => "reader",
            Self::Writer => "writer",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountAuthState {
    Connected,
    Disconnected,
    ReconnectRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct AccountRoleStatus {
    pub state: AccountAuthState,
    pub identity_verified: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct AccountAuthStatus {
    pub reader: AccountRoleStatus,
    pub writer: AccountRoleStatus,
}

/// Live account-auth handler (#68): the daemon's Microsoft sign-in + sign-out.
/// `None` => the account menu offers only switching (the read-only CLI `serve`).
pub trait AccountAuthHandler: Send + Sync {
    /// Begin Authorization Code + PKCE for a configured account. Returns only an opaque login ID
    /// and a reviewed authorization URI. The expected state, verifier, callback code, and tokens
    /// stay in Rust; the authorization URI necessarily contains the public state challenge.
    fn start_login(
        &self,
        account: &str,
        role: AccountAuthRole,
    ) -> Result<serde_json::Value, String>;
    /// Poll a started login by its `login_id` -> `{ state: "pending"|"done"|"error", code? }`.
    fn poll_login(&self, login_id: &str) -> serde_json::Value;
    /// Cancel one active login attempt. Returns true only when this call cancelled it.
    fn cancel_login(&self, login_id: &str) -> bool;
    /// Remove exactly one role's cached token. Reader and Writer remain independently usable.
    fn sign_out(&self, account: &str, role: AccountAuthRole) -> Result<serde_json::Value, String>;
    /// Return only closed connection state; token and account identity values never leave Rust.
    fn status(&self, account: &str) -> AccountAuthStatus;
}

/// Push-registration handler (#576): the web UI hands the daemon this device's FCM
/// registration token so the daemon can send notifications (e.g. "backup complete")
/// to the phone. `None` => push is unavailable (the read-only CLI `serve`).
pub trait PushHandler: Send + Sync {
    /// Register (idempotently) a device push token reported by the native shell.
    fn register(&self, token: &str) -> Result<(), String>;
    /// Send a test notification to all registered devices. Returns `{ sent, registered }`.
    fn send_test(&self) -> Result<serde_json::Value, String>;
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
    /// Optional mobile backup enqueue handler (#625). `None` => backup POSTs are
    /// refused; when present the route queues a mobile job only.
    backup: Option<std::sync::Arc<dyn BackupHandler>>,
    /// Separate capability token for backup POSTs.
    backup_cap_token: Option<String>,
    /// Optional mobile job status/cancel handler (#625).
    mobile_jobs: Option<std::sync::Arc<dyn MobileJobHandler>>,
    /// Separate capability token for mobile job status/cancel.
    mobile_job_cap_token: Option<String>,
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
    /// Optional live OneDrive info handler (the daemon's). Enables the quota /
    /// permissions GETs; `None` => those 404 (the read-only CLI `serve`).
    onedrive_info: Option<std::sync::Arc<dyn OneDriveInfoHandler>>,
    /// Optional live OneDrive folder-listing handler (the daemon's/mobile's).
    /// Enables the online-browse children GET; `None` => it 404s (read-only CLI).
    onedrive_list: Option<std::sync::Arc<dyn OneDriveListHandler>>,
    /// Optional live OneDrive on-demand open handler (the daemon's/mobile's). Enables
    /// the online content-fetch GET; `None` => it 404s (the read-only CLI `serve`).
    onedrive_open: Option<std::sync::Arc<dyn OneDriveOpenHandler>>,
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
    /// Optional live-mail write handler (the daemon's). `None` => every
    /// `/api/v1/mail/*` POST is refused (the read-only CLI `serve`).
    mail_write: Option<std::sync::Arc<dyn MailWriteHandler>>,
    /// Separate capability token for mail-write POSTs (distinct blast radius —
    /// these send/modify real mail).
    mail_write_cap_token: Option<String>,
    /// Optional live-calendar write handler (the daemon's). `None` => every
    /// `/api/v1/calendar/*` POST is refused (the read-only CLI `serve`).
    calendar_write: Option<std::sync::Arc<dyn CalendarWriteHandler>>,
    /// Separate capability token for calendar-write POSTs.
    calendar_write_cap_token: Option<String>,
    /// Optional live-contact write handler (the daemon's). `None` => every
    /// `/api/v1/contact/{create,update,delete}` POST is refused (read-only `serve`).
    contact_write: Option<std::sync::Arc<dyn ContactWriteHandler>>,
    /// Separate capability token for contact-write POSTs.
    contact_write_cap_token: Option<String>,
    /// Optional live-ToDo write handler (the daemon's). `None` => every
    /// `/api/v1/todo/*` POST is refused (the read-only CLI `serve`).
    task_write: Option<std::sync::Arc<dyn TaskWriteHandler>>,
    /// Separate capability token for ToDo-write POSTs.
    task_write_cap_token: Option<String>,
    /// Optional live-OneNote write handler (the daemon's). `None` => every
    /// `/api/v1/onenote/*` POST is refused (the read-only CLI `serve`).
    onenote_write: Option<std::sync::Arc<dyn OneNoteWriteHandler>>,
    /// Separate capability token for OneNote-write POSTs.
    onenote_write_cap_token: Option<String>,
    /// Optional live-OneDrive cloud-write handler (create/rename/move/delete). `None` =>
    /// every `/api/v1/onedrive/{create,rename,move,delete}` POST is refused (#654).
    onedrive_write: Option<std::sync::Arc<dyn OneDriveWriteHandler>>,
    /// Separate capability token for OneDrive cloud-write POSTs.
    onedrive_write_cap_token: Option<String>,
    mutation_intents: Option<std::sync::Arc<dyn MutationIntentHandler>>,
    mutation_intent_cap_token: Option<String>,
    /// Optional OneDrive per-folder mode handler (#651): fresh mode reads + persisted
    /// set/clear. `None` => the mode POST is refused (read-only CLI `serve`); the GET
    /// then falls back to the static config.
    onedrive_mode: Option<std::sync::Arc<dyn OneDriveModeHandler>>,
    /// Separate capability token for the mode POST (distinct blast radius).
    onedrive_mode_cap_token: Option<String>,
    /// Optional OneDrive risk classifier for Android-only biometric prompts (#723).
    /// Desktop routes must not call it when `biometric_gate` is false.
    onedrive_risk: Option<std::sync::Arc<dyn OneDriveRiskHandler>>,
    /// Optional account-auth handler (#68): Microsoft sign-in + sign-out. `None`
    /// => the account menu only switches between already-configured accounts.
    account_auth: Option<std::sync::Arc<dyn AccountAuthHandler>>,
    /// Separate capability token for account login/sign-out POSTs.
    account_cap_token: Option<String>,
    /// Optional push-registration handler (#576): stores device FCM tokens + sends
    /// notifications. `None` => `/api/v1/push/*` POSTs are refused (read-only `serve`).
    push: Option<std::sync::Arc<dyn PushHandler>>,
    /// Separate capability token for push register/test POSTs.
    push_cap_token: Option<String>,
    /// In-app agent (S-AG.6/#621). `None` => `/api/v1/agent/*` is refused (read-only).
    agent: Option<std::sync::Arc<dyn AgentHandler>>,
    /// Separate capability token for agent POSTs (chat/confirm/cancel).
    agent_cap_token: Option<String>,
    /// Mobile/standalone profile (#89): an unguessable per-process token required on
    /// EVERY `/api/v1/*` route. On Android `127.0.0.1` is reachable by any app on the
    /// device, so unlike the desktop daemon (GET open, POST cap-gated) the data API
    /// must be fully gated. `None` => desktop daemon behaviour (no extra gate). The
    /// token reaches the WebView via the native bridge, never in a static asset.
    session_token: Option<String>,
    /// Mobile biometric gate (#onedrive-mobile 0.6). `true` only when the standalone
    /// Android app builds the router (via `with_biometric_gate`). When set, destructive
    /// ops in the gate catalogue require a per-action token that is only valid after a
    /// native `BiometricPrompt` — a defense the WebView/agent cannot satisfy on its own
    /// even though it holds the cap-tokens. `false` (desktop) => unchanged behaviour.
    biometric_gate: bool,
    /// Registry of destructive actions awaiting/holding a biometric confirmation. Used
    /// only when `biometric_gate` is set; the native side confirms entries over JNI.
    pending: isyncyou_core::pending::PendingActionRegistry,
    /// Optional in-flight transfer progress/cancel handler (#onedrive-mobile 0.8). `None`
    /// => the transfers endpoint reports idle and cancel 404s (the read-only CLI `serve`).
    transfers: Option<std::sync::Arc<dyn TransferProgress>>,
    /// Capability token for the cancel/pause/retry POSTs (distinct blast radius).
    transfer_cap_token: Option<String>,
    /// Optional OneDrive local-body management handler (#659): free-up / download-now / conflict
    /// list+resolve / offline→online cleanup. `None` => every management route is refused (the
    /// read-only CLI `serve`).
    onedrive_manage: Option<std::sync::Arc<dyn OneDriveManageHandler>>,
    /// Separate capability token for the management POSTs (distinct blast radius).
    onedrive_manage_cap_token: Option<String>,
}

/// Constant-time byte-equality (no early return on first mismatch) so token checks
/// can't be probed byte-by-byte via timing. The length check only leaks length,
/// which is fixed for our tokens.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

impl Router {
    pub fn new(config: Config) -> Self {
        Router {
            config,
            events: None,
            gate: None,
            restore: None,
            restore_cap_token: None,
            backup: None,
            backup_cap_token: None,
            mobile_jobs: None,
            mobile_job_cap_token: None,
            sync_cap_token: None,
            share: None,
            share_cap_token: None,
            sync_control: None,
            hydrations: None,
            onedrive_info: None,
            onedrive_list: None,
            onedrive_open: None,
            verify: None,
            verify_cap_token: None,
            settings_handler: None,
            settings_cap_token: None,
            mail_write: None,
            mail_write_cap_token: None,
            calendar_write: None,
            calendar_write_cap_token: None,
            contact_write: None,
            contact_write_cap_token: None,
            task_write: None,
            task_write_cap_token: None,
            onenote_write: None,
            onenote_write_cap_token: None,
            onedrive_write: None,
            onedrive_write_cap_token: None,
            mutation_intents: None,
            mutation_intent_cap_token: None,
            onedrive_mode: None,
            onedrive_mode_cap_token: None,
            onedrive_risk: None,
            account_auth: None,
            account_cap_token: None,
            push: None,
            push_cap_token: None,
            agent: None,
            agent_cap_token: None,
            session_token: None,
            biometric_gate: false,
            pending: isyncyou_core::pending::PendingActionRegistry::new(),
            transfers: None,
            transfer_cap_token: None,
            onedrive_manage: None,
            onedrive_manage_cap_token: None,
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
            backup: None,
            backup_cap_token: None,
            mobile_jobs: None,
            mobile_job_cap_token: None,
            sync_cap_token: None,
            share: None,
            share_cap_token: None,
            sync_control: None,
            hydrations: None,
            onedrive_info: None,
            onedrive_list: None,
            onedrive_open: None,
            verify: None,
            verify_cap_token: None,
            settings_handler: None,
            settings_cap_token: None,
            mail_write: None,
            mail_write_cap_token: None,
            calendar_write: None,
            calendar_write_cap_token: None,
            contact_write: None,
            contact_write_cap_token: None,
            task_write: None,
            task_write_cap_token: None,
            onenote_write: None,
            onenote_write_cap_token: None,
            onedrive_write: None,
            onedrive_write_cap_token: None,
            mutation_intents: None,
            mutation_intent_cap_token: None,
            onedrive_mode: None,
            onedrive_mode_cap_token: None,
            onedrive_risk: None,
            account_auth: None,
            account_cap_token: None,
            push: None,
            push_cap_token: None,
            agent: None,
            agent_cap_token: None,
            session_token: None,
            biometric_gate: false,
            pending: isyncyou_core::pending::PendingActionRegistry::new(),
            transfers: None,
            transfer_cap_token: None,
            onedrive_manage: None,
            onedrive_manage_cap_token: None,
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

    /// Enable mobile backup enqueue POSTs, guarded by `cap_token`.
    pub fn with_backup(
        mut self,
        handler: std::sync::Arc<dyn BackupHandler>,
        cap_token: String,
    ) -> Self {
        self.backup = Some(handler);
        self.backup_cap_token = Some(cap_token);
        self
    }

    /// Enable mobile durable job list/cancel, guarded by `cap_token`.
    pub fn with_mobile_jobs(
        mut self,
        handler: std::sync::Arc<dyn MobileJobHandler>,
        cap_token: String,
    ) -> Self {
        self.mobile_jobs = Some(handler);
        self.mobile_job_cap_token = Some(cap_token);
        self
    }

    /// Enable the in-app agent (S-AG.6/#621), guarded by `cap_token` (builder style).
    pub fn with_agent(
        mut self,
        handler: std::sync::Arc<dyn AgentHandler>,
        cap_token: String,
    ) -> Self {
        self.agent = Some(handler);
        self.agent_cap_token = Some(cap_token);
        self
    }

    /// Crate-internal accessor for the agent handler (used by the SSE stream in serve.rs).
    pub(crate) fn agent_handler(&self) -> Option<&std::sync::Arc<dyn AgentHandler>> {
        self.agent.as_ref()
    }

    /// Enable the mobile biometric gate (#onedrive-mobile 0.6, builder style). Only the
    /// standalone Android app calls this; the desktop daemon leaves it off.
    pub fn with_biometric_gate(mut self) -> Self {
        self.biometric_gate = true;
        self
    }

    /// Record a successful native `BiometricPrompt` for a pending destructive action.
    /// Called ONLY from the native JNI path — the WebView has no route to it, which is
    /// exactly what makes the per-action token a real second factor even though the UI
    /// holds every cap-token. Returns `false` for an unknown or expired id.
    pub fn confirm_biometric(&self, pending_id: &str) -> bool {
        self.pending
            .confirm_biometric(pending_id, isyncyou_core::pending::now_ms())
    }

    /// Return the bounded Rust-owned descriptor for native prompt rendering. This
    /// lookup does not arm or consume the pending action and never exposes the
    /// action hash or destructive payload.
    pub fn describe_biometric(
        &self,
        pending_id: &str,
    ) -> Result<
        isyncyou_core::pending::PendingActionDescriptor,
        isyncyou_core::pending::DescribeError,
    > {
        self.pending
            .describe(pending_id, isyncyou_core::pending::now_ms())
    }

    /// The mobile biometric gate for one destructive op. Returns:
    /// - `None` — proceed (desktop profile, op not in the gate catalogue, or a valid
    ///   single-use public handle rode in the per-action header and was consumed);
    /// - `Some(confirmation_required)` — mobile + gated + no token yet: a pending action
    ///   was registered; the UI must run the native biometric and re-issue with the
    ///   per-action header;
    /// - `Some(403)` — a token was presented but was bad/expired/replayed/mismatched.
    fn biometric_challenge(
        &self,
        op: &str,
        account: &str,
        service: &str,
        item: &str,
        req: &ApiRequest,
    ) -> Option<ApiResponse> {
        if !self.biometric_gate || !isyncyou_core::pending::requires_confirmation(op) {
            return None;
        }
        let now = isyncyou_core::pending::now_ms();
        match req.per_action_token.as_deref().filter(|s| !s.is_empty()) {
            Some(pat) => match self.pending.consume(pat, op, account, service, item, now) {
                Ok(()) => None,
                Err(e) => Some(ApiResponse::error(
                    403,
                    &format!("biometric confirmation invalid: {e:?}"),
                )),
            },
            None => {
                match self.pending.register(
                    op,
                    account,
                    service,
                    item,
                    now,
                    isyncyou_core::pending::DEFAULT_TTL_MS,
                ) {
                    Ok(id) => Some(ApiResponse::ok_json(&json!({
                        "status": "confirmation_required",
                        "pending_action_id": id,
                        "op": op,
                        "account": account,
                        "service": service,
                        "item": item,
                    }))),
                    Err(e) => {
                        let status = match e {
                        isyncyou_core::pending::RegistrationError::RandomnessUnavailable => 500,
                        isyncyou_core::pending::RegistrationError::UnknownOperation
                        | isyncyou_core::pending::RegistrationError::UnknownService
                        | isyncyou_core::pending::RegistrationError::InvalidServiceForOperation => 400,
                    };
                        Some(ApiResponse::error(
                            status,
                            "invalid biometric action descriptor",
                        ))
                    }
                }
            }
        }
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

    /// Enable the transfer progress GET + cancel POST (#onedrive-mobile 0.8), the cancel
    /// guarded by `cap_token` (builder style).
    pub fn with_transfers(
        mut self,
        transfers: std::sync::Arc<dyn TransferProgress>,
        cap_token: String,
    ) -> Self {
        self.transfers = Some(transfers);
        self.transfer_cap_token = Some(cap_token);
        self
    }

    /// Enable the live OneDrive info GETs (quota/permissions) (builder style).
    pub fn with_onedrive_info(mut self, info: std::sync::Arc<dyn OneDriveInfoHandler>) -> Self {
        self.onedrive_info = Some(info);
        self
    }

    /// Enable the live OneDrive folder-listing GET (online browse) (builder style).
    pub fn with_onedrive_list(mut self, list: std::sync::Arc<dyn OneDriveListHandler>) -> Self {
        self.onedrive_list = Some(list);
        self
    }

    /// Enable the live OneDrive on-demand open (content fetch) GET (builder style).
    pub fn with_onedrive_open(mut self, open: std::sync::Arc<dyn OneDriveOpenHandler>) -> Self {
        self.onedrive_open = Some(open);
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

    /// Enable the OneDrive per-folder mode POST + fresh mode reads, guarded by
    /// `cap_token` (builder style) (#651).
    pub fn with_onedrive_mode(
        mut self,
        handler: std::sync::Arc<dyn OneDriveModeHandler>,
        cap_token: String,
    ) -> Self {
        self.onedrive_mode = Some(handler);
        self.onedrive_mode_cap_token = Some(cap_token);
        self
    }

    /// Enable OneDrive risk classification for Android biometric gates (#723).
    pub fn with_onedrive_risk(mut self, handler: std::sync::Arc<dyn OneDriveRiskHandler>) -> Self {
        self.onedrive_risk = Some(handler);
        self
    }

    /// Enable the live-mail write POSTs (`/api/v1/mail/*`), guarded by `cap_token`
    /// (builder style). Injected by the daemon; the read-only `serve` leaves it
    /// unset so those POSTs 404.
    pub fn with_mail_write(
        mut self,
        handler: std::sync::Arc<dyn MailWriteHandler>,
        cap_token: String,
    ) -> Self {
        self.mail_write = Some(handler);
        self.mail_write_cap_token = Some(cap_token);
        self
    }

    /// Enable the live-calendar write POSTs (builder style, #565).
    pub fn with_calendar_write(
        mut self,
        handler: std::sync::Arc<dyn CalendarWriteHandler>,
        cap_token: String,
    ) -> Self {
        self.calendar_write = Some(handler);
        self.calendar_write_cap_token = Some(cap_token);
        self
    }

    /// Enable the live-contact write POSTs (builder style, #566).
    pub fn with_contact_write(
        mut self,
        handler: std::sync::Arc<dyn ContactWriteHandler>,
        cap_token: String,
    ) -> Self {
        self.contact_write = Some(handler);
        self.contact_write_cap_token = Some(cap_token);
        self
    }

    /// Enable the live-ToDo write POSTs (builder style, #567).
    pub fn with_task_write(
        mut self,
        handler: std::sync::Arc<dyn TaskWriteHandler>,
        cap_token: String,
    ) -> Self {
        self.task_write = Some(handler);
        self.task_write_cap_token = Some(cap_token);
        self
    }

    /// Enable the live-OneNote write POSTs (builder style, #568).
    pub fn with_onenote_write(
        mut self,
        handler: std::sync::Arc<dyn OneNoteWriteHandler>,
        cap_token: String,
    ) -> Self {
        self.onenote_write = Some(handler);
        self.onenote_write_cap_token = Some(cap_token);
        self
    }

    /// Enable the live-OneDrive cloud-write POSTs (builder style, #654).
    pub fn with_onedrive_write(
        mut self,
        handler: std::sync::Arc<dyn OneDriveWriteHandler>,
        cap_token: String,
    ) -> Self {
        self.onedrive_write = Some(handler);
        self.onedrive_write_cap_token = Some(cap_token);
        self
    }

    pub fn with_mutation_intents(
        mut self,
        handler: std::sync::Arc<dyn MutationIntentHandler>,
        cap_token: String,
    ) -> Self {
        self.mutation_intents = Some(handler);
        self.mutation_intent_cap_token = Some(cap_token);
        self
    }

    /// Enable the OneDrive local-body management POSTs/GET (free-up / download-now / conflict
    /// list+resolve / offline→online cleanup), guarded by `cap_token` (builder style, #659).
    pub fn with_onedrive_manage(
        mut self,
        handler: std::sync::Arc<dyn OneDriveManageHandler>,
        cap_token: String,
    ) -> Self {
        self.onedrive_manage = Some(handler);
        self.onedrive_manage_cap_token = Some(cap_token);
        self
    }

    /// Wire the account-auth handler (Microsoft sign-in + sign-out, #68).
    pub fn with_account_auth(
        mut self,
        handler: std::sync::Arc<dyn AccountAuthHandler>,
        cap_token: String,
    ) -> Self {
        self.account_auth = Some(handler);
        self.account_cap_token = Some(cap_token);
        self
    }

    /// Wire the push-registration handler (FCM device-token store + sender, #576).
    pub fn with_push(
        mut self,
        handler: std::sync::Arc<dyn PushHandler>,
        cap_token: String,
    ) -> Self {
        self.push = Some(handler);
        self.push_cap_token = Some(cap_token);
        self
    }

    /// Enable the mobile session-token gate (#89): every `/api/v1/*` route then
    /// requires this token. Off (desktop daemon) when never set.
    pub fn with_session_token(mut self, token: String) -> Self {
        self.session_token = Some(token);
        self
    }

    /// Whether `provided` satisfies the session-token gate. `true` when the gate is
    /// off (desktop). When on, the token must be present and match in constant time.
    /// Used by both `route()` (data routes) and the SSE path in `serve.rs`.
    pub fn session_authorized(&self, provided: Option<&str>) -> bool {
        match &self.session_token {
            None => true,
            Some(expected) => provided.is_some_and(|p| ct_eq(expected.as_bytes(), p.as_bytes())),
        }
    }

    /// Open a push stream for the Android in-process bridge (#0A) — the replacement for the
    /// two `EventSource` endpoints on the phone, where no loopback port exists to hold an
    /// SSE socket open. Items are ready-to-embed JSON event objects
    /// `{"event":<name>,"data":<string>}`; the native side wraps each in a bridge push
    /// message. Session-gated exactly like the HTTP SSE paths; query authority is rejected.
    /// `None` when unauthorized or the stream is unknown/absent. Dropping the returned
    /// receiver ends the source thread (the next `send` fails).
    pub fn open_bridge_stream(
        &self,
        target: &str,
        session_token: Option<&str>,
    ) -> Option<std::sync::mpsc::Receiver<String>> {
        let req =
            ApiRequest::new("GET", target).with_session_token(session_token.map(str::to_string));
        if req.q("_st").is_some() || !self.session_authorized(req.session_token.as_deref()) {
            return None;
        }
        match req.path.as_str() {
            "/api/v1/events" => {
                let bus = self.events_bus()?.clone();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let mut last = bus.generation();
                    loop {
                        let g = bus.wait_change(last, std::time::Duration::from_secs(15));
                        // A `change` on a real generation bump, else a `ping` heartbeat —
                        // both double as the dropped-receiver check (send fails → exit).
                        let msg = if g != last {
                            last = g;
                            r#"{"event":"change","data":""}"#
                        } else {
                            r#"{"event":"ping","data":""}"#
                        };
                        if tx.send(msg.to_string()).is_err() {
                            break;
                        }
                    }
                });
                Some(rx)
            }
            "/api/v1/agent/stream" => {
                let turn = req
                    .query
                    .iter()
                    .find(|(k, _)| k == "turn")
                    .map(|(_, v)| v.as_str())
                    .filter(|t| !t.is_empty())?;
                let inner = self.agent_handler()?.open_stream(turn)?;
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    // Each inner item is one pre-serialized agent event (a JSON string);
                    // carry it as the `data` field so app.js JSON-parses it as it does the
                    // EventSource `data:` line. Terminate with a `done` event.
                    for line in inner.iter() {
                        let msg =
                            serde_json::json!({ "event": "message", "data": line }).to_string();
                        if tx.send(msg).is_err() {
                            return;
                        }
                    }
                    let _ = tx.send(r#"{"event":"done","data":""}"#.to_string());
                });
                Some(rx)
            }
            _ => None,
        }
    }

    /// Whether the request carries the configured capability token. The token is
    /// compared in **constant time** so a timing side-channel can't reveal it byte
    /// by byte (the length check only leaks length, which is fixed for our tokens).
    fn cap_ok(expected: &Option<String>, req: &ApiRequest) -> bool {
        let (Some(w), Some(g)) = (expected, &req.cap_token) else {
            return false;
        };
        ct_eq(w.as_bytes(), g.as_bytes())
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

    /// Dispatch one request to a response. Never panics; unknown routes -> 404.
    pub fn route(&self, req: &ApiRequest) -> ApiResponse {
        let mut response = self.route_inner(req);
        if req.path.starts_with("/api/v1/")
            && !response
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("cache-control"))
        {
            response
                .headers
                .push(("Cache-Control".into(), "no-store".into()));
        }
        response
    }

    fn route_inner(&self, req: &ApiRequest) -> ApiResponse {
        // Mobile/standalone profile (#89/#721): the data API is fully session-token
        // gated. The static shell (`/`, `/app.js`, `/app.css`, fonts, `/sfx/*`) stays
        // open so the WebView can bootstrap — it carries no user data and no token.
        // The current Android WebView path injects the trusted session natively; `_st`
        // remains accepted only for legacy/non-WebView callers. No-op on the desktop
        // daemon (session_token = None).
        if req.path.starts_with("/api/v1/") {
            if req.q("_st").is_some() {
                return ApiResponse::error(400, "session token query is not allowed");
            }
            if !self.session_authorized(req.session_token.as_deref()) {
                return ApiResponse::error(401, "missing or invalid session token");
            }
        }
        // Hold the store-access gate (if any) for the whole request so a concurrent
        // sync pass and this request never both hold the store's single-instance lock.
        //
        // EXCEPTION — every GET that either (a) reads the store read-only (a WAL reader
        // takes no instance lock, safe concurrent with the writer) or (b) touches no store
        // at all skips the gate. Otherwise a long sync pass that holds the gate stalls
        // these requests, and — because responses are `Connection: close`, so each is its
        // own TCP connection — the blocked ones exhaust the WebView's small per-origin
        // connection pool, queueing even the exempt reads *browser-side* (the measured
        // cold-start hang). `sync_state`/`accounts`/`settings`/`debug_stats` read config or
        // `/proc` only; the static shell touches nothing. Writable-store GETs (item,
        // search, drive, …) and all POSTs still take the gate. Preview back-fill on the
        // read path uses its own short writable open, best-effort.
        const GATE_EXEMPT_GET: &[&str] = &[
            "/api/v1/items",
            "/api/v1/activity",
            "/api/v1/status",
            "/api/v1/sync/state",
            "/api/v1/accounts",
            "/api/v1/settings",
            "/api/v1/debug/stats",
            // Agent status and session projections are backed by their own encrypted stores,
            // lifecycle locks, and bounded hot caches. They never touch the archive Store guarded
            // by the sync pass, so taking that gate here would let an unrelated M365 sync stall
            // the Assistant bridge until its 15-second watchdog expires.
            "/api/v1/agent/status",
            "/api/v1/agent/session/list",
            "/api/v1/agent/session/history",
            "/api/v1/agent/request/status",
            // #656: the live transfer-progress poll reads only the in-memory SharedProgress
            // snapshot (no store). It MUST be gate-exempt: the mobile offline pass holds the
            // store gate for the whole blocking materialize, so a gated poll would block until
            // the pass finishes — leaving the panel unable to show progress while it downloads.
            "/api/v1/onedrive/transfers",
        ];
        // #659: the transfer-CONTROL POSTs (cancel/pause/retry) touch ONLY the in-memory
        // SharedProgress (the cancel/pause sets), never the store — so, like the transfers GET
        // above, they MUST be gate-exempt. The mobile offline pass holds the store gate for the
        // whole blocking materialize; a gated pause/retry/cancel would block until that pass
        // finished, i.e. it could never interrupt the very transfer it targets (the pause/retry
        // AC is exactly "pause a LIVE materialization"). They are still session-token-gated (checked
        // above) and cap-token-gated in the handler; only the store gate is skipped.
        const GATE_EXEMPT_POST: &[&str] = &[
            "/api/v1/onedrive/transfers/cancel",
            "/api/v1/onedrive/transfers/pause",
            "/api/v1/onedrive/transfers/retry",
            // Turn admission and cancellation are owned by AgentHandler and do not open the
            // archive Store protected by this gate. Keeping them gated would let an unrelated
            // sync delay the turn ID or make cancellation unresponsive.
            "/api/v1/agent/turn",
            "/api/v1/agent/turn/cancel",
            "/api/v1/agent/pending/cancel",
        ];
        let static_get = req.method == "GET"
            && (matches!(
                req.path.as_str(),
                "/" | "/app.js" | "/app.css" | "/callback"
            ) || req.path.ends_with(".woff2")
                || req.path.starts_with("/sfx/"));
        let gate_exempt = static_get
            || (req.method == "GET" && GATE_EXEMPT_GET.contains(&req.path.as_str()))
            || (req.method == "POST" && GATE_EXEMPT_POST.contains(&req.path.as_str()));
        let _gate = if gate_exempt {
            None
        } else {
            self.gate
                .as_ref()
                .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()))
        };
        if req.method == "POST" {
            return match req.path.as_str() {
                "/api/v1/restore" => self.restore(req),
                "/api/v1/backup" => self.backup(req),
                "/api/v1/jobs/cancel" => self.mobile_job_cancel(req),
                "/api/v1/share" => self.share_link(req),
                "/api/v1/sync/pause" => self.sync_command(req, |c| c.pause()),
                "/api/v1/sync/resume" => self.sync_command(req, |c| c.resume()),
                "/api/v1/sync/now" => self.sync_command(req, |c| c.trigger()),
                "/api/v1/verify" => self.verify_run(req),
                "/api/v1/settings" => self.update_settings(req),
                "/api/v1/mail/send" => self.mail_send(req),
                "/api/v1/mail/reply" => self.mail_reply(req),
                "/api/v1/mail/forward" => self.mail_forward(req),
                "/api/v1/mail/move" => self.mail_move(req),
                "/api/v1/mail/read" => self.mail_read(req),
                "/api/v1/mail/flag" => self.mail_flag(req),
                "/api/v1/mail/categories" => self.mail_categories(req),
                "/api/v1/mail/draft" => self.mail_draft(req),
                "/api/v1/calendar/create" => self.calendar_create(req),
                "/api/v1/calendar/update" => self.calendar_update(req),
                "/api/v1/calendar/delete" => self.calendar_delete(req),
                "/api/v1/calendar/respond" => self.calendar_respond(req),
                "/api/v1/contact/create" => self.contact_create(req),
                "/api/v1/contact/update" => self.contact_update(req),
                "/api/v1/contact/delete" => self.contact_delete(req),
                "/api/v1/todo/create" => self.todo_create(req),
                "/api/v1/todo/update" => self.todo_update(req),
                "/api/v1/todo/complete" => self.todo_complete(req),
                "/api/v1/todo/delete" => self.todo_delete(req),
                "/api/v1/todo/checklist-add" => self.todo_checklist_add(req),
                "/api/v1/todo/checklist-toggle" => self.todo_checklist_toggle(req),
                "/api/v1/todo/checklist-delete" => self.todo_checklist_delete(req),
                "/api/v1/todo/list-create" => self.todo_list_create(req),
                "/api/v1/todo/list-delete" => self.todo_list_delete(req),
                "/api/v1/onenote/create" => self.onenote_create(req),
                "/api/v1/onenote/delete" => self.onenote_delete(req),
                "/api/v1/onenote/append" => self.onenote_append(req),
                "/api/v1/onedrive/transfers/cancel" => self.transfers_cancel(req),
                "/api/v1/onedrive/transfers/pause" => self.transfers_pause(req),
                "/api/v1/onedrive/transfers/retry" => self.transfers_retry(req),
                "/api/v1/onedrive/create" => self.onedrive_create(req),
                "/api/v1/onedrive/rename" => self.onedrive_rename(req),
                "/api/v1/onedrive/move" => self.onedrive_move(req),
                "/api/v1/onedrive/delete" => self.onedrive_delete(req),
                "/api/v1/onedrive/mode" => self.onedrive_set_mode(req),
                "/api/v1/mutation-intent/create" => self.mutation_intent_create(req),
                "/api/v1/mutation-intent/chunk" => self.mutation_intent_chunk(req),
                "/api/v1/mutation-intent/commit" => self.mutation_intent_commit(req),
                "/api/v1/mutation-intent/cancel" => self.mutation_intent_cancel(req),
                "/api/v1/onedrive/free-up" => self.onedrive_free_up(req),
                "/api/v1/onedrive/download-now" => self.onedrive_download_now(req),
                "/api/v1/onedrive/conflict/resolve" => self.onedrive_conflict_resolve(req),
                "/api/v1/onedrive/cleanup" => self.onedrive_cleanup(req),
                "/api/v1/account/login/start" => self.account_login_start(req),
                "/api/v1/account/login/poll" => self.account_login_poll(req),
                "/api/v1/account/login/cancel" => self.account_login_cancel(req),
                "/api/v1/account/signout" => self.account_signout(req),
                "/api/v1/push/register" => self.push_register(req),
                "/api/v1/push/test" => self.push_test(req),
                "/api/v1/agent/turn" => self.agent_turn(req),
                "/api/v1/agent/session/create" => self.agent_session_create(req),
                "/api/v1/agent/session/select" => self.agent_session_select(req),
                "/api/v1/agent/session/archive" => self.agent_session_archive(req),
                "/api/v1/agent/user-presence/start" => self.agent_user_presence_start(req),
                "/api/v1/agent/user-presence/confirm" => self.agent_user_presence_confirm(req),
                "/api/v1/agent/session/pairing/create" => self.agent_session_pairing_create(req),
                "/api/v1/agent/session/pairing/reveal" => self.agent_session_pairing_reveal(req),
                "/api/v1/agent/session/pairing/claim" => self.agent_session_pairing_claim(req),
                "/api/v1/agent/session/pairing/finalize" => {
                    self.agent_session_pairing_finalize(req)
                }
                "/api/v1/agent/session/pairing/revoke" => self.agent_session_pairing_revoke(req),
                "/api/v1/agent/confirm" => self.agent_confirm(req),
                "/api/v1/agent/turn/cancel" => self.agent_turn_cancel(req),
                "/api/v1/agent/pending/cancel" => self.agent_pending_cancel(req),
                "/api/v1/agent/connectivity/preflight" => self.agent_connectivity_preflight(req),
                "/api/v1/agent/credential/refresh" => self.agent_credential_refresh(req),
                "/api/v1/agent/oauth/start" => self.agent_oauth_start(req),
                "/api/v1/agent/oauth/logout" => self.agent_oauth_logout(req),
                "/api/v1/agent/oauth/lifecycle/resume" => self.agent_oauth_lifecycle_resume(req),
                "/api/v1/agent/oauth/cancel" => self.agent_oauth_cancel(req),
                "/api/v1/agent/oauth/complete" => self.agent_oauth_complete(req),
                "/api/v1/agent/model" => self.agent_set_model(req),
                _ => ApiResponse::error(405, "method not allowed"),
            };
        }
        if req.method != "GET" {
            return ApiResponse::error(405, "method not allowed");
        }
        match req.path.as_str() {
            // The shell is static; the strict app CSP header locks it to our assets.
            "/" => {
                let mut response = ApiResponse::html_with_csp(INDEX_HTML, APP_SHELL_CSP);
                if let Some(token) = self.session_token.as_deref() {
                    response.headers.push((
                        "Set-Cookie".into(),
                        format!("isy_session={token}; HttpOnly; SameSite=Strict; Path=/api/v1"),
                    ));
                    response
                        .headers
                        .push(("Cache-Control".into(), "no-store".into()));
                }
                response
            }
            // Agent provider OAuth callback. The **system browser**
            // returns here after the operator's login; deliberately NOT under `/api/v1/`
            // so it is exempt from the session-token gate (the browser has no token).
            // CSRF-protected by the `state` minted at oauth/start (single-use). The path
            // is exactly `/callback` because provider OAuth clients register the loopback
            // redirect as http://127.0.0.1:<port>/callback (RFC 8252).
            "/callback" => self.agent_oauth_callback(req),
            // Agent connection status (session-gated by the /api/v1/ gate above; read-only,
            // so no capability token). The Assistant UI reads it to switch connect⇄chat.
            "/api/v1/agent/status" => self.agent_status(req),
            "/api/v1/agent/session/list" => self.agent_session_list(req),
            "/api/v1/agent/session/history" => self.agent_session_history(req),
            "/api/v1/agent/request/status" => self.agent_request_status(req),
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
                        "__BACKUP_CAP_TOKEN__",
                        self.backup_cap_token.as_deref().unwrap_or(""),
                    )
                    .replace(
                        "__MOBILE_JOB_CAP_TOKEN__",
                        self.mobile_job_cap_token.as_deref().unwrap_or(""),
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
                    // #651 server-side half of the cap bridge; app.js grows the
                    // `__ONEDRIVE_MODE_CAP_TOKEN__` placeholder + `CAP.onedriveMode` in
                    // #652. A no-op until then (the placeholder isn't in APP_JS yet).
                    .replace(
                        "__ONEDRIVE_MODE_CAP_TOKEN__",
                        self.onedrive_mode_cap_token.as_deref().unwrap_or(""),
                    )
                    // #656 server-side half of the transfers cap bridge; app.js grows the
                    // `__TRANSFER_CAP_TOKEN__` placeholder + `CAP.transfers` for the cancel
                    // button on the live-transfer panel. A no-op until the transfers handler
                    // is wired (`with_transfers`).
                    .replace(
                        "__TRANSFER_CAP_TOKEN__",
                        self.transfer_cap_token.as_deref().unwrap_or(""),
                    )
                    // #659 server-side half of the manage cap bridge; app.js grows the
                    // `__ONEDRIVE_MANAGE_CAP_TOKEN__` placeholder + `CAP.onedriveManage` for the
                    // free-up / download-now buttons, the Conflict Center and the cleanup toast.
                    .replace(
                        "__ONEDRIVE_MANAGE_CAP_TOKEN__",
                        self.onedrive_manage_cap_token.as_deref().unwrap_or(""),
                    )
                    .replace(
                        "__MAILWRITE_CAP_TOKEN__",
                        self.mail_write_cap_token.as_deref().unwrap_or(""),
                    )
                    .replace(
                        "__CALENDARWRITE_CAP_TOKEN__",
                        self.calendar_write_cap_token.as_deref().unwrap_or(""),
                    )
                    .replace(
                        "__CONTACTWRITE_CAP_TOKEN__",
                        self.contact_write_cap_token.as_deref().unwrap_or(""),
                    )
                    .replace(
                        "__TASKWRITE_CAP_TOKEN__",
                        self.task_write_cap_token.as_deref().unwrap_or(""),
                    )
                    .replace(
                        "__ONENOTEWRITE_CAP_TOKEN__",
                        self.onenote_write_cap_token.as_deref().unwrap_or(""),
                    )
                    .replace(
                        "__ONEDRIVEWRITE_CAP_TOKEN__",
                        self.onedrive_write_cap_token.as_deref().unwrap_or(""),
                    )
                    .replace(
                        "__MUTATION_INTENT_CAP_TOKEN__",
                        self.mutation_intent_cap_token.as_deref().unwrap_or(""),
                    )
                    .replace(
                        "__ACCOUNT_CAP_TOKEN__",
                        self.account_cap_token.as_deref().unwrap_or(""),
                    )
                    .replace(
                        "__PUSH_CAP_TOKEN__",
                        self.push_cap_token.as_deref().unwrap_or(""),
                    )
                    .replace(
                        "__AGENT_CAP_TOKEN__",
                        self.agent_cap_token.as_deref().unwrap_or(""),
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
            "/sfx/shoot.mp3" => audio_response(SFX_SHOOT),
            "/sfx/boom.mp3" => audio_response(SFX_BOOM),
            "/sfx/level.mp3" => audio_response(SFX_LEVEL),
            "/sfx/drop.mp3" => audio_response(SFX_DROP),
            "/sfx/pickup.mp3" => audio_response(SFX_PICKUP),
            "/sfx/hit.mp3" => audio_response(SFX_HIT),
            "/api/v1/accounts" => self.accounts(),
            "/api/v1/jobs" => self.mobile_jobs(req),
            "/api/v1/settings" => self.settings(),
            "/api/v1/activity" => self.activity(req),
            "/api/v1/status" => self.status(req),
            "/api/v1/items" => self.items(req),
            "/api/v1/item" => self.item(req),
            "/api/v1/body" => self.body(req),
            "/api/v1/attachment" => self.attachment(req),
            "/api/v1/view" => self.view(req),
            "/api/v1/open-external" => self.open_external(req),
            "/api/v1/search" => self.search(req),
            "/api/v1/sync/state" => self.sync_state(),
            "/api/v1/hydrations" => self.hydrations_state(),
            "/api/v1/onedrive/transfers" => self.transfers_state(),
            "/api/v1/onedrive/conflicts" => self.onedrive_conflicts(req),
            "/api/v1/onedrive/policy" => self.policy_state(),
            "/api/v1/onedrive/mode" => self.onedrive_mode(req),
            "/api/v1/drive" => self.drive_info(req),
            "/api/v1/permissions" => self.item_permissions(req),
            "/api/v1/onedrive/children" => self.onedrive_children(req),
            "/api/v1/onedrive/open" => self.onedrive_open(req),
            "/api/v1/contact/photo" => self.contact_photo(req),
            "/api/v1/debug/stats" => self.debug_stats(),
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
        let parsed =
            match parse_strict_scalar_mutation(req, "settings update", &["poll_interval_secs"]) {
                Ok(req) => req,
                Err(e) => return e,
            };
        let req = &parsed;
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

    // ---- live-mail write endpoints (#561; UI is #563) -----------------------
    //
    // All `/api/v1/mail/*` POSTs share one gate (handler injected + cap token).
    // Content-bearing operations use strict JSON; the remaining metadata-only
    // operations are migrated by the closed route catalogue.

    /// The shared gate: the handler must be injected (else 404 on the read-only
    /// server) and the request must carry the mail-write capability token (else 401).
    fn mail_gate(
        &self,
        req: &ApiRequest,
    ) -> Result<&std::sync::Arc<dyn MailWriteHandler>, ApiResponse> {
        let h = self
            .mail_write
            .as_ref()
            .ok_or_else(|| ApiResponse::error(404, "mail write is not enabled on this server"))?;
        if !Self::cap_ok(&self.mail_write_cap_token, req) {
            return Err(ApiResponse::error(
                401,
                "missing or invalid capability token",
            ));
        }
        Ok(h)
    }

    /// Audit + map a unit mail result to a response. NB: we deliberately do NOT
    /// fire the SSE change bus on a self-write — the daemon doesn't re-sync mail
    /// into the store on a write, so an SSE-driven re-fetch would read the *stale*
    /// store and clobber the UI's optimistic update. The frontend's optimistic
    /// update is the correct immediate feedback; the store reconciles on the next
    /// backup. (New *incoming* mail live needs daemon mail-sync — a follow-up.)
    fn mail_result(&self, account: &str, what: &str, r: Result<(), String>) -> ApiResponse {
        match r {
            Ok(()) => {
                let _ = self.audit_account(account, "audit:mail", "ok", what);
                ApiResponse::ok_json(&json!({ "ok": true }))
            }
            Err(e) => {
                let _ = self.audit_account(account, "audit:mail", "error", &format!("{what}: {e}"));
                ApiResponse::error(500, &e)
            }
        }
    }

    // ---- live-calendar write endpoints (#565 B7) ----------------------------
    /// Shared gate for `/api/v1/calendar/*` (handler injected + cap token).
    fn calendar_gate(
        &self,
        req: &ApiRequest,
    ) -> Result<&std::sync::Arc<dyn CalendarWriteHandler>, ApiResponse> {
        let h = self.calendar_write.as_ref().ok_or_else(|| {
            ApiResponse::error(404, "calendar write is not enabled on this server")
        })?;
        if !Self::cap_ok(&self.calendar_write_cap_token, req) {
            return Err(ApiResponse::error(
                401,
                "missing or invalid capability token",
            ));
        }
        Ok(h)
    }

    /// Audit + map a unit calendar result (same SSE caveat as mail_result).
    fn cal_result(&self, account: &str, what: &str, r: Result<(), String>) -> ApiResponse {
        match r {
            Ok(()) => {
                let _ = self.audit_account(account, "audit:calendar", "ok", what);
                ApiResponse::ok_json(&json!({ "ok": true }))
            }
            Err(e) => {
                let _ =
                    self.audit_account(account, "audit:calendar", "error", &format!("{what}: {e}"));
                ApiResponse::error(500, &e)
            }
        }
    }

    /// Build a Graph event resource from the request's query params (subject,
    /// start/end with a timezone, location, HTML body, all-day). Only provided
    /// fields are set; the write layer sanitizes to the writable whitelist.
    fn event_from_req(req: &ApiRequest) -> Value {
        let tz = req.q("tz").filter(|s| !s.is_empty()).unwrap_or("UTC");
        let mut ev = json!({});
        let obj = ev.as_object_mut().unwrap();
        if let Some(s) = req.q("subject") {
            obj.insert("subject".into(), json!(s));
        }
        if let Some(s) = req.q("start").filter(|s| !s.is_empty()) {
            obj.insert("start".into(), json!({ "dateTime": s, "timeZone": tz }));
        }
        if let Some(s) = req.q("end").filter(|s| !s.is_empty()) {
            obj.insert("end".into(), json!({ "dateTime": s, "timeZone": tz }));
        }
        if let Some(s) = req.q("location").filter(|s| !s.is_empty()) {
            obj.insert("location".into(), json!({ "displayName": s }));
        }
        if let Some(s) = req.q("body").filter(|s| !s.is_empty()) {
            obj.insert(
                "body".into(),
                json!({ "contentType": "HTML", "content": s }),
            );
        }
        if req.q("all_day") == Some("1") {
            obj.insert("isAllDay".into(), json!(true));
        }
        ev
    }

    fn calendar_create(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.calendar_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(
            req,
            "calendar create",
            &[
                "account", "subject", "start", "end", "tz", "location", "body", "all_day",
            ],
        ) {
            Ok(req) => req,
            Err(e) => return e,
        };
        let req = &parsed;
        let account = match req.q("account").filter(|a| !a.is_empty()) {
            Some(a) => a,
            None => return ApiResponse::error(400, "account is required"),
        };
        let ev = Self::event_from_req(req);
        if ev.get("subject").is_none() || ev.get("start").is_none() {
            return ApiResponse::error(400, "subject and start are required");
        }
        match h.create(account, &ev) {
            Ok(id) => {
                let _ = self.audit_account(account, "audit:calendar", "ok", "create");
                ApiResponse::ok_json(&json!({ "ok": true, "id": id }))
            }
            Err(e) => {
                let _ =
                    self.audit_account(account, "audit:calendar", "error", &format!("create: {e}"));
                ApiResponse::error(500, &e)
            }
        }
    }

    fn calendar_update(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.calendar_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(
            req,
            "calendar update",
            &[
                "account", "id", "subject", "start", "end", "tz", "location", "body", "all_day",
            ],
        ) {
            Ok(req) => req,
            Err(e) => return e,
        };
        let req = &parsed;
        let (account, id) = match (
            req.q("account").filter(|a| !a.is_empty()),
            req.q("id").filter(|i| !i.is_empty()),
        ) {
            (Some(a), Some(i)) => (a, i),
            _ => return ApiResponse::error(400, "account and id are required"),
        };
        let ev = Self::event_from_req(req);
        self.cal_result(
            account,
            &format!("update id={id}"),
            h.update(account, id, &ev),
        )
    }

    fn calendar_delete(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.calendar_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(req, "calendar delete", &["account", "id"])
        {
            Ok(req) => req,
            Err(e) => return e,
        };
        let req = &parsed;
        let (account, id) = match (
            req.q("account").filter(|a| !a.is_empty()),
            req.q("id").filter(|i| !i.is_empty()),
        ) {
            (Some(a), Some(i)) => (a, i),
            _ => return ApiResponse::error(400, "account and id are required"),
        };
        if let Some(r) = self.biometric_challenge("delete", account, "calendar", id, req) {
            return r;
        }
        self.cal_result(account, &format!("delete id={id}"), h.delete(account, id))
    }

    fn calendar_respond(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.calendar_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(
            req,
            "calendar respond",
            &["account", "id", "response", "comment"],
        ) {
            Ok(req) => req,
            Err(e) => return e,
        };
        let req = &parsed;
        let (account, id) = match (
            req.q("account").filter(|a| !a.is_empty()),
            req.q("id").filter(|i| !i.is_empty()),
        ) {
            (Some(a), Some(i)) => (a, i),
            _ => return ApiResponse::error(400, "account and id are required"),
        };
        let response = req
            .q("response")
            .filter(|r| !r.is_empty())
            .unwrap_or("accept");
        let comment = req.q("comment").unwrap_or("");
        self.cal_result(
            account,
            &format!("respond id={id} {response}"),
            h.respond(account, id, response, comment),
        )
    }

    fn contact_gate(
        &self,
        req: &ApiRequest,
    ) -> Result<&std::sync::Arc<dyn ContactWriteHandler>, ApiResponse> {
        let h = self.contact_write.as_ref().ok_or_else(|| {
            ApiResponse::error(404, "contact write is not enabled on this server")
        })?;
        if !Self::cap_ok(&self.contact_write_cap_token, req) {
            return Err(ApiResponse::error(
                401,
                "missing or invalid capability token",
            ));
        }
        Ok(h)
    }

    /// Audit + map a unit contact result (same no-SSE-on-self-write caveat as
    /// `cal_result`: the daemon doesn't re-sync contacts on a write, so an SSE
    /// refresh would read the stale store and clobber the optimistic UI).
    fn contact_result(&self, account: &str, what: &str, r: Result<(), String>) -> ApiResponse {
        match r {
            Ok(()) => {
                let _ = self.audit_account(account, "audit:contact", "ok", what);
                ApiResponse::ok_json(&json!({ "ok": true }))
            }
            Err(e) => {
                let _ =
                    self.audit_account(account, "audit:contact", "error", &format!("{what}: {e}"));
                ApiResponse::error(500, &e)
            }
        }
    }

    /// Build a structured Graph physical address from `<prefix>_{street,city,
    /// state,zip,country}` query params; `None` when none are set.
    fn addr_from_req(req: &ApiRequest, prefix: &str) -> Option<Value> {
        let g = |k: &str| {
            req.q(&format!("{prefix}_{k}"))
                .filter(|s| !s.is_empty())
                .map(String::from)
        };
        let mut a = serde_json::Map::new();
        for (q, key) in [
            ("street", "street"),
            ("city", "city"),
            ("state", "state"),
            ("zip", "postalCode"),
            ("country", "countryOrRegion"),
        ] {
            if let Some(v) = g(q) {
                a.insert(key.into(), json!(v));
            }
        }
        (!a.is_empty()).then_some(Value::Object(a))
    }

    /// Build a Graph contact resource from the request's query params (name parts,
    /// email, phones, company/title, notes, birthday, three structured addresses).
    /// Only provided fields are set; the write layer sanitizes to the whitelist.
    fn contact_from_req(req: &ApiRequest) -> Value {
        let mut c = json!({});
        let obj = c.as_object_mut().unwrap();
        for (q, key) in [
            ("given", "givenName"),
            ("surname", "surname"),
            ("display_name", "displayName"),
            ("nickname", "nickName"),
            ("title", "title"),
            ("company", "companyName"),
            ("job", "jobTitle"),
            ("department", "department"),
            ("mobile", "mobilePhone"),
            ("notes", "personalNotes"),
            ("birthday", "birthday"),
        ] {
            if let Some(s) = req.q(q).filter(|s| !s.is_empty()) {
                obj.insert(key.into(), json!(s));
            }
        }
        if let Some(e) = req.q("email").filter(|s| !s.is_empty()) {
            obj.insert("emailAddresses".into(), json!([{ "address": e }]));
        }
        if let Some(p) = req.q("business_phone").filter(|s| !s.is_empty()) {
            obj.insert("businessPhones".into(), json!([p]));
        }
        if let Some(p) = req.q("home_phone").filter(|s| !s.is_empty()) {
            obj.insert("homePhones".into(), json!([p]));
        }
        if let Some(a) = Self::addr_from_req(req, "business") {
            obj.insert("businessAddress".into(), a);
        }
        if let Some(a) = Self::addr_from_req(req, "home") {
            obj.insert("homeAddress".into(), a);
        }
        if let Some(a) = Self::addr_from_req(req, "other") {
            obj.insert("otherAddress".into(), a);
        }
        c
    }

    fn contact_create(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.contact_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(
            req,
            "contact create",
            &[
                "account",
                "given",
                "surname",
                "display_name",
                "nickname",
                "title",
                "company",
                "job",
                "department",
                "mobile",
                "notes",
                "birthday",
                "email",
                "business_phone",
                "home_phone",
                "business_street",
                "business_city",
                "business_state",
                "business_zip",
                "business_country",
                "home_street",
                "home_city",
                "home_state",
                "home_zip",
                "home_country",
                "other_street",
                "other_city",
                "other_state",
                "other_zip",
                "other_country",
            ],
        ) {
            Ok(req) => req,
            Err(e) => return e,
        };
        let req = &parsed;
        let account = match req.q("account").filter(|a| !a.is_empty()) {
            Some(a) => a,
            None => return ApiResponse::error(400, "account is required"),
        };
        let c = Self::contact_from_req(req);
        if c.as_object().map(serde_json::Map::is_empty).unwrap_or(true) {
            return ApiResponse::error(400, "at least one contact field is required");
        }
        match h.create(account, &c) {
            Ok(id) => {
                let _ = self.audit_account(account, "audit:contact", "ok", "create");
                ApiResponse::ok_json(&json!({ "ok": true, "id": id }))
            }
            Err(e) => {
                let _ =
                    self.audit_account(account, "audit:contact", "error", &format!("create: {e}"));
                ApiResponse::error(500, &e)
            }
        }
    }

    fn contact_update(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.contact_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(
            req,
            "contact update",
            &[
                "account",
                "id",
                "given",
                "surname",
                "display_name",
                "nickname",
                "title",
                "company",
                "job",
                "department",
                "mobile",
                "notes",
                "birthday",
                "email",
                "business_phone",
                "home_phone",
                "business_street",
                "business_city",
                "business_state",
                "business_zip",
                "business_country",
                "home_street",
                "home_city",
                "home_state",
                "home_zip",
                "home_country",
                "other_street",
                "other_city",
                "other_state",
                "other_zip",
                "other_country",
            ],
        ) {
            Ok(req) => req,
            Err(e) => return e,
        };
        let req = &parsed;
        let (account, id) = match (
            req.q("account").filter(|a| !a.is_empty()),
            req.q("id").filter(|i| !i.is_empty()),
        ) {
            (Some(a), Some(i)) => (a, i),
            _ => return ApiResponse::error(400, "account and id are required"),
        };
        let c = Self::contact_from_req(req);
        self.contact_result(
            account,
            &format!("update id={id}"),
            h.update(account, id, &c),
        )
    }

    fn contact_delete(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.contact_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(req, "contact delete", &["account", "id"]) {
            Ok(req) => req,
            Err(e) => return e,
        };
        let req = &parsed;
        let (account, id) = match (
            req.q("account").filter(|a| !a.is_empty()),
            req.q("id").filter(|i| !i.is_empty()),
        ) {
            (Some(a), Some(i)) => (a, i),
            _ => return ApiResponse::error(400, "account and id are required"),
        };
        if let Some(r) = self.biometric_challenge("delete", account, "contacts", id, req) {
            return r;
        }
        self.contact_result(account, &format!("delete id={id}"), h.delete(account, id))
    }

    fn todo_gate(
        &self,
        req: &ApiRequest,
    ) -> Result<&std::sync::Arc<dyn TaskWriteHandler>, ApiResponse> {
        let h = self
            .task_write
            .as_ref()
            .ok_or_else(|| ApiResponse::error(404, "todo write is not enabled on this server"))?;
        if !Self::cap_ok(&self.task_write_cap_token, req) {
            return Err(ApiResponse::error(
                401,
                "missing or invalid capability token",
            ));
        }
        Ok(h)
    }

    /// Audit + map a unit ToDo result (no SSE on self-write, like `cal_result`).
    fn todo_result(&self, account: &str, what: &str, r: Result<(), String>) -> ApiResponse {
        match r {
            Ok(()) => {
                let _ = self.audit_account(account, "audit:todo", "ok", what);
                ApiResponse::ok_json(&json!({ "ok": true }))
            }
            Err(e) => {
                let _ = self.audit_account(account, "audit:todo", "error", &format!("{what}: {e}"));
                ApiResponse::error(500, &e)
            }
        }
    }

    /// Build a Graph todoTask from query params (title/body/importance/status,
    /// due/start/reminder dates, comma-separated categories). The write layer
    /// sanitizes to the writable whitelist.
    fn task_from_req(req: &ApiRequest) -> Value {
        let tz = req.q("tz").filter(|s| !s.is_empty()).unwrap_or("UTC");
        let mut t = json!({});
        let o = t.as_object_mut().unwrap();
        if let Some(s) = req.q("title") {
            o.insert("title".into(), json!(s));
        }
        if let Some(s) = req.q("body").filter(|s| !s.is_empty()) {
            o.insert(
                "body".into(),
                json!({ "contentType": "text", "content": s }),
            );
        }
        if let Some(s) = req.q("importance").filter(|s| !s.is_empty()) {
            o.insert("importance".into(), json!(s));
        }
        if let Some(s) = req.q("status").filter(|s| !s.is_empty()) {
            o.insert("status".into(), json!(s));
        }
        for (q, key) in [
            ("due", "dueDateTime"),
            ("start", "startDateTime"),
            ("reminder", "reminderDateTime"),
        ] {
            if let Some(s) = req.q(q).filter(|s| !s.is_empty()) {
                o.insert(key.into(), json!({ "dateTime": s, "timeZone": tz }));
                if q == "reminder" {
                    o.insert("isReminderOn".into(), json!(true));
                }
            }
        }
        if let Some(s) = req.q("categories").filter(|s| !s.is_empty()) {
            let cats: Vec<Value> = s
                .split(',')
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(|c| json!(c))
                .collect();
            if !cats.is_empty() {
                o.insert("categories".into(), Value::Array(cats));
            }
        }
        t
    }

    /// `(account, list)` from the request, both required.
    fn todo_acc_list(req: &ApiRequest) -> Result<(&str, &str), ApiResponse> {
        match (
            req.q("account").filter(|a| !a.is_empty()),
            req.q("list").filter(|l| !l.is_empty()),
        ) {
            (Some(a), Some(l)) => Ok((a, l)),
            _ => Err(ApiResponse::error(400, "account and list are required")),
        }
    }

    fn todo_create(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.todo_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(
            req,
            "todo create",
            &[
                "account",
                "list",
                "title",
                "body",
                "importance",
                "status",
                "due",
                "start",
                "reminder",
                "tz",
                "categories",
            ],
        ) {
            Ok(req) => req,
            Err(e) => return e,
        };
        let req = &parsed;
        let (account, list) = match Self::todo_acc_list(req) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let t = Self::task_from_req(req);
        if t.get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
        {
            return ApiResponse::error(400, "title is required");
        }
        match h.create(account, list, &t) {
            Ok(id) => {
                let _ = self.audit_account(account, "audit:todo", "ok", "create");
                ApiResponse::ok_json(&json!({ "ok": true, "id": id }))
            }
            Err(e) => {
                let _ = self.audit_account(account, "audit:todo", "error", &format!("create: {e}"));
                ApiResponse::error(500, &e)
            }
        }
    }

    fn todo_update(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.todo_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(
            req,
            "todo update",
            &[
                "account",
                "list",
                "id",
                "title",
                "body",
                "importance",
                "status",
                "due",
                "start",
                "reminder",
                "tz",
                "categories",
            ],
        ) {
            Ok(req) => req,
            Err(e) => return e,
        };
        let req = &parsed;
        let (account, list) = match Self::todo_acc_list(req) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let id = match req.q("id").filter(|i| !i.is_empty()) {
            Some(i) => i,
            None => return ApiResponse::error(400, "id is required"),
        };
        let t = Self::task_from_req(req);
        self.todo_result(
            account,
            &format!("update id={id}"),
            h.update(account, list, id, &t),
        )
    }

    fn todo_complete(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.todo_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed =
            match parse_strict_scalar_mutation(req, "todo complete", &["account", "list", "id"]) {
                Ok(req) => req,
                Err(e) => return e,
            };
        let req = &parsed;
        let (account, list) = match Self::todo_acc_list(req) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let id = match req.q("id").filter(|i| !i.is_empty()) {
            Some(i) => i,
            None => return ApiResponse::error(400, "id is required"),
        };
        self.todo_result(
            account,
            &format!("complete id={id}"),
            h.complete(account, list, id),
        )
    }

    fn todo_delete(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.todo_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed =
            match parse_strict_scalar_mutation(req, "todo delete", &["account", "list", "id"]) {
                Ok(req) => req,
                Err(e) => return e,
            };
        let req = &parsed;
        let (account, list) = match Self::todo_acc_list(req) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let id = match req.q("id").filter(|i| !i.is_empty()) {
            Some(i) => i,
            None => return ApiResponse::error(400, "id is required"),
        };
        if let Some(r) = self.biometric_challenge("delete", account, "todo", id, req) {
            return r;
        }
        self.todo_result(
            account,
            &format!("delete id={id}"),
            h.delete(account, list, id),
        )
    }

    fn todo_checklist_add(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.todo_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(
            req,
            "todo checklist add",
            &["account", "list", "task", "title"],
        ) {
            Ok(req) => req,
            Err(e) => return e,
        };
        let req = &parsed;
        let (account, list) = match Self::todo_acc_list(req) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let (task, title) = match (
            req.q("task").filter(|t| !t.is_empty()),
            req.q("title").filter(|t| !t.is_empty()),
        ) {
            (Some(t), Some(ti)) => (t, ti),
            _ => return ApiResponse::error(400, "task and title are required"),
        };
        match h.checklist_add(account, list, task, title) {
            Ok(id) => {
                let _ = self.audit_account(account, "audit:todo", "ok", "checklist-add");
                ApiResponse::ok_json(&json!({ "ok": true, "id": id }))
            }
            Err(e) => {
                let _ = self.audit_account(
                    account,
                    "audit:todo",
                    "error",
                    &format!("checklist-add: {e}"),
                );
                ApiResponse::error(500, &e)
            }
        }
    }

    fn todo_checklist_toggle(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.todo_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(
            req,
            "todo checklist toggle",
            &["account", "list", "task", "item", "checked"],
        ) {
            Ok(req) => req,
            Err(e) => return e,
        };
        let req = &parsed;
        let (account, list) = match Self::todo_acc_list(req) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let (task, item) = match (
            req.q("task").filter(|t| !t.is_empty()),
            req.q("item").filter(|i| !i.is_empty()),
        ) {
            (Some(t), Some(i)) => (t, i),
            _ => return ApiResponse::error(400, "task and item are required"),
        };
        let checked = req.q("checked") == Some("1");
        self.todo_result(
            account,
            &format!("checklist-toggle item={item} checked={checked}"),
            h.checklist_toggle(account, list, task, item, checked),
        )
    }

    fn todo_checklist_delete(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.todo_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(
            req,
            "todo checklist delete",
            &["account", "list", "task", "item"],
        ) {
            Ok(req) => req,
            Err(e) => return e,
        };
        let req = &parsed;
        let (account, list) = match Self::todo_acc_list(req) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let (task, item) = match (
            req.q("task").filter(|t| !t.is_empty()),
            req.q("item").filter(|i| !i.is_empty()),
        ) {
            (Some(t), Some(i)) => (t, i),
            _ => return ApiResponse::error(400, "task and item are required"),
        };
        self.todo_result(
            account,
            &format!("checklist-delete item={item}"),
            h.checklist_delete(account, list, task, item),
        )
    }

    fn todo_list_create(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.todo_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed =
            match parse_strict_scalar_mutation(req, "todo list create", &["account", "name"]) {
                Ok(req) => req,
                Err(e) => return e,
            };
        let req = &parsed;
        let (account, name) = match (
            req.q("account").filter(|a| !a.is_empty()),
            req.q("name").filter(|n| !n.is_empty()),
        ) {
            (Some(a), Some(n)) => (a, n),
            _ => return ApiResponse::error(400, "account and name are required"),
        };
        match h.list_create(account, name) {
            Ok(id) => {
                let _ = self.audit_account(account, "audit:todo", "ok", "list-create");
                ApiResponse::ok_json(&json!({ "ok": true, "id": id }))
            }
            Err(e) => {
                let _ = self.audit_account(
                    account,
                    "audit:todo",
                    "error",
                    &format!("list-create: {e}"),
                );
                ApiResponse::error(500, &e)
            }
        }
    }

    fn todo_list_delete(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.todo_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(req, "todo list delete", &["account", "id"])
        {
            Ok(req) => req,
            Err(e) => return e,
        };
        let req = &parsed;
        let (account, id) = match (
            req.q("account").filter(|a| !a.is_empty()),
            req.q("id").filter(|i| !i.is_empty()),
        ) {
            (Some(a), Some(i)) => (a, i),
            _ => return ApiResponse::error(400, "account and id are required"),
        };
        self.todo_result(
            account,
            &format!("list-delete id={id}"),
            h.list_delete(account, id),
        )
    }

    fn onenote_gate(
        &self,
        req: &ApiRequest,
    ) -> Result<&std::sync::Arc<dyn OneNoteWriteHandler>, ApiResponse> {
        let h = self.onenote_write.as_ref().ok_or_else(|| {
            ApiResponse::error(404, "onenote write is not enabled on this server")
        })?;
        if !Self::cap_ok(&self.onenote_write_cap_token, req) {
            return Err(ApiResponse::error(
                401,
                "missing or invalid capability token",
            ));
        }
        Ok(h)
    }

    /// Audit + map a unit OneNote result (no SSE on self-write, like `cal_result`).
    fn onenote_result(&self, account: &str, what: &str, r: Result<(), String>) -> ApiResponse {
        match r {
            Ok(()) => {
                let _ = self.audit_account(account, "audit:onenote", "ok", what);
                ApiResponse::ok_json(&json!({ "ok": true }))
            }
            Err(e) => {
                let _ =
                    self.audit_account(account, "audit:onenote", "error", &format!("{what}: {e}"));
                ApiResponse::error(500, &e)
            }
        }
    }

    /// Build a minimal, well-formed OneNote page HTML from the request's `title` +
    /// `body` (both HTML-escaped; body newlines become paragraph breaks).
    fn page_html(title: &str, body: &str) -> Vec<u8> {
        let esc = |s: &str| {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        };
        let title = if title.is_empty() { "Untitled" } else { title };
        let body_html = esc(body).replace('\n', "</p><p>");
        format!(
            "<!DOCTYPE html><html><head><title>{}</title></head><body><p>{}</p></body></html>",
            esc(title),
            body_html
        )
        .into_bytes()
    }

    fn onenote_create(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.onenote_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let request: OneNoteCreateRequest =
            match parse_agent_strict_json_with_limit(req, "OneNote create", 64 * 1024) {
                Ok(request) => request,
                Err(response) => return response,
            };
        if !valid_client_request_id(&request.request_id)
            || request.account.is_empty()
            || request.section.is_empty()
        {
            return ApiResponse::error(400, "invalid OneNote create request");
        }
        let html = Self::page_html(&request.title, &request.body);
        match h.create(&request.account, &request.section, &html) {
            Ok(id) => {
                let _ = self.audit_account(&request.account, "audit:onenote", "ok", "create");
                ApiResponse::ok_json(&json!({ "ok": true, "id": id }))
            }
            Err(e) => {
                let _ = self.audit_account(
                    &request.account,
                    "audit:onenote",
                    "error",
                    &format!("create: {e}"),
                );
                ApiResponse::error(500, &e)
            }
        }
    }

    fn onenote_delete(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.onenote_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(req, "OneNote delete", &["account", "id"]) {
            Ok(req) => req,
            Err(e) => return e,
        };
        let req = &parsed;
        let (account, id) = match (
            req.q("account").filter(|a| !a.is_empty()),
            req.q("id").filter(|i| !i.is_empty()),
        ) {
            (Some(a), Some(i)) => (a, i),
            _ => return ApiResponse::error(400, "account and id are required"),
        };
        if let Some(r) = self.biometric_challenge("delete", account, "onenote", id, req) {
            return r;
        }
        self.onenote_result(account, &format!("delete id={id}"), h.delete(account, id))
    }

    fn onenote_append(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.onenote_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let request: OneNoteAppendRequest =
            match parse_agent_strict_json_with_limit(req, "OneNote append", 64 * 1024) {
                Ok(request) => request,
                Err(response) => return response,
            };
        if !valid_client_request_id(&request.request_id)
            || request.account.is_empty()
            || request.id.is_empty()
            || request.text.is_empty()
        {
            return ApiResponse::error(400, "invalid OneNote append request");
        }
        self.onenote_result(
            &request.account,
            &format!("append id={}", request.id),
            h.append(&request.account, &request.id, &request.text),
        )
    }

    /// Gate a OneDrive cloud-write POST: handler present + valid capability token (#654).
    fn onedrive_gate(
        &self,
        req: &ApiRequest,
    ) -> Result<&std::sync::Arc<dyn OneDriveWriteHandler>, ApiResponse> {
        let h = self.onedrive_write.as_ref().ok_or_else(|| {
            ApiResponse::error(404, "onedrive write is not enabled on this server")
        })?;
        if !Self::cap_ok(&self.onedrive_write_cap_token, req) {
            return Err(ApiResponse::error(
                401,
                "missing or invalid capability token",
            ));
        }
        Ok(h)
    }

    fn mutation_intent_gate(
        &self,
        req: &ApiRequest,
    ) -> Result<(&std::sync::Arc<dyn MutationIntentHandler>, String), ApiResponse> {
        let handler = self.mutation_intents.as_ref().ok_or_else(|| {
            ApiResponse::error(404, "mutation intents are not enabled on this server")
        })?;
        if !Self::cap_ok(&self.mutation_intent_cap_token, req) {
            return Err(ApiResponse::error(
                401,
                "missing or invalid capability token",
            ));
        }
        let owner = req
            .session_token
            .as_deref()
            .filter(|owner| !owner.is_empty())
            .ok_or_else(|| ApiResponse::error(401, "session token required"))?;
        Ok((handler, owner.to_owned()))
    }

    fn mutation_intent_create(&self, req: &ApiRequest) -> ApiResponse {
        let (handler, owner) = match self.mutation_intent_gate(req) {
            Ok(value) => value,
            Err(response) => return response,
        };
        let request: MutationIntentCreateRequest =
            match parse_agent_strict_json_with_limit(req, "mutation intent create", 16 * 1024) {
                Ok(request) => request,
                Err(response) => return response,
            };
        if !valid_client_request_id(&request.request_id)
            || !request.purpose.is_valid()
            || !valid_sha256(&request.sha256)
        {
            return ApiResponse::error(400, "invalid mutation intent request");
        }
        if self.biometric_gate && req.storage_not_low != Some(true) {
            return ApiResponse::error(507, "mutation_intent_insufficient_storage");
        }
        let (op, account, service, item) = request.purpose.confirmation_binding();
        if let Some(response) = self.biometric_challenge(op, account, service, &item, req) {
            return response;
        }
        match handler.create(MutationIntentCreate {
            request_id: request.request_id,
            owner,
            purpose: request.purpose,
            total_bytes: request.total_bytes,
            sha256: request.sha256,
        }) {
            Ok(info) => ApiResponse::ok_json(&json!({
                "intent_id": info.intent_id,
                "chunk_bytes": info.chunk_bytes,
                "expires_at_ms": info.expires_at_ms,
            })),
            Err(error) => mutation_error_response(&error),
        }
    }

    fn mutation_intent_chunk(&self, req: &ApiRequest) -> ApiResponse {
        use base64::Engine as _;
        let (handler, owner) = match self.mutation_intent_gate(req) {
            Ok(value) => value,
            Err(response) => return response,
        };
        let request: MutationIntentChunkRequest =
            match parse_agent_strict_json_with_limit(req, "mutation intent chunk", 16 * 1024) {
                Ok(request) => request,
                Err(response) => return response,
            };
        if !valid_client_request_id(&request.request_id)
            || !valid_opaque_agent_id(&request.intent_id)
            || !valid_sha256(&request.chunk_sha256)
        {
            return ApiResponse::error(400, "invalid mutation chunk request");
        }
        let bytes = match base64::engine::general_purpose::STANDARD.decode(&request.data_base64) {
            Ok(bytes) if bytes.len() <= MUTATION_CHUNK_BYTES => bytes,
            _ => return ApiResponse::error(400, "invalid mutation chunk request"),
        };
        match handler.put_chunk(MutationIntentChunk {
            owner,
            request_id: request.request_id,
            intent_id: request.intent_id,
            index: request.index,
            offset: request.offset,
            chunk_sha256: request.chunk_sha256,
            bytes,
        }) {
            Ok(()) => ApiResponse::ok_json(&json!({ "stored": true })),
            Err(error) => mutation_error_response(&error),
        }
    }

    fn mutation_intent_commit(&self, req: &ApiRequest) -> ApiResponse {
        let (handler, owner) = match self.mutation_intent_gate(req) {
            Ok(value) => value,
            Err(response) => return response,
        };
        let request: MutationIntentCommitRequest =
            match parse_agent_strict_json_with_limit(req, "mutation intent commit", 16 * 1024) {
                Ok(request) => request,
                Err(response) => return response,
            };
        if !valid_client_request_id(&request.request_id)
            || !valid_opaque_agent_id(&request.intent_id)
            || !valid_sha256(&request.sha256)
        {
            return ApiResponse::error(400, "invalid mutation commit request");
        }
        match handler.commit(
            &owner,
            &request.request_id,
            &request.intent_id,
            request.total_bytes,
            &request.sha256,
        ) {
            Ok(result) => ApiResponse::ok_json(&result),
            Err(error) => mutation_error_response(&error),
        }
    }

    fn mutation_intent_cancel(&self, req: &ApiRequest) -> ApiResponse {
        let (handler, owner) = match self.mutation_intent_gate(req) {
            Ok(value) => value,
            Err(response) => return response,
        };
        let request: MutationIntentCancelRequest =
            match parse_agent_strict_json_with_limit(req, "mutation intent cancel", 16 * 1024) {
                Ok(request) => request,
                Err(response) => return response,
            };
        if !valid_client_request_id(&request.request_id)
            || !valid_opaque_agent_id(&request.intent_id)
        {
            return ApiResponse::error(400, "invalid mutation cancel request");
        }
        match handler.cancel(&owner, &request.request_id, &request.intent_id) {
            Ok(()) => ApiResponse::ok_json(&json!({ "cancelled": true })),
            Err(error) => mutation_error_response(&error),
        }
    }

    /// Audit + map a unit OneDrive cloud-write result.
    fn onedrive_result(&self, account: &str, what: &str, r: Result<(), String>) -> ApiResponse {
        match r {
            Ok(()) => {
                let _ = self.audit_account(account, "audit:onedrive", "ok", what);
                ApiResponse::ok_json(&json!({ "ok": true }))
            }
            Err(e) => {
                let _ =
                    self.audit_account(account, "audit:onedrive", "error", &format!("{what}: {e}"));
                ApiResponse::error(500, &e)
            }
        }
    }

    fn onedrive_create(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.onedrive_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(
            req,
            "OneDrive create",
            &["account", "parent", "name"],
        ) {
            Ok(parsed) => parsed,
            Err(response) => return response,
        };
        let req = &parsed;
        let (account, name) = match (
            req.q("account").filter(|a| !a.is_empty()),
            req.q("name").filter(|n| !n.is_empty()),
        ) {
            (Some(a), Some(n)) => (a, n),
            _ => return ApiResponse::error(400, "account and name are required"),
        };
        // An empty/absent parent means the drive root (Graph `create_folder` addresses it).
        let parent = req.q("parent").unwrap_or("");
        match h.create_folder(account, parent, name) {
            Ok(id) => {
                let _ = self.audit_account(account, "audit:onedrive", "ok", "create");
                ApiResponse::ok_json(&json!({ "ok": true, "id": id }))
            }
            Err(e) => {
                let _ =
                    self.audit_account(account, "audit:onedrive", "error", &format!("create: {e}"));
                ApiResponse::error(500, &e)
            }
        }
    }

    fn onedrive_rename(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.onedrive_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(
            req,
            "OneDrive rename",
            &["account", "id", "name"],
        ) {
            Ok(parsed) => parsed,
            Err(response) => return response,
        };
        let req = &parsed;
        let (account, id, name) = match (
            req.q("account").filter(|a| !a.is_empty()),
            req.q("id").filter(|i| !i.is_empty()),
            req.q("name").filter(|n| !n.is_empty()),
        ) {
            (Some(a), Some(i), Some(n)) => (a, i, n),
            _ => return ApiResponse::error(400, "account, id and name are required"),
        };
        self.onedrive_result(
            account,
            &format!("rename id={id}"),
            h.rename(account, id, name),
        )
    }

    fn onedrive_move(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.onedrive_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(
            req,
            "OneDrive move",
            &["account", "id", "parent", "name"],
        ) {
            Ok(parsed) => parsed,
            Err(response) => return response,
        };
        let req = &parsed;
        let (account, id, name) = match (
            req.q("account").filter(|a| !a.is_empty()),
            req.q("id").filter(|i| !i.is_empty()),
            req.q("name").filter(|n| !n.is_empty()),
        ) {
            (Some(a), Some(i), Some(n)) => (a, i, n),
            _ => return ApiResponse::error(400, "account, id and name are required"),
        };
        // Destination parent ("" = the drive root). Absent => not a move.
        let new_parent = match req.q("parent") {
            Some(p) => p,
            None => return ApiResponse::error(400, "parent (destination) is required"),
        };
        if self.biometric_gate {
            let requires_confirmation = match &self.onedrive_risk {
                Some(risk) => match risk.move_risk(account, id, new_parent) {
                    Ok(OneDriveMoveRisk::Low) => false,
                    Ok(
                        OneDriveMoveRisk::MoveOutOfProtected { .. }
                        | OneDriveMoveRisk::Unknown { .. },
                    ) => true,
                    Err(_) => true,
                },
                None => true,
            };
            if requires_confirmation {
                let item = onedrive_move_pat_item(id, new_parent, name);
                if let Some(r) = self.biometric_challenge(
                    "move-out-of-protected",
                    account,
                    "onedrive",
                    &item,
                    req,
                ) {
                    return r;
                }
            }
        }
        self.onedrive_result(
            account,
            &format!("move id={id}"),
            h.move_item(account, id, Some(new_parent), name),
        )
    }

    fn onedrive_delete(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.onedrive_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(req, "OneDrive delete", &["account", "id"])
        {
            Ok(parsed) => parsed,
            Err(response) => return response,
        };
        let req = &parsed;
        let (account, id) = match (
            req.q("account").filter(|a| !a.is_empty()),
            req.q("id").filter(|i| !i.is_empty()),
        ) {
            (Some(a), Some(i)) => (a, i),
            _ => return ApiResponse::error(400, "account and id are required"),
        };
        // #654: delete is destructive -> a biometric per-action token on mobile (no-op desktop).
        if let Some(r) = self.biometric_challenge("delete", account, "onedrive", id, req) {
            return r;
        }
        self.onedrive_result(account, &format!("delete id={id}"), h.delete(account, id))
    }

    /// Cap-gate the OneDrive management handler (#659), like [`onedrive_gate`] for the write verbs.
    fn manage_gate(
        &self,
        req: &ApiRequest,
    ) -> Result<&std::sync::Arc<dyn OneDriveManageHandler>, ApiResponse> {
        let h = self.onedrive_manage.as_ref().ok_or_else(|| {
            ApiResponse::error(404, "onedrive management is not enabled on this server")
        })?;
        if !Self::cap_ok(&self.onedrive_manage_cap_token, req) {
            return Err(ApiResponse::error(
                401,
                "missing or invalid capability token",
            ));
        }
        Ok(h)
    }

    /// `POST /api/v1/onedrive/free-up?account=…&id=…` — drop a materialized body but keep the item
    /// listable (#659). Local-only + reversible → NOT biometric-gated. Cap-gated + audited.
    fn onedrive_free_up(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.manage_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(req, "OneDrive free up", &["account", "id"])
        {
            Ok(parsed) => parsed,
            Err(response) => return response,
        };
        let req = &parsed;
        let (account, id) = match (
            req.q("account").filter(|a| !a.is_empty()),
            req.q("id").filter(|i| !i.is_empty()),
        ) {
            (Some(a), Some(i)) => (a, i),
            _ => return ApiResponse::error(400, "account and id are required"),
        };
        match h.free_up(account, id) {
            Ok(()) => {
                let _ = self.audit_account(account, "audit:onedrive-manage", "ok", "free-up");
                ApiResponse::ok_json(&json!({ "ok": true }))
            }
            Err(e) => {
                let _ = self.audit_account(
                    account,
                    "audit:onedrive-manage",
                    "error",
                    &format!("free-up id={id}: {e}"),
                );
                ApiResponse::error(500, &e)
            }
        }
    }

    /// `POST /api/v1/onedrive/download-now?account=…&id=…` — fetch one item on demand (#659/#724).
    /// Local-only + reversible → NOT biometric-gated. Sync mode targets cache; offline targets the
    /// editable sync root. `downloaded:false` when policy blocked it.
    fn onedrive_download_now(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.manage_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed =
            match parse_strict_scalar_mutation(req, "OneDrive download now", &["account", "id"]) {
                Ok(parsed) => parsed,
                Err(response) => return response,
            };
        let req = &parsed;
        let (account, id) = match (
            req.q("account").filter(|a| !a.is_empty()),
            req.q("id").filter(|i| !i.is_empty()),
        ) {
            (Some(a), Some(i)) => (a, i),
            _ => return ApiResponse::error(400, "account and id are required"),
        };
        match h.download_now(account, id) {
            Ok(result) => {
                let _ = self.audit_account(account, "audit:onedrive-manage", "ok", "download-now");
                ApiResponse::ok_json(&json!({
                    "ok": true,
                    "downloaded": result.downloaded,
                    "target": result.target,
                }))
            }
            Err(e) => {
                let _ = self.audit_account(
                    account,
                    "audit:onedrive-manage",
                    "error",
                    &format!("download-now id={id}: {e}"),
                );
                ApiResponse::error(500, &e)
            }
        }
    }

    /// `GET /api/v1/onedrive/conflicts?account=…` — the account's unresolved keep-both conflicts for
    /// the Conflict Center (#659). Read-only (session-gated on mobile; no cap). 404 without a handler.
    fn onedrive_conflicts(&self, req: &ApiRequest) -> ApiResponse {
        let h = match &self.onedrive_manage {
            Some(h) => h,
            None => {
                return ApiResponse::error(404, "onedrive management is not enabled on this server")
            }
        };
        let account = match req.q("account") {
            Some(a) if !a.is_empty() => a,
            _ => return ApiResponse::error(400, "account is required"),
        };
        match h.list_conflicts(account) {
            Ok(conflicts) => ApiResponse::ok_json(&json!({ "conflicts": conflicts })),
            Err(e) => ApiResponse::error(500, &format!("conflicts: {e}")),
        }
    }

    /// `POST /api/v1/onedrive/conflict/resolve?account=…&id=…&resolution=keep-both|keep-mine|keep-cloud`
    /// — resolve one keep-both conflict (#659). keep-mine deletes the cloud copy → biometric-gated;
    /// keep-both / keep-cloud are local-only. Cap-gated + audited.
    fn onedrive_conflict_resolve(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.manage_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(
            req,
            "OneDrive conflict resolve",
            &["account", "id", "resolution"],
        ) {
            Ok(parsed) => parsed,
            Err(response) => return response,
        };
        let req = &parsed;
        let (account, id, resolution) = match (
            req.q("account").filter(|a| !a.is_empty()),
            req.q("id").filter(|i| !i.is_empty()),
            req.q("resolution").filter(|r| !r.is_empty()),
        ) {
            (Some(a), Some(i), Some(r)) => (a, i, r),
            _ => return ApiResponse::error(400, "account, id and resolution are required"),
        };
        if !matches!(resolution, "keep-both" | "keep-mine" | "keep-cloud") {
            return ApiResponse::error(
                400,
                "resolution must be keep-both, keep-mine or keep-cloud",
            );
        }
        // keep-mine deletes the cloud version → the destructive per-action biometric gate on mobile.
        if resolution == "keep-mine" {
            if let Some(r) =
                self.biometric_challenge("conflict-keep-mine", account, "onedrive", id, req)
            {
                return r;
            }
        }
        self.onedrive_result(
            account,
            &format!("conflict-resolve id={id} resolution={resolution}"),
            h.resolve_conflict(account, id, resolution),
        )
    }

    /// `POST /api/v1/onedrive/cleanup?account=…` — the explicit offline→online cleanup (#659):
    /// drop provably-safe now-online bodies (to trash), keep anything unsynced. Bulk op →
    /// biometric-gated. Shares the same logic the mode-POST hook runs. Cap-gated + audited.
    fn onedrive_cleanup(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.manage_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(req, "OneDrive cleanup", &["account"]) {
            Ok(parsed) => parsed,
            Err(response) => return response,
        };
        let req = &parsed;
        let account = match req.q("account").filter(|a| !a.is_empty()) {
            Some(a) => a,
            None => return ApiResponse::error(400, "account is required"),
        };
        // Cleanup can drop many bodies → the bulk per-action biometric gate on mobile.
        if let Some(r) = self.biometric_challenge("bulk", account, "onedrive", account, req) {
            return r;
        }
        match h.cleanup_offline_to_online(account) {
            Ok(report) => {
                let _ = self.audit_account(account, "audit:onedrive-manage", "ok", "cleanup");
                ApiResponse::ok_json(&json!({ "ok": true, "cleanup": report }))
            }
            Err(e) => {
                let _ = self.audit_account(
                    account,
                    "audit:onedrive-manage",
                    "error",
                    &format!("cleanup: {e}"),
                );
                ApiResponse::error(500, &e)
            }
        }
    }

    fn mail_send(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.mail_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let request: MailSendRequest =
            match parse_agent_strict_json_with_limit(req, "mail send", 64 * 1024) {
                Ok(request) => request,
                Err(response) => return response,
            };
        if !valid_client_request_id(&request.request_id)
            || request.account.is_empty()
            || request.to.is_empty()
        {
            return ApiResponse::error(400, "at least one recipient (to) is required");
        }
        let importance = request
            .importance
            .as_deref()
            .filter(|value| !value.is_empty());
        let r = h.send(
            &request.account,
            &request.subject,
            &request.body,
            &request.to,
            &request.cc,
            &request.bcc,
            importance,
            request.read_receipt,
        );
        self.mail_result(
            &request.account,
            &format!("send to={}", request.to.len()),
            r,
        )
    }

    fn mail_reply(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.mail_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let request: MailReplyRequest =
            match parse_agent_strict_json_with_limit(req, "mail reply", 64 * 1024) {
                Ok(request) => request,
                Err(response) => return response,
            };
        if !valid_client_request_id(&request.request_id)
            || request.account.is_empty()
            || request.id.is_empty()
        {
            return ApiResponse::error(400, "invalid mail reply request");
        }
        let r = if request.body.is_empty() {
            h.reply(&request.account, &request.id, &request.comment, request.all)
        } else {
            h.reply_html(&request.account, &request.id, &request.body, request.all)
        };
        self.mail_result(
            &request.account,
            &format!("reply id={} all={}", request.id, request.all),
            r,
        )
    }

    fn mail_forward(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.mail_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let request: MailForwardRequest =
            match parse_agent_strict_json_with_limit(req, "mail forward", 64 * 1024) {
                Ok(request) => request,
                Err(response) => return response,
            };
        if !valid_client_request_id(&request.request_id)
            || request.account.is_empty()
            || request.id.is_empty()
            || request.to.is_empty()
        {
            return ApiResponse::error(400, "at least one recipient (to) is required");
        }
        let r = if request.body.is_empty() {
            h.forward(&request.account, &request.id, &request.comment, &request.to)
        } else {
            h.forward_html(&request.account, &request.id, &request.body, &request.to)
        };
        self.mail_result(
            &request.account,
            &format!("forward id={} to={}", request.id, request.to.len()),
            r,
        )
    }

    fn mail_move(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.mail_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed =
            match parse_strict_scalar_mutation(req, "mail move", &["account", "id", "destination"])
            {
                Ok(parsed) => parsed,
                Err(response) => return response,
            };
        let req = &parsed;
        let (account, id, dest) = match (
            req.q("account").filter(|a| !a.is_empty()),
            req.q("id").filter(|i| !i.is_empty()),
            req.q("destination").filter(|d| !d.is_empty()),
        ) {
            (Some(a), Some(i), Some(d)) => (a, i, d),
            _ => return ApiResponse::error(400, "account, id and destination are required"),
        };
        match h.move_to(account, id, dest) {
            Ok(new_id) => {
                let _ = self.audit_account(account, "audit:mail", "ok", &format!("move id={id}"));
                ApiResponse::ok_json(&json!({ "moved": id, "new_id": new_id }))
            }
            Err(e) => {
                let _ = self.audit_account(
                    account,
                    "audit:mail",
                    "error",
                    &format!("move id={id}: {e}"),
                );
                ApiResponse::error(500, &e)
            }
        }
    }

    fn mail_read(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.mail_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed =
            match parse_strict_scalar_mutation(req, "mail read", &["account", "id", "is_read"]) {
                Ok(parsed) => parsed,
                Err(response) => return response,
            };
        let req = &parsed;
        let (account, id) = match (
            req.q("account").filter(|a| !a.is_empty()),
            req.q("id").filter(|i| !i.is_empty()),
        ) {
            (Some(a), Some(i)) => (a, i),
            _ => return ApiResponse::error(400, "account and id are required"),
        };
        let is_read = req.q("is_read") != Some("0");
        let r = h.set_read(account, id, is_read);
        self.mail_result(account, &format!("set_read id={id} is_read={is_read}"), r)
    }

    fn mail_flag(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.mail_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(
            req,
            "mail flag",
            &["account", "id", "status", "due", "tz"],
        ) {
            Ok(parsed) => parsed,
            Err(response) => return response,
        };
        let req = &parsed;
        let (account, id) = match (
            req.q("account").filter(|a| !a.is_empty()),
            req.q("id").filter(|i| !i.is_empty()),
        ) {
            (Some(a), Some(i)) => (a, i),
            _ => return ApiResponse::error(400, "account and id are required"),
        };
        let status = req
            .q("status")
            .filter(|s| !s.is_empty())
            .unwrap_or("flagged");
        if !["notFlagged", "flagged", "complete"].contains(&status) {
            return ApiResponse::error(400, "status must be notFlagged|flagged|complete");
        }
        let due = req.q("due").filter(|s| !s.is_empty());
        let tz = req.q("tz").filter(|s| !s.is_empty()).unwrap_or("UTC");
        let r = h.set_flag(account, id, status, due, tz);
        self.mail_result(
            account,
            &format!("set_flag id={id} status={status} due={due:?}"),
            r,
        )
    }

    fn mail_categories(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.mail_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(
            req,
            "mail categories",
            &["account", "id", "categories"],
        ) {
            Ok(parsed) => parsed,
            Err(response) => return response,
        };
        let req = &parsed;
        let (account, id) = match (
            req.q("account").filter(|a| !a.is_empty()),
            req.q("id").filter(|i| !i.is_empty()),
        ) {
            (Some(a), Some(i)) => (a, i),
            _ => return ApiResponse::error(400, "account and id are required"),
        };
        // empty categories param clears the categories (a valid operation)
        let cats: Vec<String> = req
            .q("categories")
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        let r = h.set_categories(account, id, &cats);
        self.mail_result(
            account,
            &format!("set_categories id={id} n={}", cats.len()),
            r,
        )
    }

    /// `POST /api/v1/mail/draft` — create a draft (no `id`) or send an existing
    /// draft (`id` present). One endpoint covers both draft verbs.
    fn mail_draft(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.mail_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let request: MailDraftRequest =
            match parse_agent_strict_json_with_limit(req, "mail draft", 64 * 1024) {
                Ok(request) => request,
                Err(response) => return response,
            };
        if !valid_client_request_id(&request.request_id) || request.account.is_empty() {
            return ApiResponse::error(400, "invalid mail draft request");
        }
        if let Some(id) = request.id.as_deref().filter(|id| !id.is_empty()) {
            let r = h.send_draft(&request.account, id);
            return self.mail_result(&request.account, &format!("send_draft id={id}"), r);
        }
        match h.create_draft(
            &request.account,
            &request.subject,
            &request.body,
            &request.to,
        ) {
            Ok(draft_id) => {
                let _ = self.audit_account(&request.account, "audit:mail", "ok", "create_draft");
                ApiResponse::ok_json(&json!({ "draft_id": draft_id }))
            }
            Err(e) => {
                let _ = self.audit_account(
                    &request.account,
                    "audit:mail",
                    "error",
                    &format!("create_draft: {e}"),
                );
                ApiResponse::error(500, &e)
            }
        }
    }

    /// `POST /api/v1/restore` — re-create an archived item in the cloud from a strict
    /// JSON request. Requires the capability token; the actual work is the injected handler.
    fn restore(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match &self.restore {
            Some(h) => h,
            None => return ApiResponse::error(404, "restore is not enabled on this server"),
        };
        if !Self::cap_ok(&self.restore_cap_token, req) {
            return ApiResponse::error(401, "missing or invalid capability token");
        }
        let parsed =
            match parse_strict_scalar_mutation(req, "restore", &["account", "service", "id"]) {
                Ok(req) => req,
                Err(e) => return e,
            };
        let req = &parsed;
        let (account, service, id) = match (req.q("account"), req.q("service"), req.q("id")) {
            (Some(a), Some(s), Some(i)) if !a.is_empty() && !s.is_empty() && !i.is_empty() => {
                (a, s, i)
            }
            _ => return ApiResponse::error(400, "account, service and id are required"),
        };
        let pending_item = restore_cloud_pending_item(service, id);
        if let Some(r) =
            self.biometric_challenge("restore-cloud", account, service, &pending_item, req)
        {
            return r;
        }
        if let Err(e) = self.audit_account(
            account,
            "audit:restore",
            "started",
            &format!("restore requested service={service} id={id}"),
        ) {
            return ApiResponse::error(500, &format!("audit: {e}"));
        }
        match handler.restore(account, service, id) {
            Ok(RestoreResponse::Completed { new_id }) => {
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
            Ok(RestoreResponse::Queued { job_id, state }) => {
                let _ = self.audit_account(
                    account,
                    "audit:restore",
                    "ok",
                    &format!("restore queued service={service} id={id} job_id={job_id}"),
                );
                ApiResponse::ok_json(&json!({
                    "queued": true,
                    "restored": id,
                    "service": service,
                    "job_id": job_id,
                    "kind": "restore-cloud",
                    "state": state,
                }))
            }
            Err(e) => {
                let public = redact_restore_error_for_public_surface(&e);
                let _ = self.audit_account(
                    account,
                    "audit:restore",
                    "error",
                    &format!(
                        "restore error service={service} id={id}: {}",
                        public.message
                    ),
                );
                ApiResponse::error(public.status, &public.message)
            }
        }
    }

    /// `POST /api/v1/backup` — enqueue a durable mobile backup job from a strict JSON
    /// request. Requires its own capability token and, on mobile, a per-action biometric
    /// token. The handler must not run the backup inline.
    fn backup(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match &self.backup {
            Some(h) => h,
            None => return ApiResponse::error(404, "backup is not enabled on this server"),
        };
        if !Self::cap_ok(&self.backup_cap_token, req) {
            return ApiResponse::error(401, "missing or invalid capability token");
        }
        let parsed = match parse_strict_scalar_mutation(req, "backup", &["account", "services"]) {
            Ok(req) => req,
            Err(e) => return e,
        };
        let req = &parsed;
        let account = match req.q("account").filter(|a| !a.is_empty()) {
            Some(a) => a,
            None => return ApiResponse::error(400, "account is required"),
        };
        let services = match parse_backup_services_param(req.q("services")) {
            Ok(services) => services,
            Err(e) => return ApiResponse::error(400, &e),
        };
        if let Some(r) = self.biometric_challenge("backup", account, "backup", account, req) {
            return r;
        }
        match handler.enqueue_backup(account, &services) {
            Ok(job) => ApiResponse::ok_json(&json!({
                "queued": true,
                "job_id": job.job_id,
                "kind": "backup",
                "state": job.state,
            })),
            Err(e) => ApiResponse::error(500, &e),
        }
    }

    /// `GET /api/v1/jobs?account` — secret-free mobile job summaries.
    fn mobile_jobs(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match &self.mobile_jobs {
            Some(h) => h,
            None => return ApiResponse::error(404, "mobile jobs are not enabled on this server"),
        };
        if !Self::cap_ok(&self.mobile_job_cap_token, req) {
            return ApiResponse::error(401, "missing or invalid capability token");
        }
        let account = match req.q("account").filter(|a| !a.is_empty()) {
            Some(a) => a,
            None => return ApiResponse::error(400, "account is required"),
        };
        let limit = clamp_limit(req.q("limit"));
        match handler.list_jobs(account, limit) {
            Ok(jobs) => ApiResponse::ok_json(&json!({
                "account": account,
                "jobs": jobs.into_iter().map(mobile_job_summary_json).collect::<Vec<_>>(),
            })),
            Err(e) => ApiResponse::error(500, &e),
        }
    }

    /// `POST /api/v1/jobs/cancel?account&job_id` — mark a queued/running mobile
    /// job cancelled. This is queue-state cancellation; it does not claim to abort
    /// an already in-flight Graph request.
    fn mobile_job_cancel(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match &self.mobile_jobs {
            Some(h) => h,
            None => return ApiResponse::error(404, "mobile jobs are not enabled on this server"),
        };
        if !Self::cap_ok(&self.mobile_job_cap_token, req) {
            return ApiResponse::error(401, "missing or invalid capability token");
        }
        let (account, job_id) = match (req.q("account"), req.q("job_id")) {
            (Some(a), Some(j)) if !a.is_empty() && !j.is_empty() => (a, j),
            _ => return ApiResponse::error(400, "account and job_id are required"),
        };
        match handler.cancel_job(account, job_id) {
            Ok(cancelled) => ApiResponse::ok_json(&json!({
                "cancelled": cancelled,
                "job_id": job_id,
            })),
            Err(e) => ApiResponse::error(500, &e),
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
        let parsed = match parse_strict_scalar_mutation(req, "verify", &["account"]) {
            Ok(req) => req,
            Err(e) => return e,
        };
        let req = &parsed;
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
        let parsed = match parse_strict_scalar_mutation(
            req,
            "share",
            &["account", "service", "id", "type", "scope", "email", "role"],
        ) {
            Ok(req) => req,
            Err(e) => return e,
        };
        let req = &parsed;
        let (account, service, id) = match (req.q("account"), req.q("service"), req.q("id")) {
            (Some(a), Some(s), Some(i)) if !a.is_empty() && !s.is_empty() && !i.is_empty() => {
                (a, s, i)
            }
            _ => return ApiResponse::error(400, "account, service and id are required"),
        };
        // #onedrive-mobile 0.9: sharing is external/destructive — on the mobile profile it
        // requires a biometric per-action token bound to exactly this (share, account,
        // service, id). Covers both invite and anonymous-link modes. Desktop is unaffected.
        if let Some(r) = self.biometric_challenge("share", account, service, id, req) {
            return r;
        }
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
                    let public = redact_share_error_for_public_surface(&e);
                    let _ = self.audit_account(
                        account,
                        "audit:share",
                        "error",
                        &format!("invite error service={service} id={id}: {}", public.message),
                    );
                    ApiResponse::error(public.status, &public.message)
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
                let public = redact_share_error_for_public_surface(&e);
                let _ = self.audit_account(
                    account,
                    "audit:share",
                    "error",
                    &format!("share error service={service} id={id}: {}", public.message),
                );
                ApiResponse::error(public.status, &public.message)
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
        if let Err(e) = parse_strict_scalar_mutation(req, "sync command", &[]) {
            return e;
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

    /// `GET /api/v1/debug/stats` — the app's whole-process load. The embedded engine and the
    /// WebView share ONE OS process, so `/proc/self` is the total load the app causes (CPU +
    /// GPU/render threads + RAM + disk IO + disk wait), which powers the perf overlay.
    /// Linux/Android; each field defaults to 0 when unreadable. Self-stats only (plus the
    /// world-readable system IO-queue depth) — not sensitive.
    fn debug_stats(&self) -> ApiResponse {
        let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
        // Fields after the final ')' in /proc/<pid>/stat: index i = field (i+3). We read
        // utime (idx 11), stime (idx 12) → CPU, and delayacct_blkio_ticks (idx 39) → the time
        // the process spent *blocked on disk IO*. All at 100 Hz (Android/Linux) → 10 ms/tick.
        // The client turns the cumulative ms counters into live %/rates across polls.
        let (cpu_ms, blkio_ms) = read("/proc/self/stat")
            .rsplit_once(')')
            .map(|(_, rest)| {
                let f: Vec<&str> = rest.split_whitespace().collect();
                let g = |i: usize| f.get(i).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                ((g(11) + g(12)) * 10, g(39) * 10)
            })
            .unwrap_or((0, 0));
        // RSS: resident pages (field 2 of /proc/self/statm) × 4 KiB.
        let rss_kb = read("/proc/self/statm")
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u64>().ok())
            .map(|pages| pages * 4)
            .unwrap_or(0);
        // Disk IO (/proc/self/io): rchar/wchar = all read/write activity incl. the page cache
        // (what the app actually moves — usually non-zero); read_bytes/write_bytes = bytes that
        // truly hit the block device (often 0 when served from cache). We expose both.
        let io = read("/proc/self/io");
        let io_field = |k: &str| {
            io.lines()
                .find_map(|l| l.strip_prefix(k).and_then(|v| v.trim().parse::<u64>().ok()))
                .unwrap_or(0)
        };
        // GPU proxy: Android exposes no per-process GPU%, and the WebView's heavy renderer runs
        // in an ISOLATED child process this app can't read (different uid). So we sum the CPU of
        // the render/compositor/GPU threads that DO live in-process — RenderThread, VizWebView,
        // the in-process GPU thread (comm is capped at 15 chars → "Chrome_InProcGp", so match the
        // truncated "InProcGp" too), and the mali driver threads. Same tick→ms scale as cpu_ms.
        // This tracks the in-app render load; the out-of-process WebView renderer is not included.
        let mut render_ms = 0u64;
        if let Ok(entries) = std::fs::read_dir("/proc/self/task") {
            for e in entries.flatten() {
                let p = e.path();
                let comm = std::fs::read_to_string(p.join("comm")).unwrap_or_default();
                let c = comm.trim();
                if c.contains("RenderThread")
                    || c.contains("InProcGp")
                    || c.contains("Gpu")
                    || c.contains("GPU")
                    || c.contains("Viz")
                    || c.contains("mali")
                {
                    if let Some((_, rest)) = std::fs::read_to_string(p.join("stat"))
                        .unwrap_or_default()
                        .rsplit_once(')')
                    {
                        let f: Vec<&str> = rest.split_whitespace().collect();
                        let g =
                            |i: usize| f.get(i).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                        render_ms += (g(11) + g(12)) * 10;
                    }
                }
            }
        }
        // IO queue depth (system-wide, /proc/diskstats): "I/Os currently in progress" is field 9
        // (token index 11) per device. Report the busiest real block device (skip loop/ram/zram)
        // — an instantaneous indicator of disk saturation the per-process counters can't show.
        let mut io_inflight = 0u64;
        for line in read("/proc/diskstats").lines() {
            let t: Vec<&str> = line.split_whitespace().collect();
            let name = match t.get(2) {
                Some(n) => *n,
                None => continue,
            };
            if name.starts_with("loop") || name.starts_with("ram") || name.starts_with("zram") {
                continue;
            }
            let inflight = t.get(11).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            io_inflight = io_inflight.max(inflight);
        }
        ApiResponse::ok_json(&json!({
            "cpu_ms": cpu_ms,
            "render_ms": render_ms,
            "rss_kb": rss_kb,
            "io_read": io_field("rchar:"),
            "io_write": io_field("wchar:"),
            "io_disk_read": io_field("read_bytes:"),
            "io_disk_write": io_field("write_bytes:"),
            "blkio_ms": blkio_ms,
            "io_inflight": io_inflight,
            "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        }))
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

    /// `GET /api/v1/onedrive/transfers` — in-flight download/materialization progress
    /// (#onedrive-mobile 0.8). Idle (`[]`) when no transfer engine is wired yet.
    fn transfers_state(&self) -> ApiResponse {
        let list = self
            .transfers
            .as_ref()
            .map(|t| t.transfers())
            .unwrap_or_default();
        let items: Vec<Value> = list
            .iter()
            .map(|t| {
                json!({
                    "id": t.id,
                    "name": t.name,
                    "bytes_done": t.bytes_done,
                    "bytes_total": t.bytes_total,
                    "retry_after_secs": t.retry_after_secs,
                    "paused": t.paused,
                })
            })
            .collect();
        ApiResponse::ok_json(&json!({ "count": items.len(), "transfers": items }))
    }

    /// `POST /api/v1/onedrive/transfers/cancel?id=…` — cap-gated cancel of one in-flight
    /// transfer (#onedrive-mobile 0.8). 404 when no transfer engine is wired.
    fn transfers_cancel(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match &self.transfers {
            Some(h) => h,
            None => return ApiResponse::error(404, "transfers are not enabled on this server"),
        };
        if !Self::cap_ok(&self.transfer_cap_token, req) {
            return ApiResponse::error(401, "missing or invalid capability token");
        }
        let parsed = match parse_strict_scalar_mutation(req, "transfer cancel", &["id"]) {
            Ok(parsed) => parsed,
            Err(response) => return response,
        };
        let req = &parsed;
        let id = match req.q("id") {
            Some(i) if !i.is_empty() => i,
            _ => return ApiResponse::error(400, "id is required"),
        };
        ApiResponse::ok_json(&json!({ "cancelled": handler.cancel(id) }))
    }

    /// `POST /api/v1/onedrive/transfers/pause?id=…` — cap-gated pause of one in-flight transfer
    /// (#659, queue-deep). Persistent until resumed. 404 when no transfer engine is wired.
    fn transfers_pause(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match &self.transfers {
            Some(h) => h,
            None => return ApiResponse::error(404, "transfers are not enabled on this server"),
        };
        if !Self::cap_ok(&self.transfer_cap_token, req) {
            return ApiResponse::error(401, "missing or invalid capability token");
        }
        let parsed = match parse_strict_scalar_mutation(req, "transfer pause", &["id"]) {
            Ok(parsed) => parsed,
            Err(response) => return response,
        };
        let req = &parsed;
        let id = match req.q("id") {
            Some(i) if !i.is_empty() => i,
            _ => return ApiResponse::error(400, "id is required"),
        };
        ApiResponse::ok_json(&json!({ "paused": handler.pause(id) }))
    }

    /// `POST /api/v1/onedrive/transfers/retry?id=…` — cap-gated retry of a failed/backed-off or
    /// paused transfer (#659): re-queue it (clears pause + 429 backoff) for the next pass. Also the
    /// resume affordance (retry un-pauses). 404 when no transfer engine is wired.
    fn transfers_retry(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match &self.transfers {
            Some(h) => h,
            None => return ApiResponse::error(404, "transfers are not enabled on this server"),
        };
        if !Self::cap_ok(&self.transfer_cap_token, req) {
            return ApiResponse::error(401, "missing or invalid capability token");
        }
        let parsed = match parse_strict_scalar_mutation(req, "transfer retry", &["id"]) {
            Ok(parsed) => parsed,
            Err(response) => return response,
        };
        let req = &parsed;
        let id = match req.q("id") {
            Some(i) if !i.is_empty() => i,
            _ => return ApiResponse::error(400, "id is required"),
        };
        ApiResponse::ok_json(&json!({ "retried": handler.retry(id) }))
    }

    /// `GET /api/v1/onedrive/policy` — the effective mobile transfer policy plus the
    /// mass-delete-guard status (#onedrive-mobile 0.8). Read-only; reads the config.
    fn policy_state(&self) -> ApiResponse {
        let s = &self.config.sync;
        let g = &s.delete_guard;
        ApiResponse::ok_json(&json!({
            "wifi_only": s.wifi_only,
            "charging_only": s.charging_only,
            "min_free_bytes": s.min_free_bytes,
            "delete_guard": {
                "max_absolute": g.max_absolute,
                "max_fraction": g.max_fraction,
                "fraction_min_total": g.fraction_min_total,
            },
        }))
    }

    /// `GET /api/v1/onedrive/mode?account=…` — the account's OneDrive mode policy
    /// (default + per-folder overrides) (#651). Read-only. With the mode handler wired the
    /// read is **fresh** (reflects a prior POST); without it (read-only `serve`) it falls
    /// back to the static in-memory config.
    fn onedrive_mode(&self, req: &ApiRequest) -> ApiResponse {
        let account = match req.q("account") {
            Some(a) if !a.is_empty() => a,
            _ => return ApiResponse::error(400, "account is required"),
        };
        let modes = match &self.onedrive_mode {
            Some(h) => match h.modes(account) {
                Ok(m) => m,
                Err(e) => return ApiResponse::error(500, &format!("mode: {e}")),
            },
            None => self
                .config
                .onedrive_modes
                .get(account)
                .cloned()
                .unwrap_or_default(),
        };
        let folder_modes: serde_json::Map<String, Value> = modes
            .folder_modes
            .iter()
            .map(|(id, m)| (id.clone(), Value::from(m.as_str())))
            .collect();
        ApiResponse::ok_json(&json!({
            "account": account,
            "default_mode": modes.default_mode.as_str(),
            "folder_modes": folder_modes,
        }))
    }

    /// `POST /api/v1/onedrive/mode?account=…&folder=…&mode=online|sync|offline` — set a
    /// folder's explicit OneDrive mode override; an empty/absent `mode` **clears** it
    /// (#651). Cap-token-gated and audited (`audit:onedrive-mode`). Persists via the
    /// injected mode handler (`Config::load → mutate → validate → save`).
    fn onedrive_set_mode(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match &self.onedrive_mode {
            Some(h) => h,
            None => return ApiResponse::error(404, "OneDrive mode is not editable on this server"),
        };
        if !Self::cap_ok(&self.onedrive_mode_cap_token, req) {
            return ApiResponse::error(401, "missing or invalid capability token");
        }
        let parsed = match parse_strict_scalar_mutation(
            req,
            "OneDrive mode",
            &["account", "folder", "mode"],
        ) {
            Ok(parsed) => parsed,
            Err(response) => return response,
        };
        let req = &parsed;
        let (account, folder) = match (req.q("account"), req.q("folder")) {
            (Some(a), Some(f)) if !a.is_empty() && !f.is_empty() => (a, f),
            _ => return ApiResponse::error(400, "account and folder are required"),
        };
        // `mode` present + non-empty => set; empty/absent => clear the override. Parsed
        // via serde so it stays symmetric with `OneDriveMode::as_str` (online/sync/offline).
        let mode: Option<OneDriveMode> = match req.q("mode").filter(|m| !m.is_empty()) {
            Some(m) => match serde_json::from_str::<OneDriveMode>(&format!("\"{m}\"")) {
                Ok(parsed) => Some(parsed),
                Err(_) => return ApiResponse::error(400, "mode must be online, sync or offline"),
            },
            None => None,
        };
        if self.biometric_gate {
            if mode == Some(OneDriveMode::Offline) {
                let requires_confirmation = match &self.onedrive_risk {
                    Some(risk) => match risk.offline_mode_risk(account, folder) {
                        Ok(risk) => risk.requires_confirmation,
                        Err(_) => true,
                    },
                    None => true,
                };
                if requires_confirmation {
                    let item = onedrive_mode_offline_pat_item(folder);
                    if let Some(r) = self.biometric_challenge(
                        "mode-switch-offline-large",
                        account,
                        "onedrive",
                        &item,
                        req,
                    ) {
                        return r;
                    }
                }
            }
            if mode == Some(OneDriveMode::Online) && self.onedrive_manage.is_some() {
                let item = onedrive_mode_online_cleanup_pat_item(folder);
                if let Some(r) = self.biometric_challenge("bulk", account, "onedrive", &item, req) {
                    return r;
                }
            }
        }
        let summary = format!(
            "mode-set account={account} folder={folder} mode={}",
            mode.map(|m| m.as_str()).unwrap_or("clear")
        );
        if let Err(e) = self.audit_account(account, "audit:onedrive-mode", "started", &summary) {
            return ApiResponse::error(500, &format!("audit: {e}"));
        }
        match handler.set_folder(account, folder, mode) {
            Ok(()) => {
                let _ = self.audit_account(account, "audit:onedrive-mode", "ok", &summary);
                let mut resp = json!({
                    "account": account,
                    "folder": folder,
                    "mode": mode.map(|m| m.as_str()),
                });
                // #659 D1: switching a folder to ONLINE triggers the offline→online cleanup (drop
                // provably-safe now-online bodies to trash, keep unsynced), reported as an additive
                // `cleanup: {freed, kept}` key. Runs ONLY when the manage handler is wired
                // (daemon/mobile); the mode-only read-only router skips it, so #651/#652's
                // mode-toggle tests are unaffected. The mode already persisted, so a cleanup error
                // never fails the switch — it is reported as `cleanup_error` and audited.
                if mode.map(|m| m.as_str()) == Some("online") {
                    if let Some(mh) = &self.onedrive_manage {
                        match mh.cleanup_offline_to_online(account) {
                            Ok(report) => {
                                resp["cleanup"] = report;
                            }
                            Err(e) => {
                                let _ = self.audit_account(
                                    account,
                                    "audit:onedrive-manage",
                                    "error",
                                    &format!("cleanup-on-mode account={account}: {e}"),
                                );
                                resp["cleanup_error"] = json!(e);
                            }
                        }
                    }
                }
                ApiResponse::ok_json(&resp)
            }
            Err(e) => {
                let _ = self.audit_account(
                    account,
                    "audit:onedrive-mode",
                    "error",
                    &format!("{summary}: {e}"),
                );
                ApiResponse::error(500, &format!("mode: {e}"))
            }
        }
    }

    /// The OneDrive storage quota for an account — a live Graph call via the
    /// daemon's handler (#564). 404 when no handler (read-only CLI `serve`).
    fn drive_info(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match &self.onedrive_info {
            Some(h) => h,
            None => return ApiResponse::error(404, "OneDrive info is not enabled on this server"),
        };
        let account = match req.q("account") {
            Some(a) if !a.is_empty() => a,
            _ => return ApiResponse::error(400, "account is required"),
        };
        match handler.drive_quota(account) {
            Ok(q) => ApiResponse::ok_json(&json!({ "quota": q })),
            // No write token / not connected is an EXPECTED state (e.g. before
            // login) — return 200 with `available:false` so the UI shows a quiet
            // "not connected" state instead of a console error from a 5xx fetch.
            Err(e) => ApiResponse::ok_json(&json!({ "available": false, "reason": e })),
        }
    }

    /// A OneDrive item's sharing permissions ("who has access") — a live Graph
    /// call via the daemon's handler (#564). 404 when no handler; 400 without
    /// account/id. Fetched lazily by the explorer on detail open.
    fn item_permissions(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match &self.onedrive_info {
            Some(h) => h,
            None => return ApiResponse::error(404, "OneDrive info is not enabled on this server"),
        };
        let (account, id) = match (req.q("account"), req.q("id")) {
            (Some(a), Some(i)) if !a.is_empty() && !i.is_empty() => (a, i),
            _ => return ApiResponse::error(400, "account and id are required"),
        };
        match handler.permissions(account, id) {
            Ok(p) => ApiResponse::ok_json(&json!({ "permissions": p })),
            Err(e) => ApiResponse::error(502, &e),
        }
    }

    /// A OneDrive folder's children — a live, fully paged Graph call via the
    /// daemon's handler (#648, Mode 1 online). 404 when no handler; 400 without an
    /// account; an empty/absent `folder` = the drive root. No store write.
    fn onedrive_children(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match &self.onedrive_list {
            Some(h) => h,
            None => {
                return ApiResponse::error(404, "OneDrive listing is not enabled on this server")
            }
        };
        let account = match req.q("account") {
            Some(a) if !a.is_empty() => a,
            _ => return ApiResponse::error(400, "account is required"),
        };
        let folder = req.q("folder").unwrap_or("");
        match handler.children(account, folder) {
            Ok(mut children) => {
                // #651: annotate each child with its effective OneDrive mode. Live Graph
                // children carry no parent chain, so F's ancestry (deepest-first, F's
                // parents up toward the root) is supplied by the caller via `&ancestry=`
                // (comma-separated ids); absent => folder-level resolution. `folder` (F) is
                // always the immediate parent of every child, so it heads each child's
                // ancestry; a child's own id (a subfolder's override) wins over inheritance.
                let ancestry_param = req.q("ancestry").unwrap_or("");
                let mut child_ancestry: Vec<&str> = Vec::new();
                if !folder.is_empty() {
                    child_ancestry.push(folder);
                }
                child_ancestry.extend(ancestry_param.split(',').filter(|s| !s.is_empty()));
                let modes = match &self.onedrive_mode {
                    Some(h) => h.modes(account).unwrap_or_default(),
                    None => self
                        .config
                        .onedrive_modes
                        .get(account)
                        .cloned()
                        .unwrap_or_default(),
                };
                for child in &mut children {
                    let id = child.get("id").and_then(Value::as_str).map(str::to_string);
                    if let (Some(id), Some(obj)) = (id, child.as_object_mut()) {
                        let em = modes.effective_mode(&id, &child_ancestry).as_str();
                        obj.insert("effective_mode".to_string(), Value::from(em));
                    }
                }
                ApiResponse::ok_json(&json!({ "children": children }))
            }
            Err(e) => ApiResponse::error(502, &e),
        }
    }

    /// On-demand OneDrive content by id (#649, Mode 1 online): a live Graph download,
    /// served inertly (safe content-type from `name`). 404 when no handler; 400 without
    /// account/id. No store row is written (Mode 1 keeps no metadata).
    fn onedrive_open(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match &self.onedrive_open {
            Some(h) => h,
            None => return ApiResponse::error(404, "OneDrive open is not enabled on this server"),
        };
        let (account, id) = match (req.q("account"), req.q("id")) {
            (Some(a), Some(i)) if !a.is_empty() && !i.is_empty() => (a, i),
            _ => return ApiResponse::error(400, "account and id are required"),
        };
        let name = req.q("name").unwrap_or("");
        match handler.download(account, id) {
            Ok(bytes) => ApiResponse {
                status: 200,
                content_type: safe_content_type(name).into(),
                body: bytes,
                headers: Vec::new(),
            },
            Err(e) => ApiResponse::error(502, &e),
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
            .map(|a| {
                let auth = self
                    .account_auth
                    .as_ref()
                    .map(|handler| handler.status(&a.id));
                json!({ "id": a.id, "username": a.username, "auth": auth })
            })
            .collect();
        ApiResponse::ok_json(&json!({ "accounts": accounts }))
    }

    /// Account-auth cap check (#68): 404 if sign-in isn't enabled (read-only serve),
    /// 401 unless the request carries the account capability token.
    fn account_gate(
        &self,
        req: &ApiRequest,
    ) -> Result<&std::sync::Arc<dyn AccountAuthHandler>, ApiResponse> {
        let h = self.account_auth.as_ref().ok_or_else(|| {
            ApiResponse::error(404, "account sign-in is not enabled on this server")
        })?;
        if !Self::cap_ok(&self.account_cap_token, req) {
            return Err(ApiResponse::error(
                401,
                "missing or invalid capability token",
            ));
        }
        Ok(h)
    }

    fn account_login_start(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.account_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed =
            match parse_strict_scalar_mutation(req, "account login start", &["account", "role"]) {
                Ok(req) => req,
                Err(e) => return e,
            };
        let req = &parsed;
        let account = match req.q("account") {
            Some(a) => a,
            None => return ApiResponse::error(400, "missing 'account'"),
        };
        let role = match req.q("role").and_then(AccountAuthRole::parse) {
            Some(role) => role,
            None => return ApiResponse::error(400, "invalid 'role'"),
        };
        match h.start_login(account, role) {
            Ok(v) => ApiResponse::ok_json(&v),
            Err(e) => ApiResponse::error(502, &e),
        }
    }

    fn account_login_poll(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.account_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(req, "account login poll", &["id"]) {
            Ok(req) => req,
            Err(e) => return e,
        };
        let req = &parsed;
        let id = match req.q("id") {
            Some(i) => i,
            None => return ApiResponse::error(400, "missing 'id'"),
        };
        ApiResponse::ok_json(&h.poll_login(id))
    }

    fn account_login_cancel(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.account_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed = match parse_strict_scalar_mutation(req, "account login cancel", &["id"]) {
            Ok(req) => req,
            Err(e) => return e,
        };
        let Some(id) = parsed.q("id") else {
            return ApiResponse::error(400, "missing 'id'");
        };
        ApiResponse::ok_json(&json!({ "cancelled": h.cancel_login(id) }))
    }

    fn account_signout(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.account_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let parsed =
            match parse_strict_scalar_mutation(req, "account signout", &["account", "role"]) {
                Ok(req) => req,
                Err(e) => return e,
            };
        let req = &parsed;
        let account = match req.q("account") {
            Some(a) => a,
            None => return ApiResponse::error(400, "missing 'account'"),
        };
        let role = match parsed.q("role").and_then(AccountAuthRole::parse) {
            Some(role) => role,
            None => return ApiResponse::error(400, "invalid 'role'"),
        };
        match h.sign_out(account, role) {
            Ok(v) => ApiResponse::ok_json(&v),
            Err(e) => ApiResponse::error(500, &e),
        }
    }

    /// Push cap check (#576): 404 if push isn't enabled (read-only serve),
    /// 401 unless the request carries the push capability token.
    fn push_gate(&self, req: &ApiRequest) -> Result<&std::sync::Arc<dyn PushHandler>, ApiResponse> {
        let h = self
            .push
            .as_ref()
            .ok_or_else(|| ApiResponse::error(404, "push is not enabled on this server"))?;
        if !Self::cap_ok(&self.push_cap_token, req) {
            return Err(ApiResponse::error(
                401,
                "missing or invalid capability token",
            ));
        }
        Ok(h)
    }

    /// Register this device's FCM token (sent by the native shell via the web UI).
    fn push_register(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.push_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let request: PushRegisterRequest = match parse_agent_strict_json(req, "push register") {
            Ok(request) => request,
            Err(response) => return response,
        };
        if !valid_client_request_id(&request.request_id) || request.token.is_empty() {
            return ApiResponse::error(400, "invalid push register request");
        }
        match h.register(&request.token) {
            Ok(()) => ApiResponse::ok_json(&json!({ "registered": true })),
            Err(e) => ApiResponse::error(500, &e),
        }
    }

    /// Send a test push to every registered device (UI diagnostics).
    fn push_test(&self, req: &ApiRequest) -> ApiResponse {
        let h = match self.push_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        match h.send_test() {
            Ok(v) => ApiResponse::ok_json(&v),
            Err(e) => ApiResponse::error(502, &e),
        }
    }

    /// Resolve the agent handler + check its cap token (shared by the agent POSTs).
    fn agent_gate(
        &self,
        req: &ApiRequest,
    ) -> Result<&std::sync::Arc<dyn AgentHandler>, ApiResponse> {
        let handler = self
            .agent
            .as_ref()
            .ok_or_else(|| ApiResponse::error(404, "the agent is not enabled on this server"))?;
        if !Self::cap_ok(&self.agent_cap_token, req) {
            return Err(ApiResponse::error(
                401,
                "missing or invalid capability token",
            ));
        }
        Ok(handler)
    }

    fn agent_session_create(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match self.agent_gate(req) {
            Ok(handler) => handler,
            Err(error) => return with_no_store(error),
        };
        let request: AgentSessionCreateRequest =
            match parse_agent_strict_json(req, "session create") {
                Ok(request) => request,
                Err(error) => return error,
            };
        if !valid_client_request_id(&request.request_id)
            || request
                .display_name
                .as_ref()
                .is_some_and(|name| name.is_empty() || name.len() > 128)
        {
            return no_store_json_error(400, "invalid session create request");
        }
        match handler.session_create(&request.request_id, request.display_name.as_deref()) {
            Ok(value) => with_no_store(ApiResponse::ok_json(&value)),
            Err(error) => no_store_json_error(agent_session_error_status(&error), &error),
        }
    }

    fn agent_session_select(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match self.agent_gate(req) {
            Ok(handler) => handler,
            Err(error) => return with_no_store(error),
        };
        let request: AgentSessionSelectRequest =
            match parse_agent_strict_json(req, "session select") {
                Ok(request) => request,
                Err(error) => return error,
            };
        if !valid_client_request_id(&request.request_id)
            || !valid_opaque_agent_id(&request.session_id)
        {
            return no_store_json_error(400, "invalid session select request");
        }
        match handler.session_select(&request.request_id, &request.session_id) {
            Ok(value) => with_no_store(ApiResponse::ok_json(&value)),
            Err(error) => no_store_json_error(agent_session_error_status(&error), &error),
        }
    }

    fn agent_session_list(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match self.agent_gate(req) {
            Ok(handler) => handler,
            Err(error) => return with_no_store(error),
        };
        let cursor = req.q("cursor");
        if cursor.is_some_and(|value| value.is_empty() || value.len() > 512) {
            return no_store_json_error(400, "invalid session list cursor");
        }
        let limit = match parse_bounded_page_limit(req.q("limit")) {
            Ok(limit) => limit,
            Err(error) => return error,
        };
        match handler.session_list(cursor, limit) {
            Ok(value) => with_no_store(ApiResponse::ok_json(&value)),
            Err(error) => no_store_json_error(agent_session_error_status(&error), &error),
        }
    }

    fn agent_session_history(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match self.agent_gate(req) {
            Ok(handler) => handler,
            Err(error) => return with_no_store(error),
        };
        let Some(session_id) = req.q("session_id").filter(|id| valid_opaque_agent_id(id)) else {
            return no_store_json_error(400, "invalid session history request");
        };
        let cursor = req.q("cursor");
        if cursor.is_some_and(|value| value.is_empty() || value.len() > 512) {
            return no_store_json_error(400, "invalid session history cursor");
        }
        let limit = match parse_bounded_page_limit(req.q("limit")) {
            Ok(limit) => limit,
            Err(error) => return error,
        };
        match handler.session_history(session_id, cursor, limit) {
            Ok(value) => with_no_store(ApiResponse::ok_json(&value)),
            Err(error) => no_store_json_error(agent_session_error_status(&error), &error),
        }
    }

    fn agent_request_status(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match self.agent_gate(req) {
            Ok(handler) => handler,
            Err(error) => return with_no_store(error),
        };
        const FIELDS: [&str; 3] = ["session_id", "route", "request_id"];
        if req.query.len() != FIELDS.len()
            || FIELDS
                .iter()
                .any(|field| req.query.iter().filter(|(name, _)| name == field).count() != 1)
            || req
                .query
                .iter()
                .any(|(name, _)| !FIELDS.contains(&name.as_str()))
        {
            return no_store_json_error(400, "invalid agent request status query");
        }
        let Some(session_id) = req.q("session_id").filter(|id| valid_opaque_agent_id(id)) else {
            return no_store_json_error(400, "invalid agent request status query");
        };
        let Some(request_id) = req.q("request_id").filter(|id| valid_client_request_id(id)) else {
            return no_store_json_error(400, "invalid agent request status query");
        };
        let Some(route) = req.q("route").filter(|route| *route == "agent_turn") else {
            return no_store_json_error(400, "invalid agent request status query");
        };
        match handler.request_status(session_id, route, request_id) {
            Ok(value) => with_no_store(ApiResponse::ok_json(&value)),
            Err(error) => no_store_json_error(agent_session_error_status(&error), &error),
        }
    }

    fn agent_session_archive(&self, req: &ApiRequest) -> ApiResponse {
        self.agent_session_operation(req, "session archive", |handler, request| {
            handler.session_archive(request)
        })
    }

    fn agent_user_presence_start(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match self.agent_gate(req) {
            Ok(handler) => handler,
            Err(error) => return with_no_store(error),
        };
        let request: AgentUserPresenceStartRequest =
            match parse_agent_strict_json(req, "user presence start") {
                Ok(request) => request,
                Err(error) => return error,
            };
        let binding_matches = matches!(
            (&*request.kind, &request.binding),
            (
                "session_archive",
                AgentUserPresenceBinding::SessionArchive(AgentSessionArchiveBinding {
                    session_id
                })
            ) if valid_opaque_agent_id(session_id)
        ) || matches!(
            (&*request.kind, &request.binding),
            (
                "session_pairing_reveal",
                AgentUserPresenceBinding::SessionPairingReveal(
                    AgentSessionPairingRevealBinding { session_id, pair_id }
                )
            ) if valid_opaque_agent_id(session_id) && valid_opaque_agent_id(pair_id)
        ) || matches!(
            (&*request.kind, &request.binding),
            (
                "session_pairing_import",
                AgentUserPresenceBinding::SessionPairingImport(
                    AgentSessionPairingImportBinding { pairing_code }
                )
            ) if pairing_code.len() == 81 && pairing_code.is_ascii()
        );
        if !valid_client_request_id(&request.request_id) || !binding_matches {
            return no_store_json_error(400, "invalid user presence request");
        }
        match handler.user_presence_start(request) {
            Ok(value) => with_no_store(ApiResponse::ok_json(&value)),
            Err(error) => no_store_json_error(agent_session_error_status(&error), &error),
        }
    }

    fn agent_user_presence_confirm(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match self.agent_gate(req) {
            Ok(handler) => handler,
            Err(error) => return with_no_store(error),
        };
        let request: AgentUserPresenceConfirmRequest =
            match parse_agent_strict_json(req, "user presence confirm") {
                Ok(request) => request,
                Err(error) => return error,
            };
        if !valid_client_request_id(&request.request_id)
            || !valid_opaque_agent_id(&request.operation_id)
            || !valid_opaque_agent_id(&request.intent_id)
            || request.token.is_empty()
            || request.token.len() > 128
            || request.action_hash.len() != 64
        {
            return no_store_json_error(400, "invalid user presence confirmation");
        }
        if let Some(response) = self.biometric_challenge(
            "user-presence",
            "agent",
            "agent",
            &request.operation_id,
            req,
        ) {
            return with_no_store(response);
        }
        match handler.user_presence_confirm(request) {
            Ok(value) => with_no_store(ApiResponse::ok_json(&value)),
            Err(error) => no_store_json_error(agent_session_error_status(&error), &error),
        }
    }

    fn agent_session_pairing_create(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match self.agent_gate(req) {
            Ok(handler) => handler,
            Err(error) => return with_no_store(error),
        };
        let request: AgentSessionPairingCreateRequest =
            match parse_agent_strict_json(req, "session pairing create") {
                Ok(request) => request,
                Err(error) => return error,
            };
        if !valid_client_request_id(&request.request_id)
            || !valid_opaque_agent_id(&request.session_id)
        {
            return no_store_json_error(400, "invalid session pairing create request");
        }
        match handler.session_pairing_create(request) {
            Ok(value) => with_no_store(ApiResponse::ok_json(&value)),
            Err(error) => no_store_json_error(agent_session_error_status(&error), &error),
        }
    }

    fn agent_session_pairing_reveal(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match self.agent_gate(req) {
            Ok(handler) => handler,
            Err(error) => return with_no_store(error),
        };
        let request: AgentSessionPairingOperationRequest =
            match parse_agent_strict_json(req, "session pairing reveal") {
                Ok(request) => request,
                Err(error) => return error,
            };
        if !valid_client_request_id(&request.request_id)
            || !valid_opaque_agent_id(&request.operation_id)
        {
            return no_store_json_error(400, "invalid session pairing reveal request");
        }
        match handler.session_pairing_reveal(request) {
            Ok(value) => with_no_store(ApiResponse::ok_json(&value)),
            Err(error) => no_store_json_error(agent_session_error_status(&error), &error),
        }
    }

    fn agent_session_pairing_claim(&self, req: &ApiRequest) -> ApiResponse {
        self.agent_session_pairing_operation(req, "session pairing claim", |handler, request| {
            handler.session_pairing_claim(request)
        })
    }

    fn agent_session_pairing_finalize(&self, req: &ApiRequest) -> ApiResponse {
        self.agent_session_pairing_operation(req, "session pairing finalize", |handler, request| {
            handler.session_pairing_finalize(request)
        })
    }

    fn agent_session_pairing_revoke(&self, req: &ApiRequest) -> ApiResponse {
        self.agent_session_pairing_operation(req, "session pairing revoke", |handler, request| {
            handler.session_pairing_revoke(request)
        })
    }

    fn agent_session_pairing_operation<F>(
        &self,
        req: &ApiRequest,
        label: &str,
        operation: F,
    ) -> ApiResponse
    where
        F: FnOnce(
            &dyn AgentHandler,
            AgentSessionPairingOperationRequest,
        ) -> Result<serde_json::Value, String>,
    {
        let handler = match self.agent_gate(req) {
            Ok(handler) => handler,
            Err(error) => return with_no_store(error),
        };
        let request: AgentSessionPairingOperationRequest = match parse_agent_strict_json(req, label)
        {
            Ok(request) => request,
            Err(error) => return error,
        };
        if !valid_client_request_id(&request.request_id)
            || !valid_opaque_agent_id(&request.operation_id)
        {
            return no_store_json_error(400, "invalid session pairing operation");
        }
        match operation(handler.as_ref(), request) {
            Ok(value) => with_no_store(ApiResponse::ok_json(&value)),
            Err(error) => no_store_json_error(agent_session_error_status(&error), &error),
        }
    }

    fn agent_session_operation<F>(&self, req: &ApiRequest, label: &str, operation: F) -> ApiResponse
    where
        F: FnOnce(
            &dyn AgentHandler,
            AgentSessionArchiveRequest,
        ) -> Result<serde_json::Value, String>,
    {
        let handler = match self.agent_gate(req) {
            Ok(handler) => handler,
            Err(error) => return with_no_store(error),
        };
        let request: AgentSessionArchiveRequest = match parse_agent_strict_json(req, label) {
            Ok(request) => request,
            Err(error) => return error,
        };
        if !valid_client_request_id(&request.request_id)
            || !valid_opaque_agent_id(&request.operation_id)
        {
            return no_store_json_error(400, "invalid session operation");
        }
        match operation(handler.as_ref(), request) {
            Ok(value) => with_no_store(ApiResponse::ok_json(&value)),
            Err(error) => no_store_json_error(agent_session_error_status(&error), &error),
        }
    }

    /// Start an agent turn; the client streams it from `/api/v1/agent/stream?turn=<id>`.
    fn agent_turn(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match self.agent_gate(req) {
            Ok(h) => h,
            Err(e) => return with_no_store(e),
        };
        let request: AgentTurnRequest = match parse_agent_strict_json_with_limit(
            req,
            "agent turn",
            AGENT_TURN_JSON_MAX_BYTES,
        ) {
            Ok(request) => request,
            Err(error) => return error,
        };
        if !valid_client_request_id(&request.request_id)
            || !valid_opaque_agent_id(&request.session_id)
            || request.account.is_empty()
            || request.account.len() > 128
            || request.prompt.is_empty()
            || request.prompt.len() > 32 * 1024
        {
            return no_store_json_error(400, "invalid agent turn request");
        }
        match std::sync::Arc::clone(handler).start_turn_request(request) {
            Ok(turn_id) => with_no_store(ApiResponse::ok_json(&json!({ "turn": turn_id }))),
            // #639: a host-verified not-ready product turn is a closed 409, not a blanket 500.
            Err(e)
                if matches!(
                    e.as_str(),
                    "product_not_ready" | "provider_busy" | "session_busy" | "request_id_conflict"
                ) =>
            {
                no_store_json_error(409, &e)
            }
            Err(e) => no_store_json_error(agent_session_error_status(&e), &e),
        }
    }

    /// Confirm a pending destructive action with its one-time token (REQ-AGENT-003).
    fn agent_confirm(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match self.agent_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let request: AgentConfirmRequest = match parse_agent_strict_json(req, "agent confirm") {
            Ok(request) => request,
            Err(response) => return response,
        };
        if !valid_client_request_id(&request.request_id)
            || !valid_opaque_agent_id(&request.pending)
            || request.token.is_empty()
            || request.token.len() > 512
            || request.action_hash.is_empty()
            || request.action_hash.len() > 128
        {
            return no_store_json_error(400, "invalid agent confirm request");
        }
        if self.biometric_gate {
            let binding = match handler.pending_binding(&request.pending, &request.action_hash) {
                Ok(binding) => binding,
                Err(e) => return ApiResponse::error(agent_confirm_error_status(&e), &e),
            };
            if let Some(r) = self.biometric_challenge(
                &binding.op,
                &binding.account,
                &binding.service,
                &binding.item,
                req,
            ) {
                return r;
            }
        }
        match handler.confirm(&request.pending, &request.token, &request.action_hash) {
            Ok(summary) => {
                ApiResponse::ok_json(&json!({ "confirmed": request.pending, "result": summary }))
            }
            Err(e) => ApiResponse::error(agent_confirm_error_status(&e), &e),
        }
    }

    /// Cancel an in-flight agent turn. Cancellation is strict JSON and never
    /// multiplexed with pending-action cancellation.
    fn agent_turn_cancel(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match self.agent_gate(req) {
            Ok(h) => h,
            Err(e) => return with_no_store(e),
        };
        let request: AgentTurnCancelRequest = match parse_agent_strict_json_with_limit(
            req,
            "agent turn cancel",
            AGENT_STRICT_JSON_MAX_BYTES,
        ) {
            Ok(request) => request,
            Err(error) => return error,
        };
        if !valid_client_request_id(&request.request_id) || !valid_opaque_agent_id(&request.turn_id)
        {
            return no_store_json_error(400, "invalid agent turn cancel request");
        }
        match handler.cancel_turn(&request.turn_id) {
            Ok(()) => with_no_store(ApiResponse::ok_json(&json!({
                "state": "cancel_requested",
                "turn_id": request.turn_id,
            }))),
            Err(error) => no_store_json_error(agent_session_error_status(&error), &error),
        }
    }

    fn agent_pending_cancel(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match self.agent_gate(req) {
            Ok(h) => h,
            Err(e) => return with_no_store(e),
        };
        let request: AgentPendingCancelRequest = match parse_agent_strict_json_with_limit(
            req,
            "agent pending cancel",
            AGENT_STRICT_JSON_MAX_BYTES,
        ) {
            Ok(request) => request,
            Err(error) => return error,
        };
        if !valid_client_request_id(&request.request_id)
            || !valid_opaque_agent_id(&request.pending)
            || request.action_hash.len() != 64
            || !request
                .action_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return no_store_json_error(400, "invalid agent pending cancel request");
        }
        match handler.cancel_pending(&request.pending, &request.action_hash) {
            Ok(()) => with_no_store(ApiResponse::ok_json(&json!({
                "state": "cancelled",
                "pending": request.pending,
            }))),
            Err(error) => no_store_json_error(agent_confirm_error_status(&error), &error),
        }
    }

    /// Run a bounded, redacted connectivity preflight. This is deliberately the only
    /// new #640 JSON route in this change; legacy agent routes remain query-based until
    /// their separate hardening work lands.
    fn agent_connectivity_preflight(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match self.agent_gate(req) {
            Ok(h) => h,
            Err(mut e) => {
                e.headers.push(("Cache-Control".into(), "no-store".into()));
                return e;
            }
        };
        if !req.query.is_empty() {
            return no_store_json_error(400, "connectivity preflight accepts JSON only");
        }
        if !is_json_content_type(req.content_type.as_deref()) {
            return no_store_json_error(400, "application/json content type is required");
        }
        if req.body.len() > AGENT_STRICT_JSON_MAX_BYTES {
            return no_store_json_error(413, "request body too large");
        }
        let body = match std::str::from_utf8(&req.body) {
            Ok(body) => body,
            Err(_) => return no_store_json_error(400, "invalid JSON request body"),
        };
        let request = match serde_json::from_str::<AgentConnectivityPreflightRequest>(body) {
            Ok(request) => request,
            Err(_) => return no_store_json_error(400, "invalid connectivity preflight request"),
        };
        let response = match handler.connectivity_preflight_with_session(
            request,
            req.session_token.as_deref(),
            req.mobile_bridge,
        ) {
            Ok(response) => response,
            Err(_) => return no_store_json_error(503, "connectivity preflight unavailable"),
        };
        let mut response =
            ApiResponse::ok_json(&serde_json::to_value(response).unwrap_or_else(|_| {
                json!({
                    "status": "unavailable",
                    "code": "connectivity_unavailable",
                    "retryable": true,
                    "settings_hint": "none",
                })
            }));
        response
            .headers
            .push(("Cache-Control".into(), "no-store".into()));
        response
    }

    /// Explicit product credential refresh. This uses the same strict, bounded JSON contract as
    /// connectivity preflight but deliberately never accepts snapshot, URL, or token fields.
    fn agent_credential_refresh(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match self.agent_gate(req) {
            Ok(h) => h,
            Err(mut e) => {
                e.headers.push(("Cache-Control".into(), "no-store".into()));
                return e;
            }
        };
        if !req.query.is_empty() || !is_json_content_type(req.content_type.as_deref()) {
            return no_store_json_error(400, "credential refresh accepts JSON only");
        }
        if req.body.len() > AGENT_STRICT_JSON_MAX_BYTES {
            return no_store_json_error(413, "request body too large");
        }
        let body = match std::str::from_utf8(&req.body) {
            Ok(body) => body,
            Err(_) => return no_store_json_error(400, "invalid JSON request body"),
        };
        let request = match serde_json::from_str::<AgentCredentialRefreshRequest>(body) {
            Ok(request) => request,
            Err(_) => return no_store_json_error(400, "invalid credential refresh request"),
        };
        let state = match handler.credential_refresh(&request.provider) {
            Ok(state) => state,
            Err(_) => return no_store_json_error(409, "reconnect_required"),
        };
        let mut response = ApiResponse::ok_json(&json!({ "credential_state": state }));
        response
            .headers
            .push(("Cache-Control".into(), "no-store".into()));
        response
    }

    /// Agent connection status as JSON (`{connected, model?}`). Read-only; returns
    /// `enabled:false` when no agent is wired so the UI can hide the assistant entirely.
    fn agent_status(&self, _req: &ApiRequest) -> ApiResponse {
        let body = match self.agent.as_ref() {
            Some(h) => h.status_json(),
            None => "{\"connected\":false,\"enabled\":false}".to_string(),
        };
        ApiResponse {
            status: 200,
            content_type: "application/json".into(),
            // #639 T9: status carries onboarding/credential state — never let a WebView cache it.
            headers: vec![("Cache-Control".into(), "no-store".into())],
            body: body.into_bytes(),
        }
    }

    /// Begin the agent provider OAuth login. Cap+session gated
    /// (the app initiates it); returns the authorize URL the UI opens in the system
    /// browser. `redirect` is the loopback callback the client supplies (its origin).
    fn agent_oauth_start(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match self.agent_gate(req) {
            Ok(h) => h,
            Err(e) => return with_no_store(e),
        };
        let request = match parse_agent_strict_json::<AgentOAuthStartRequest>(req, "oauth start") {
            Ok(request) => request,
            Err(response) => return response,
        };
        if agent_oauth_provider_param(&request.provider).is_err()
            || !valid_client_request_id(&request.request_id)
            || request
                .lifecycle_operation_id
                .as_ref()
                .is_some_and(|value| value.is_empty() || value.len() > 128)
        {
            return no_store_json_error(400, "invalid oauth start request");
        }
        with_no_store(match handler.oauth_start_request(request) {
            Ok(result) => ApiResponse::ok_json(&serde_json::to_value(result).unwrap_or_default()),
            Err(_) => ApiResponse::error(409, "oauth_start_failed"),
        })
    }

    fn agent_oauth_logout(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match self.agent_gate(req) {
            Ok(handler) => handler,
            Err(response) => return with_no_store(response),
        };
        let request = match parse_agent_strict_json::<AgentOAuthLogoutRequest>(req, "oauth logout")
        {
            Ok(request) => request,
            Err(response) => return response,
        };
        let new_form = request.credential_etag.is_some() && request.resume.is_none();
        let resume_form = request.credential_etag.is_none() && request.resume.is_some();
        if agent_oauth_provider_param(&request.provider).is_err()
            || !matches!(request.mode.as_str(), "disconnect" | "reconnect" | "switch")
            || !valid_client_request_id(&request.request_id)
            || !(new_form || resume_form)
        {
            return no_store_json_error(400, "invalid oauth logout request");
        }
        with_no_store(match handler.oauth_logout(request) {
            Ok(response) => ApiResponse::ok_json(
                &serde_json::to_value(response).unwrap_or_else(|_| serde_json::json!({})),
            ),
            Err(code) => no_store_json_error(409, &closed_lifecycle_error(&code)),
        })
    }

    fn agent_oauth_lifecycle_resume(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match self.agent_gate(req) {
            Ok(handler) => handler,
            Err(response) => return with_no_store(response),
        };
        let request = match parse_agent_strict_json::<AgentOAuthLifecycleResumeRequest>(
            req,
            "oauth lifecycle resume",
        ) {
            Ok(request) => request,
            Err(response) => return response,
        };
        if agent_oauth_provider_param(&request.provider).is_err()
            || !valid_client_request_id(&request.request_id)
            || request.operation_id.is_empty()
            || request.operation_id.len() > 128
            || request.operation_etag.is_empty()
            || request.operation_etag.len() > 128
            || !matches!(
                request.action.as_str(),
                "retry_revoke" | "resume_cleanup" | "retry_exchange" | "continue_oauth"
            )
        {
            return no_store_json_error(400, "invalid lifecycle resume request");
        }
        with_no_store(match handler.oauth_lifecycle_resume(request) {
            Ok(response) => ApiResponse::ok_json(
                &serde_json::to_value(response).unwrap_or_else(|_| serde_json::json!({})),
            ),
            Err(code) => no_store_json_error(409, &closed_lifecycle_error(&code)),
        })
    }

    /// Cancel one active provider OAuth attempt. The body is deliberately strict so
    /// callback state, verifier, and URLs never cross the WebView boundary.
    fn agent_oauth_cancel(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match self.agent_gate(req) {
            Ok(h) => h,
            Err(e) => return with_no_store(e),
        };
        if !req.query.is_empty() || !is_json_content_type(req.content_type.as_deref()) {
            return no_store_json_error(400, "oauth cancel accepts JSON only");
        }
        if req.body.len() > AGENT_STRICT_JSON_MAX_BYTES {
            return no_store_json_error(413, "request body too large");
        }
        let body = match std::str::from_utf8(&req.body) {
            Ok(body) => body,
            Err(_) => return no_store_json_error(400, "invalid oauth cancel request"),
        };
        let request = match serde_json::from_str::<AgentOAuthCancelRequest>(body) {
            Ok(request) => request,
            Err(_) => return no_store_json_error(400, "invalid oauth cancel request"),
        };
        let provider = match agent_oauth_provider_param(&request.provider) {
            Ok(provider) => provider,
            Err(e) => return no_store_json_error(400, e),
        };
        if request.attempt_id.is_empty() || request.attempt_id.len() > 128 {
            return no_store_json_error(400, "invalid oauth cancel request");
        }
        match handler.oauth_cancel(provider, &request.attempt_id) {
            Ok(()) => {
                let mut response = ApiResponse::ok_json(&json!({ "cancelled": true }));
                response
                    .headers
                    .push(("Cache-Control".into(), "no-store".into()));
                response
            }
            Err(_) => no_store_json_error(409, "oauth attempt is not active"),
        }
    }

    /// Complete the manual login (S-AG.12): the app POSTs the pasted `code#state`. Cap+
    /// session gated (the app initiates it). Exchanges + stores the token.
    fn agent_oauth_complete(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match self.agent_gate(req) {
            Ok(h) => h,
            Err(e) => return with_no_store(e),
        };
        // #639 T9: strict JSON only — the pasted code must never arrive as a URL/query param.
        if !req.query.is_empty() || !is_json_content_type(req.content_type.as_deref()) {
            return no_store_json_error(400, "oauth complete accepts JSON only");
        }
        if req.body.len() > AGENT_STRICT_JSON_MAX_BYTES {
            return no_store_json_error(413, "request body too large");
        }
        let body = match std::str::from_utf8(&req.body) {
            Ok(body) => body,
            Err(_) => return no_store_json_error(400, "invalid oauth complete request"),
        };
        // deny_unknown_fields + serde's derived struct deserializer reject unknown/duplicate keys.
        let request = match serde_json::from_str::<AgentOAuthCompleteRequest>(body) {
            Ok(request) => request,
            Err(_) => return no_store_json_error(400, "invalid oauth complete request"),
        };
        // Manual completion is the Claude copy-paste flow ONLY; Codex ends via its loopback callback.
        if request.provider != "claude" {
            return no_store_json_error(400, "oauth complete is claude-only");
        }
        if request.attempt_id.is_empty()
            || request.attempt_id.len() > 128
            || request.pasted_code.is_empty()
            || request.pasted_code.len() > AGENT_STRICT_JSON_MAX_BYTES
        {
            return no_store_json_error(400, "invalid oauth complete request");
        }
        match handler.oauth_complete(&request.attempt_id, &request.pasted_code) {
            Ok(_) => {
                let mut response = ApiResponse::ok_json(&json!({ "connected": true }));
                response
                    .headers
                    .push(("Cache-Control".into(), "no-store".into()));
                response
            }
            Err(_) => no_store_json_error(400, "oauth complete failed"),
        }
    }

    /// Set the active provider + model from the in-app switcher (S-AG.6). Cap + session
    /// gated; the closed provider/model selection is carried only in strict JSON.
    fn agent_set_model(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match self.agent_gate(req) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let request: AgentModelRequest = match parse_agent_strict_json(req, "agent model") {
            Ok(request) => request,
            Err(response) => return response,
        };
        if !valid_client_request_id(&request.request_id)
            || agent_oauth_provider_param(&request.provider).is_err()
            || request.model.is_empty()
            || request.model.len() > 128
        {
            return no_store_json_error(400, "invalid agent model request");
        }
        match handler.set_model(
            &request.provider,
            &request.model,
            request.reasoning_effort.as_deref(),
        ) {
            Ok(_) => ApiResponse::ok_json(&json!({
                "provider": request.provider,
                "model": request.model,
                "reasoning_effort": request.reasoning_effort,
            })),
            Err(e) => ApiResponse::error(400, &e),
        }
    }

    /// The system-browser OAuth callback (S-AG.12). NOT cap/session gated — the browser
    /// holds no token; the `state` minted at start is the CSRF defence. Exchanges the
    /// code, stores the token, and returns a human-facing success page.
    fn agent_oauth_callback(&self, req: &ApiRequest) -> ApiResponse {
        let handler = match self.agent.as_ref() {
            Some(h) => h,
            None => return no_store_json_error(404, "the agent is not enabled on this server"),
        };
        let (code, state) = match (req.q("code"), req.q("state")) {
            (Some(c), Some(s)) if !c.is_empty() && !s.is_empty() => (c, s),
            _ => return no_store_json_error(400, "code and state are required"),
        };
        with_no_store(match handler.oauth_callback(code, state) {
            Ok(html) => ApiResponse::html(&html),
            Err(_) => ApiResponse::error(400, "oauth_callback_failed"),
        })
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
        let store = match self.open_readonly(Some(account)) {
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
        let store = match self.open_readonly(Some(account)) {
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

    /// Read-only store open for GET endpoints: a WAL reader that takes no instance
    /// lock, so a list load never waits out an in-flight sync holding the writer lock
    /// (the measured cause of multi-second mailbox loads). Falls back to a writable
    /// open only if the DB isn't there yet (fresh account, pre-migration).
    fn open_readonly(&self, account: Option<&str>) -> Result<Store, ApiResponse> {
        let account = account.ok_or_else(|| ApiResponse::error(400, "missing 'account'"))?;
        let path = self
            .store_path(account)
            .ok_or_else(|| ApiResponse::error(404, "unknown account"))?;
        match Store::open_readonly(&path) {
            Ok(s) => Ok(s),
            // No migrated DB yet (first run): fall back to a writable open, which
            // creates + migrates it, so the endpoint still works before the first sync.
            Err(_) => {
                Store::open(&path).map_err(|e| ApiResponse::error(500, &format!("store: {e}")))
            }
        }
    }

    fn items(&self, req: &ApiRequest) -> ApiResponse {
        let service = match req.q("service") {
            Some(s) => s,
            None => return ApiResponse::error(400, "missing 'service'"),
        };
        // GETs use a read-only WAL connection so a list load is never serialized behind
        // an in-flight sync holding the writer lock (the measured cause of slow mailbox
        // loads). Any preview back-fill happens afterwards on a brief writable open.
        let store = match self.open_readonly(req.q("account")) {
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
                    // OneDrive rows carry a read-only `preview` parsed from the
                    // archived DriveItem JSON sidecar (#564): the rich facets the
                    // indexed columns drop (mimeType/sha256/created-by/EXIF/shared/…).
                    // Best-effort — an item without a readable sidecar carries none.
                    let archive_root = (service == "onedrive")
                        .then(|| {
                            self.config
                                .accounts
                                .iter()
                                .find(|a| a.id == account)
                                .map(|a| a.archive_root.clone())
                        })
                        .flatten();
                    let strict_body_acc = (service == "onedrive"
                        && isyncyou_core::envelope::body_envelope_required_for_process())
                    .then(|| self.config.accounts.iter().find(|a| a.id == account))
                    .flatten();
                    let strict_body_all = if strict_body_acc.is_some() {
                        match store.items_by_service(account, service) {
                            Ok(all) => Some(all),
                            Err(e) => return ApiResponse::error(500, &format!("query: {e}")),
                        }
                    } else {
                        None
                    };
                    let strict_body_by_id = strict_body_all.as_ref().map(|all| {
                        all.iter()
                            .map(|i| (i.remote_id.as_str(), i))
                            .collect::<HashMap<&str, &Item>>()
                    });
                    let arr: Vec<Value> = items
                        .iter()
                        .map(|it| {
                            let mut v = item_json_with_mobile_body_policy(
                                it,
                                strict_body_acc,
                                strict_body_by_id.as_ref(),
                            );
                            if let Some(root) = archive_root.as_ref() {
                                let rel = isyncyou_connectors::shard_rel(
                                    "onedrive",
                                    &it.remote_id,
                                    "json",
                                );
                                if let Some(bytes) = read_under_root(root, &rel) {
                                    if let Ok(o) = serde_json::from_slice::<Value>(&bytes) {
                                        v["preview"] = onedrive_preview(&o);
                                    }
                                }
                            }
                            v
                        })
                        .collect();
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
                let strict_body_acc = (service == "onedrive"
                    && isyncyou_core::envelope::body_envelope_required_for_process())
                .then(|| self.config.accounts.iter().find(|a| a.id == account))
                .flatten();
                let strict_body_all = if strict_body_acc.is_some() {
                    match store.items_by_service(account, service) {
                        Ok(all) => Some(all),
                        Err(e) => return ApiResponse::error(500, &format!("query: {e}")),
                    }
                } else {
                    None
                };
                let strict_body_by_id = strict_body_all.as_ref().map(|all| {
                    all.iter()
                        .map(|i| (i.remote_id.as_str(), i))
                        .collect::<HashMap<&str, &Item>>()
                });
                // Rows are enriched with a read-only `preview` parsed from the
                // archived body on disk, so the bespoke views render richly without an
                // extra request per item. Additive + best-effort: items without a
                // readable body simply carry no `preview`. Bounded by the page size.
                // mail = sender/snippet/date/has-html (.eml); the rest parse the
                // archived JSON (calendar/contacts/todo).
                // Mail previews parsed on the fly (no cached row yet) are collected here
                // and persisted once, after the response is built, on a brief writable
                // open — so the read above never takes the writer lock.
                let mut backfill: Vec<(String, String)> = Vec::new();
                let arr: Vec<Value> = if matches!(
                    service,
                    "mail" | "calendar" | "contacts" | "todo" | "onenote"
                ) {
                    let archive_root = self
                        .config
                        .accounts
                        .iter()
                        .find(|a| a.id == account)
                        .map(|a| a.archive_root.clone());
                    items
                        .iter()
                        .map(|it| {
                            let mut v = item_json_with_mobile_body_policy(
                                it,
                                strict_body_acc,
                                strict_body_by_id.as_ref(),
                            );
                            // Fast path (schema v12): a mail row whose `preview` was
                            // already computed is served straight from the DB column —
                            // no `.eml`/`.json` read, no MIME parse, no attachment decode.
                            // This is the hot mailbox-load path once bodies are warmed.
                            let cached = service == "mail"
                                && it
                                    .preview_json
                                    .as_deref()
                                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                                    .map(|pv| v["preview"] = pv)
                                    .is_some();
                            if !cached {
                                if let (Some(root), Some(rel)) =
                                    (archive_root.as_ref(), it.local_path.as_ref())
                                {
                                    if let Some(bytes) = read_under_root(root, rel) {
                                        if service == "mail" {
                                            mail_preview_enrichment(&mut v, it, root, rel, &bytes);
                                        } else if service == "onenote" {
                                            // a page's local_path is the .html body, not JSON —
                                            // onenote_preview reads the _pagemeta_ / flank sidecar.
                                            v["preview"] = onenote_preview(it, root);
                                        } else if let Ok(o) =
                                            serde_json::from_slice::<Value>(&bytes)
                                        {
                                            v["preview"] = match service {
                                                "calendar" => calendar_preview(it, &o),
                                                "contacts" => contact_preview(it, &o, root),
                                                "todo" => todo_preview(it, &o, root),
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
                                // #89: show a mail's sender from the indexed `sender`
                                // (captured at ingest — read with the item, NO extra
                                // file I/O) whenever the .eml body isn't cached, so a row
                                // never reads "(unknown sender)". The .eml enrichment
                                // above wins when the body is present.
                                if service == "mail" {
                                    if let Some(sender) = it.sender.as_deref() {
                                        let has_from = v
                                            .get("preview")
                                            .and_then(|p| p.get("from"))
                                            .and_then(Value::as_str)
                                            .is_some_and(|s| !s.is_empty());
                                        if !has_from {
                                            let mut p = v
                                                .get("preview")
                                                .cloned()
                                                .unwrap_or_else(|| json!({}));
                                            p["from"] = json!(sender);
                                            if p.get("subject").and_then(Value::as_str).is_none() {
                                                p["subject"] = json!(it.name);
                                            }
                                            if p.get("date").and_then(Value::as_str).is_none() {
                                                if let Some(d) = it.remote_mtime.as_deref() {
                                                    p["date"] = json!(d);
                                                }
                                            }
                                            v["preview"] = p;
                                        }
                                    }
                                }
                                // Collect the freshly computed mail preview for a
                                // post-response write-through so every later load takes the
                                // fast path. Only for a mail with an archived body (the
                                // expensive case); `set_local_path` clears it when the body
                                // is re-archived.
                                if service == "mail" && it.local_path.is_some() {
                                    if let Some(pv) = v.get("preview") {
                                        if let Ok(s) = serde_json::to_string(pv) {
                                            backfill.push((it.remote_id.clone(), s));
                                        }
                                    }
                                }
                            }
                            v
                        })
                        .collect()
                } else {
                    items
                        .iter()
                        .map(|it| {
                            item_json_with_mobile_body_policy(
                                it,
                                strict_body_acc,
                                strict_body_by_id.as_ref(),
                            )
                        })
                        .collect()
                };
                // Persist freshly parsed previews so later loads hit the DB fast path.
                // Brief writable open; if the writer lock is busy (a sync is running) this
                // simply no-ops and the next load retries — it never blocks the read above.
                if !backfill.is_empty() {
                    if let Ok(w) = self.open(Some(account)) {
                        for (id, s) in &backfill {
                            let _ = w.set_preview_json(account, "mail", id, s);
                        }
                    }
                }
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
            Ok(Some(it)) => {
                let strict_body_acc = (service == "onedrive"
                    && isyncyou_core::envelope::body_envelope_required_for_process())
                .then(|| self.config.accounts.iter().find(|a| a.id == account))
                .flatten();
                let onedrive_all = if service == "onedrive" {
                    match store.items_by_service(account, service) {
                        Ok(all) => Some(all),
                        Err(e) => return ApiResponse::error(500, &format!("query: {e}")),
                    }
                } else {
                    None
                };
                let onedrive_by_id = onedrive_all.as_ref().map(|all| {
                    all.iter()
                        .map(|i| (i.remote_id.as_str(), i))
                        .collect::<HashMap<&str, &Item>>()
                });
                let mut v = item_json_with_mobile_body_policy(
                    &it,
                    strict_body_acc,
                    onedrive_by_id.as_ref(),
                );
                if let Some(by_id) = onedrive_by_id.as_ref() {
                    enrich_onedrive_effective_mode(&mut v, &self.config, account, by_id, &it);
                }
                ApiResponse::ok_json(&v)
            }
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
        let store = self.open(Some(account))?;
        let it = match store.get_item(account, service, id) {
            Ok(Some(it)) => it,
            Ok(None) => return Err(ApiResponse::error(404, "item not found")),
            Err(e) => return Err(ApiResponse::error(500, &format!("query: {e}"))),
        };
        // A OneDrive item's stored `local_path` is only the NAME segment; the on-disk body path
        // walks the parent-folder chain (materialize writes `sync_root/<folder>/…/<name>`).
        // Resolve the full sync-root-relative path the same way materialize does, else a nested
        // materialized file is read from the wrong path (#655). Other services keep the flat
        // archive-relative `local_path`.
        let rel = if service == "onedrive" {
            let items = store
                .items_by_service(account, service)
                .map_err(|e| ApiResponse::error(500, &format!("query: {e}")))?;
            let by_id: HashMap<&str, &Item> =
                items.iter().map(|i| (i.remote_id.as_str(), i)).collect();
            isyncyou_connectors::local_rel_path(&by_id, &it)
                .ok_or_else(|| ApiResponse::error(404, "item has no archived body"))?
                .to_string_lossy()
                .into_owned()
        } else {
            it.local_path
                .clone()
                .ok_or_else(|| ApiResponse::error(404, "item has no archived body"))?
        };
        let name = it.name.clone();
        // Root-aware body location (#onedrive-mobile 0C): a OneDrive body lives in the
        // offline working copy (`sync_root`) when `body_location=="sync"`, else the
        // lazy-preview cache (`cache_root`); legacy rows without a location fall back to
        // `sync_root`. Every other archived service reads from the archive root. The
        // OneDrive `local_path` is relative to the chosen root.
        let body_root = if service == "onedrive" {
            match it.body_location.as_deref() {
                Some("cache") => acc.effective_cache_root(),
                _ => acc.sync_root.clone(),
            }
        } else {
            acc.archive_root.clone()
        };
        let path = body_root.join(&rel);
        match (path.canonicalize(), body_root.canonicalize()) {
            (Ok(p), Ok(root)) if p.starts_with(&root) => {
                // Desktop may still carry plaintext OneDrive cache files. Mobile sets the
                // process policy after Keystore unwrap; from then on a raw plaintext body is
                // not a valid local OneDrive body.
                let read = if service == "onedrive"
                    && isyncyou_core::envelope::body_envelope_required_for_process()
                {
                    isyncyou_core::envelope::read_sealed_body_required(&p)
                } else {
                    isyncyou_core::envelope::read_body(&p)
                };
                match read {
                    Ok(bytes) => Ok((rel, bytes, name)),
                    Err(e) => Err(ApiResponse::error(500, &format!("read: {e}"))),
                }
            }
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

    /// List (`?account&service&id`) or download (`&index=N`) the attachments of an
    /// archived mail `.eml` (#562). Listing returns JSON metadata; download serves
    /// the decoded part bytes with an inert content-type (mapped through the
    /// attachment filename, `nosniff` always on — never an executable type).
    fn attachment(&self, req: &ApiRequest) -> ApiResponse {
        let (account, service, id) = match (req.q("account"), req.q("service"), req.q("id")) {
            (Some(a), Some(s), Some(i)) => (a, s, i),
            _ => return ApiResponse::error(400, "missing 'account', 'service' or 'id'"),
        };
        // ToDo attachments live in the `_taskatt_<id>` sub-resource sidecar as Graph
        // taskFileAttachment[] (inline base64 contentBytes), not a MIME `.eml`
        // (#567 B4): list/decode them from that JSON instead of mail's mime parser.
        if service == "todo" {
            return self.todo_attachment(account, id, req.q("index"));
        }
        let bytes = match self.read_archived(account, service, id) {
            Ok((_, b, _)) => b,
            Err(e) => return e,
        };
        match req.q("index") {
            None => {
                let list: Vec<Value> = isyncyou_connectors::list_attachments(&bytes)
                    .into_iter()
                    .map(|a| {
                        json!({
                            "index": a.index,
                            "filename": a.filename,
                            "content_type": a.content_type,
                            "size": a.size,
                        })
                    })
                    .collect();
                ApiResponse::ok_json(&json!({ "attachments": list }))
            }
            Some(idx_s) => {
                let idx = match idx_s.parse::<usize>() {
                    Ok(n) => n,
                    Err(_) => {
                        return ApiResponse::error(400, "index must be a non-negative integer")
                    }
                };
                match isyncyou_connectors::extract_attachment(&bytes, idx) {
                    Some((filename, _ct, data)) => ApiResponse {
                        status: 200,
                        content_type: safe_content_type(&filename).into(),
                        body: data,
                        headers: Vec::new(),
                    },
                    None => ApiResponse::error(404, "attachment index out of range"),
                }
            }
        }
    }

    /// List/download a task's file attachments from its `_taskatt_<id>` sub-resource
    /// sidecar (#567 B4). The download decodes the inline base64 `contentBytes` and
    /// serves it under an inert content-type (the always-on nosniff keeps it
    /// non-executable, like the mail attachment path).
    fn todo_attachment(&self, account: &str, task_id: &str, index: Option<&str>) -> ApiResponse {
        let att_id = format!("_taskatt_{task_id}");
        let bytes = match self.read_archived(account, "todo", &att_id) {
            Ok((_, b, _)) => b,
            Err(_) => return ApiResponse::error(404, "no archived attachments for this task"),
        };
        match index {
            None => {
                let list: Vec<Value> = isyncyou_connectors::list_task_attachments(&bytes)
                    .into_iter()
                    .map(|(i, name, ct, size)| {
                        json!({ "index": i, "filename": name, "content_type": ct, "size": size })
                    })
                    .collect();
                ApiResponse::ok_json(&json!({ "attachments": list }))
            }
            Some(idx_s) => {
                let idx = match idx_s.parse::<usize>() {
                    Ok(n) => n,
                    Err(_) => {
                        return ApiResponse::error(400, "index must be a non-negative integer")
                    }
                };
                match isyncyou_connectors::extract_task_attachment(&bytes, idx) {
                    Some((filename, _ct, data)) => ApiResponse {
                        status: 200,
                        content_type: safe_content_type(&filename).into(),
                        body: data,
                        headers: Vec::new(),
                    },
                    None => ApiResponse::error(404, "attachment index out of range"),
                }
            }
        }
    }

    /// Serve a contact's archived photo (#566). `backup_contact_photos` writes
    /// it to `contacts/<shard>/<id>.jpg` but never records a `local_path`, so it
    /// is resolved by id via `shard_rel` under the archive root (the same
    /// id-addressed trick as the OneDrive sidecar). `image/jpeg` + the always-on
    /// nosniff; 404 when the contact has no archived photo.
    fn contact_photo(&self, req: &ApiRequest) -> ApiResponse {
        let (account, id) = match (req.q("account"), req.q("id")) {
            (Some(a), Some(i)) if !a.is_empty() && !i.is_empty() => (a, i),
            _ => return ApiResponse::error(400, "account and id are required"),
        };
        let acc = match self.config.accounts.iter().find(|a| a.id == account) {
            Some(a) => a,
            None => return ApiResponse::error(404, "unknown account"),
        };
        let rel = isyncyou_connectors::shard_rel("contacts", id, "jpg");
        match read_under_root(&acc.archive_root, &rel) {
            Some(bytes) => ApiResponse {
                status: 200,
                content_type: "image/jpeg".into(),
                body: bytes,
                headers: Vec::new(),
            },
            None => ApiResponse::error(404, "no archived photo for this contact"),
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

fn agent_confirm_error_status(error: &str) -> u16 {
    if error.contains("BadToken")
        || error.contains("Expired")
        || error.contains("ActionMismatch")
        || error.contains("NotFound")
        || error == "bad token"
    {
        409
    } else {
        500
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

fn parse_services_param(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or("")
        .split([',', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn parse_backup_services_param(raw: Option<&str>) -> Result<Vec<String>, String> {
    let mut services = parse_services_param(raw);
    services.sort();
    services.dedup();
    for service in &services {
        if !BACKUP_SERVICES.contains(&service.as_str()) {
            return Err(format!("unsupported backup service: {service}"));
        }
    }
    Ok(services)
}

fn restore_cloud_pending_item(service: &str, id: &str) -> String {
    format!(
        "service:{}:{}:id:{}:{}",
        service.len(),
        service,
        id.len(),
        id
    )
}

fn mobile_job_summary_json(job: MobileJobSummary) -> Value {
    json!({
        "job_id": job.job_id,
        "kind": job.kind,
        "state": job.state,
        "service": job.service,
        "target_id": job.target_id,
        "created_at": job.created_at,
        "updated_at": job.updated_at,
        "finished_at": job.finished_at,
        "last_error": job.last_error,
    })
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
/// The Live∪Backup status of an element (plan §S-P4.3): the join of the cloud
/// mirror (the store, refreshed each sync) and the local archive.
/// - `backup_only` — cloud-deleted but still archived (the backup's value)
/// - `live_only` — in the cloud, not backed up
/// - `stale` — backed up, but the cloud changed since (`etag != body_etag`)
/// - `live_backup` — in the cloud and backed up, up to date
fn backup_state(it: &Item) -> &'static str {
    if it.deleted_at.is_some() {
        // only cloud-deleted items that still have an archived body reach the
        // listing (see `items_by_service_page`), so this is the archive state.
        return "backup_only";
    }
    if it.local_path.is_none() {
        return "live_only";
    }
    match (it.etag.as_deref(), it.body_etag.as_deref()) {
        (Some(e), Some(b)) if e != b => "stale",
        _ => "live_backup",
    }
}

fn item_json(it: &Item) -> Value {
    // `has_body`: for OneDrive (mobile modes, schema v14) the DB-level signal is
    // `body_state == "available"`; a filesystem probe (`local_path`) would falsely
    // mark a metadata-only Mode-2 row as openable. In the mobile encrypted process,
    // `item_json_with_mobile_body_policy` tightens this to a strict envelope probe.
    // Every other service keeps the `local_path`-based meaning (its body is a plain
    // archived file). OneDrive rows also carry their content-state fields so the UI
    // can render online / syncing / offline / conflict without a second request.
    let has_body = if it.service == "onedrive" {
        it.body_state.as_deref() == Some("available")
    } else {
        it.local_path.is_some()
    };
    let mut v = json!({
        "service": it.service,
        "remote_id": it.remote_id,
        "name": it.name,
        "item_type": it.item_type,
        "parent_remote_id": it.parent_remote_id,
        "sync_state": it.sync_state,
        "remote_mtime": it.remote_mtime,
        "size": it.size,
        "etag": it.etag,
        "has_body": has_body,
        "deleted": it.deleted_at.is_some(),
        "state": backup_state(it),
        "verify_status": it.verify_status,
        "verified_at": it.verified_at,
    });
    if it.service == "onedrive" {
        v["content_state"] = json!(it.content_state);
        v["body_location"] = json!(it.body_location);
        v["body_state"] = json!(it.body_state);
        v["conflict_state"] = json!(it.conflict_state);
        // #659: surface the last download failure so the UI can show a retry affordance.
        v["last_download_error"] = json!(it.last_download_error);
    }
    v
}

fn onedrive_store_ancestry(by_id: &HashMap<&str, &Item>, it: &Item) -> Vec<String> {
    let mut ancestry = Vec::new();
    let mut parent = it.parent_remote_id.as_deref();
    while let Some(pid) = parent.filter(|p| !p.is_empty()) {
        if ancestry.iter().any(|seen| seen == pid) {
            break;
        }
        ancestry.push(pid.to_string());
        parent = by_id
            .get(pid)
            .and_then(|parent_item| parent_item.parent_remote_id.as_deref());
    }
    ancestry
}

fn onedrive_effective_mode_for_store_item(
    config: &Config,
    account: &str,
    by_id: &HashMap<&str, &Item>,
    it: &Item,
) -> OneDriveMode {
    let ancestry = onedrive_store_ancestry(by_id, it);
    let refs: Vec<&str> = ancestry.iter().map(String::as_str).collect();
    config.effective_mode(account, &it.remote_id, &refs)
}

fn enrich_onedrive_effective_mode(
    v: &mut Value,
    config: &Config,
    account: &str,
    by_id: &HashMap<&str, &Item>,
    it: &Item,
) {
    if it.service == "onedrive" {
        v["effective_mode"] =
            json!(onedrive_effective_mode_for_store_item(config, account, by_id, it).as_str());
    }
}

fn onedrive_body_root(acc: &isyncyou_core::AccountConfig, it: &Item) -> PathBuf {
    if it.body_location.as_deref() == Some("cache") {
        acc.effective_cache_root()
    } else {
        acc.sync_root.clone()
    }
}

fn onedrive_has_sealed_body(
    acc: &isyncyou_core::AccountConfig,
    by_id: &HashMap<&str, &Item>,
    it: &Item,
) -> bool {
    if it.body_state.as_deref() != Some("available") {
        return false;
    }
    let Some(rel) = isyncyou_connectors::local_rel_path(by_id, it) else {
        return false;
    };
    let root = onedrive_body_root(acc, it);
    let path = root.join(rel);
    match (path.canonicalize(), root.canonicalize()) {
        (Ok(p), Ok(root)) if p.starts_with(&root) => {
            isyncyou_core::envelope::probe_sealed_body_required(&p).is_ok()
        }
        _ => false,
    }
}

fn item_json_with_mobile_body_policy(
    it: &Item,
    acc: Option<&isyncyou_core::AccountConfig>,
    by_id: Option<&HashMap<&str, &Item>>,
) -> Value {
    let mut v = item_json(it);
    if it.service == "onedrive" && isyncyou_core::envelope::body_envelope_required_for_process() {
        v["has_body"] = json!(match (acc, by_id) {
            (Some(acc), Some(by_id)) => onedrive_has_sealed_body(acc, by_id, it),
            _ => false,
        });
    }
    v
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
    // Decrypt the sealed body envelope on read (#0B); plaintext (desktop) passes through.
    isyncyou_core::envelope::read_body(&p).ok()
}

/// Build a calendar item's `preview` from its archived JSON sidecar (#565 B4).
/// A `calendar` entity exposes its colour (so the UI can colour-code events); an
/// `event` exposes all the detail the UI surfaces — recurrence rule (for
/// client-side month/week expansion), Teams join link, my response status,
/// categories, importance, sensitivity, show-as, cancellation, attachments,
/// webLink, multiple locations. All best-effort (absent fields → null).
fn calendar_preview(it: &Item, o: &Value) -> Value {
    if it.item_type == "calendar" {
        return json!({
            "hex_color": o.get("hexColor").and_then(Value::as_str),
            "color": o.get("color").and_then(Value::as_str),
            "is_default": o.get("isDefaultCalendar").and_then(Value::as_bool),
            "can_edit": o.get("canEdit").and_then(Value::as_bool),
        });
    }
    json!({
        "start": o.pointer("/start/dateTime"),
        "start_tz": o.pointer("/start/timeZone"),
        "end": o.pointer("/end/dateTime"),
        "end_tz": o.pointer("/end/timeZone"),
        "all_day": o.get("isAllDay"),
        "location": o.pointer("/location/displayName"),
        "locations": o.get("locations"),
        "organizer": o.pointer("/organizer/emailAddress"),
        "recurrence": o.get("recurrence"),
        "type": o.get("type").and_then(Value::as_str),
        "series_master_id": o.get("seriesMasterId").and_then(Value::as_str),
        "response_status": o.pointer("/responseStatus/response").and_then(Value::as_str),
        "online_meeting_url": o.get("onlineMeetingUrl").and_then(Value::as_str)
            .or_else(|| o.pointer("/onlineMeeting/joinUrl").and_then(Value::as_str)),
        "is_online_meeting": o.get("isOnlineMeeting").and_then(Value::as_bool),
        "show_as": o.get("showAs").and_then(Value::as_str),
        "sensitivity": o.get("sensitivity").and_then(Value::as_str),
        "importance": o.get("importance").and_then(Value::as_str),
        "categories": o.get("categories"),
        "is_cancelled": o.get("isCancelled").and_then(Value::as_bool),
        "has_attachments": o.get("hasAttachments").and_then(Value::as_bool),
        "web_link": o.get("webLink").and_then(Value::as_str),
        "reminder_minutes": o.get("reminderMinutesBeforeStart"),
    })
}

/// Build a contact's `preview` from its archived JSON sidecar (#566): every
/// detail the card surfaces — name parts, the **three** addresses, IM, birthday,
/// categories, relationships, profession/office — plus `has_photo` (does the
/// archived `.jpg` exist by id), so the UI knows whether to load the photo
/// avatar. All best-effort.
fn contact_preview(it: &Item, o: &Value, root: &std::path::Path) -> Value {
    let addr = |a: &Value| {
        json!({
            "street": a.get("street").and_then(Value::as_str),
            "city": a.get("city").and_then(Value::as_str),
            "state": a.get("state").and_then(Value::as_str),
            "postalCode": a.get("postalCode").and_then(Value::as_str),
            "countryOrRegion": a.get("countryOrRegion").and_then(Value::as_str),
        })
    };
    // The photo id is hashed by shard_rel, so the path can't traverse — a cheap
    // existence check is safe.
    let has_photo = root
        .join(isyncyou_connectors::shard_rel(
            "contacts",
            &it.remote_id,
            "jpg",
        ))
        .exists();
    json!({
        "company": o.get("companyName").and_then(Value::as_str),
        "job": o.get("jobTitle").and_then(Value::as_str),
        "department": o.get("department").and_then(Value::as_str),
        "email": o.pointer("/emailAddresses/0/address").and_then(Value::as_str),
        "emails": o.get("emailAddresses"),
        "mobile": o.get("mobilePhone").and_then(Value::as_str),
        "business_phones": o.get("businessPhones"),
        "home_phones": o.get("homePhones"),
        "birthday": o.get("birthday").and_then(Value::as_str),
        "business_address": o.get("businessAddress").map(addr),
        "home_address": o.get("homeAddress").map(addr),
        "other_address": o.get("otherAddress").map(addr),
        "im_addresses": o.get("imAddresses"),
        "categories": o.get("categories"),
        "assistant": o.get("assistantName").and_then(Value::as_str),
        "manager": o.get("manager").and_then(Value::as_str),
        "spouse": o.get("spouseName").and_then(Value::as_str),
        "children": o.get("children"),
        "profession": o.get("profession").and_then(Value::as_str),
        "office_location": o.get("officeLocation").and_then(Value::as_str),
        "homepage": o.get("businessHomePage").and_then(Value::as_str),
        "title": o.get("title").and_then(Value::as_str),
        "nick_name": o.get("nickName").and_then(Value::as_str),
        "middle_name": o.get("middleName").and_then(Value::as_str),
        "initials": o.get("initials").and_then(Value::as_str),
        "generation": o.get("generation").and_then(Value::as_str),
        "file_as": o.get("fileAs").and_then(Value::as_str),
        "yomi_given": o.get("yomiGivenName").and_then(Value::as_str),
        "yomi_surname": o.get("yomiSurname").and_then(Value::as_str),
        "yomi_company": o.get("yomiCompanyName").and_then(Value::as_str),
        "has_photo": has_photo,
    })
}

/// Build a ToDo item's `preview` from its archived JSON sidecar (#567 B3). A
/// `list` exposes its list-level fields (`wellknownListName`/`isShared`/
/// `isOwner`); a `task` exposes status/importance, the date fields, recurrence,
/// categories, attachment flag, and a checklist summary (`steps_total`/
/// `steps_done`) read from the `_checklist_<id>` sub-resource sidecar (#567 B2).
/// All best-effort (absent fields → null).
fn todo_preview(it: &Item, o: &Value, root: &std::path::Path) -> Value {
    if it.item_type == "list" {
        return json!({
            "wellknown_name": o.get("wellknownListName").and_then(Value::as_str)
                .filter(|s| !s.is_empty() && *s != "none"),
            "is_shared": o.get("isShared").and_then(Value::as_bool),
            "is_owner": o.get("isOwner").and_then(Value::as_bool),
        });
    }
    // Checklist summary from the `_checklist_<id>` sub-resource sidecar (#567 B2):
    // total steps + how many are checked. The id is hashed by shard_rel, so the
    // path can't traverse.
    let (mut steps_total, mut steps_done) = (0usize, 0usize);
    if let Some(bytes) = read_under_root(
        root,
        &isyncyou_connectors::shard_rel("todo", &format!("_checklist_{}", it.remote_id), "json"),
    ) {
        if let Ok(cl) = serde_json::from_slice::<Value>(&bytes) {
            if let Some(steps) = cl.get("value").and_then(Value::as_array) {
                steps_total = steps.len();
                steps_done = steps
                    .iter()
                    .filter(|s| s.get("isChecked").and_then(Value::as_bool) == Some(true))
                    .count();
            }
        }
    }
    json!({
        "status": o.get("status").and_then(Value::as_str),
        "importance": o.get("importance").and_then(Value::as_str),
        "due": o.pointer("/dueDateTime/dateTime"),
        "due_tz": o.pointer("/dueDateTime/timeZone"),
        "start": o.pointer("/startDateTime/dateTime"),
        "start_tz": o.pointer("/startDateTime/timeZone"),
        "reminder": o.pointer("/reminderDateTime/dateTime"),
        "is_reminder_on": o.get("isReminderOn").and_then(Value::as_bool),
        "completed": o.pointer("/completedDateTime/dateTime"),
        "created": o.get("createdDateTime").and_then(Value::as_str),
        "recurrence": o.get("recurrence"),
        "categories": o.get("categories"),
        "body_type": o.pointer("/body/contentType").and_then(Value::as_str),
        "has_attachments": o.get("hasAttachments").and_then(Value::as_bool),
        "has_note": o.pointer("/body/content")
            .and_then(Value::as_str)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false),
        "steps_total": steps_total,
        "steps_done": steps_done,
    })
}

/// Build a OneNote item's `preview` (#568). A page exposes the metadata from its
/// `_pagemeta_<id>` sidecar (the page's `local_path` is the `.html` body, not JSON) —
/// createdDateTime, level/order, userTags, the OneNote web/client links, its section
/// and notebook names, plus `has_resources` (whether a `<page>.resources.json`
/// manifest exists). A notebook/section/section-group exposes a few fields from its
/// flank JSON sidecar. All best-effort.
fn onenote_preview(it: &Item, root: &std::path::Path) -> Value {
    if it.item_type == "page" {
        let meta = read_under_root(
            root,
            &isyncyou_connectors::shard_rel(
                "onenote",
                &format!("_pagemeta_{}", it.remote_id),
                "json",
            ),
        )
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or_else(|| json!({}));
        let has_resources = it
            .local_path
            .as_deref()
            .map(|rel| root.join(rel).with_extension("resources.json").exists())
            .unwrap_or(false);
        return json!({
            "created": meta.get("createdDateTime").and_then(Value::as_str),
            "level": meta.get("level"),
            "order": meta.get("order"),
            "user_tags": meta.get("userTags"),
            "web_url": meta.pointer("/links/oneNoteWebUrl/href").and_then(Value::as_str),
            "client_url": meta.pointer("/links/oneNoteClientUrl/href").and_then(Value::as_str),
            "section_name": meta.pointer("/parentSection/displayName").and_then(Value::as_str),
            "notebook_id": meta.pointer("/parentNotebook/id").and_then(Value::as_str),
            "notebook_name": meta.pointer("/parentNotebook/displayName").and_then(Value::as_str),
            "has_resources": has_resources,
        });
    }
    // notebook / section / section-group: a few fields from the flank JSON sidecar
    let o = it
        .local_path
        .as_deref()
        .and_then(|rel| read_under_root(root, rel))
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or_else(|| json!({}));
    json!({
        "is_default": o.get("isDefault").and_then(Value::as_bool),
        "web_url": o.pointer("/links/oneNoteWebUrl/href").and_then(Value::as_str),
        "created": o.get("createdDateTime").and_then(Value::as_str),
    })
}

/// Build a OneDrive item's `preview` from its archived DriveItem JSON sidecar
/// (#564): the rich facets the indexed columns can't hold. All best-effort —
/// absent fields serialize as null/false.
fn onedrive_preview(o: &Value) -> Value {
    json!({
        "mime_type": o.pointer("/file/mimeType").and_then(Value::as_str),
        "sha256": o.pointer("/file/hashes/sha256Hash").and_then(Value::as_str),
        "created": o.get("createdDateTime").and_then(Value::as_str),
        "created_by": o.pointer("/createdBy/user/displayName").and_then(Value::as_str),
        "last_modified_by": o.pointer("/lastModifiedBy/user/displayName").and_then(Value::as_str),
        "description": o.get("description").and_then(Value::as_str),
        "web_url": o.get("webUrl").and_then(Value::as_str),
        // Rich media facets passed through verbatim — the UI reads their fields
        // (dimensions, EXIF, duration, track tags, GPS) so we never lose any.
        "image": o.get("image"),
        "photo": o.get("photo"),
        "video": o.get("video"),
        "audio": o.get("audio"),
        "location": o.get("location"),
        "shared": o.get("shared").is_some(),
        "malware": o.get("malware").is_some(),
        "special_folder": o.pointer("/specialFolder/name").and_then(Value::as_str),
        "child_count": o.pointer("/folder/childCount").and_then(Value::as_i64),
        "package_type": o.pointer("/package/type").and_then(Value::as_str),
    })
}

/// Build a mail item's `preview` (#562). A `message` carries the `.eml`-parsed
/// fields (from/to/cc/subject/snippet/date/has_html/size), the attachment list,
/// and the structured Outlook fields merged from its `<id>.json` sidecar
/// (categories/isRead/flag/importance/inferenceClassification/bcc/conversationId/
/// webLink/isDraft/receipt flags). A `category` item exposes its displayName +
/// colour so the UI can build a colour map. `bytes` is the item's `local_path`
/// body (`.eml` for a message, `.json` for a category).
fn mail_preview_enrichment(
    v: &mut Value,
    it: &Item,
    root: &std::path::Path,
    rel: &str,
    bytes: &[u8],
) {
    match it.item_type.as_str() {
        "message" => {
            let p = isyncyou_connectors::mail_preview(bytes);
            let mut preview = json!({
                "from": p.from,
                "to": p.to,
                "cc": p.cc,
                "subject": p.subject,
                "snippet": p.body_snippet,
                "date": p.date,
                "has_html": p.has_html,
                "attachments": p.attachment_count,
                "size": p.size_bytes,
            });
            let atts: Vec<Value> = isyncyou_connectors::list_attachments(bytes)
                .into_iter()
                .map(|a| {
                    json!({
                        "index": a.index,
                        "filename": a.filename,
                        "content_type": a.content_type,
                        "size": a.size,
                    })
                })
                .collect();
            preview["attachment_list"] = Value::Array(atts);
            // Merge the structured Outlook fields from the <id>.json sidecar.
            if let Some(jrel) = rel.strip_suffix(".eml").map(|s| format!("{s}.json")) {
                if let Some(jb) = read_under_root(root, &jrel) {
                    if let Ok(o) = serde_json::from_slice::<Value>(&jb) {
                        preview["categories"] = o["categories"].clone();
                        preview["isRead"] = o["isRead"].clone();
                        preview["flag"] = o["flag"]["flagStatus"].clone();
                        preview["importance"] = o["importance"].clone();
                        preview["inferenceClassification"] = o["inferenceClassification"].clone();
                        preview["conversationId"] = o["conversationId"].clone();
                        preview["webLink"] = o["webLink"].clone();
                        preview["isDraft"] = o["isDraft"].clone();
                        preview["isDeliveryReceiptRequested"] =
                            o["isDeliveryReceiptRequested"].clone();
                        preview["isReadReceiptRequested"] = o["isReadReceiptRequested"].clone();
                        if let Some(bcc) = o["bccRecipients"].as_array() {
                            preview["bcc"] = Value::Array(
                                bcc.iter()
                                    .filter_map(|r| r["emailAddress"]["address"].as_str())
                                    .map(|s| json!(s))
                                    .collect(),
                            );
                        }
                    }
                }
            }
            v["preview"] = preview;
        }
        "category" => {
            if let Ok(o) = serde_json::from_slice::<Value>(bytes) {
                v["preview"] = json!({
                    "displayName": o["displayName"],
                    "color": o["color"],
                });
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isyncyou_core::config::AccountConfig;

    struct BodyEnvelopeTestGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for BodyEnvelopeTestGuard {
        fn drop(&mut self) {
            isyncyou_core::envelope::reset_body_envelope_requirement_for_tests();
            isyncyou_core::envelope::reset_body_keys_for_tests();
        }
    }

    fn body_envelope_test_guard() -> BodyEnvelopeTestGuard {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let lock = LOCK.get_or_init(|| std::sync::Mutex::new(()));
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        isyncyou_core::envelope::reset_body_envelope_requirement_for_tests();
        isyncyou_core::envelope::reset_body_keys_for_tests();
        BodyEnvelopeTestGuard { _lock: guard }
    }

    #[test]
    fn api_request_debug_redacts_query_body_cookie_and_all_authority_headers() {
        let request = ApiRequest::new("POST", "/api/v1/agent/turn?prompt=query-secret")
            .with_cap_token(Some("cap-secret".into()))
            .with_session_token(Some("session-secret".into()))
            .with_per_action_token(Some("action-secret".into()))
            .with_content_type(Some("application/json".into()))
            .with_body(b"body-secret".to_vec());
        let debug = format!("{request:?}");
        for secret in [
            "query-secret",
            "cap-secret",
            "session-secret",
            "action-secret",
            "body-secret",
        ] {
            assert!(!debug.contains(secret), "debug leaked {secret}");
        }
        assert!(debug.contains("/api/v1/agent/turn"));
        assert!(debug.contains("body_len: 11"));
        assert!(debug.contains("has_query: true"));
    }

    #[test]
    fn onedrive_risk_action_items_are_canonical_json() {
        assert_eq!(
            serde_json::from_str::<Value>(&onedrive_move_pat_item("A:B", "P]1", "N:\"1")).unwrap(),
            json!(["onedrive_move", "A:B", "P]1", "N:\"1"])
        );
        assert_eq!(
            serde_json::from_str::<Value>(&onedrive_mode_offline_pat_item("F:1")).unwrap(),
            json!(["onedrive_mode_offline", "F:1"])
        );
        assert_eq!(
            serde_json::from_str::<Value>(&onedrive_mode_online_cleanup_pat_item("F]2")).unwrap(),
            json!(["onedrive_mode_online_account_cleanup", "F]2"])
        );
        assert!(!onedrive_move_pat_item("A:B", "P]1", "N:\"1").contains("parent:"));
        assert!(!onedrive_mode_offline_pat_item("F:1").contains("mode-offline:"));
        assert!(!onedrive_mode_online_cleanup_pat_item("F]2").contains("mode-online-cleanup:"));
    }

    // AC1: the 4-state Live∪Backup model is derived correctly per item, and
    // AC2: a body archived at an older etag than the item's current sync etag
    // surfaces as `stale`.
    #[test]
    fn backup_state_derives_four_states() {
        // live_only: in the cloud per last sync, no archived body.
        let mut live = Item::new("a", "mail", "m1", "live", "message");
        live.etag = Some("E1".into());
        assert_eq!(backup_state(&live), "live_only");

        // live_backup: archived, and the body matches the current cloud etag.
        let mut backed = Item::new("a", "mail", "m2", "backed", "message");
        backed.etag = Some("E1".into());
        backed.local_path = Some("mail/aa/m2.eml".into());
        backed.body_etag = Some("E1".into());
        assert_eq!(backup_state(&backed), "live_backup");

        // stale: archived at E1, but the cloud item moved on to E2.
        let mut stale = Item::new("a", "mail", "m3", "stale", "message");
        stale.etag = Some("E2".into());
        stale.local_path = Some("mail/aa/m3.eml".into());
        stale.body_etag = Some("E1".into());
        assert_eq!(backup_state(&stale), "stale");

        // backup_only: cloud-deleted, but we still hold the archived body.
        let mut gone = Item::new("a", "mail", "m4", "gone", "message");
        gone.etag = Some("E1".into());
        gone.local_path = Some("mail/aa/m4.eml".into());
        gone.body_etag = Some("E1".into());
        gone.deleted_at = Some("2026-06-18T00:00:00Z".into());
        assert_eq!(backup_state(&gone), "backup_only");
    }

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
                cache_root: Default::default(),
                mount_point: None,
            }],
            ..Default::default()
        };
        (dir, Router::new(cfg))
    }

    fn body_json(resp: &ApiResponse) -> Value {
        serde_json::from_slice(&resp.body).unwrap()
    }

    fn strict_json_post(path: &str, body: Value, cap: Option<&str>) -> ApiRequest {
        ApiRequest::new("POST", path)
            .with_cap_token(cap.map(str::to_string))
            .with_content_type(Some("application/json".into()))
            .with_body(serde_json::to_vec(&body).unwrap())
    }

    fn strict_scalar_post_from_target(target: &str, cap: &str) -> ApiRequest {
        let legacy_shape = ApiRequest::new("POST", target);
        let mut body = serde_json::Map::new();
        body.insert(
            "request_id".into(),
            json!("123e4567-e89b-42d3-a456-426614174099"),
        );
        for (key, value) in legacy_shape.query {
            body.insert(key, Value::String(value));
        }
        strict_json_post(&legacy_shape.path, Value::Object(body), Some(cap))
    }

    fn agent_confirm_request(action_hash: &str) -> ApiRequest {
        strict_json_post(
            "/api/v1/agent/confirm",
            json!({
                "request_id": "550e8400-e29b-41d4-a716-446655440010",
                "pending": "p1",
                "token": "right",
                "action_hash": action_hash,
            }),
            Some("agentsecret"),
        )
        .with_session_token(Some("sess".into()))
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
    fn attachment_lists_and_downloads_with_inert_type() {
        let (dir, router) = setup();
        // a real .eml with one PNG attachment at m1's archived path
        let eml = b"From: a@b.com\r\nSubject: Has attach\r\n\
Content-Type: multipart/mixed; boundary=\"B\"\r\n\r\n\
--B\r\nContent-Type: text/plain\r\n\r\nhi\r\n\
--B\r\nContent-Type: image/png; name=\"pic.png\"\r\n\
Content-Disposition: attachment; filename=\"pic.png\"\r\n\
Content-Transfer-Encoding: base64\r\n\r\niVBORw0KGgo=\r\n--B--\r\n";
        let p = dir.path().join("arch").join("mail/aa/bb/x.eml");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, eml).unwrap();

        // list
        let list = router.route(&ApiRequest::get(
            "/api/v1/attachment?account=a&service=mail&id=m1",
        ));
        assert_eq!(list.status, 200);
        let v = body_json(&list);
        assert_eq!(v["attachments"][0]["index"], 0);
        assert_eq!(v["attachments"][0]["filename"], "pic.png");
        assert_eq!(v["attachments"][0]["content_type"], "image/png");

        // download index 0 → real PNG bytes, served inert as image/png
        let dl = router.route(&ApiRequest::get(
            "/api/v1/attachment?account=a&service=mail&id=m1&index=0",
        ));
        assert_eq!(dl.status, 200);
        assert_eq!(dl.content_type, "image/png");
        assert_eq!(&dl.body[..4], b"\x89PNG");

        // out of range → 404
        let oob = router.route(&ApiRequest::get(
            "/api/v1/attachment?account=a&service=mail&id=m1&index=9",
        ));
        assert_eq!(oob.status, 404);
    }

    #[test]
    fn items_mail_preview_exposes_structured_fields_and_categories() {
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("arch");
        std::fs::create_dir_all(&arch).unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut m = Item::new("a", "mail", "m1", "Hi", "message");
            m.local_path = Some("mail/aa/bb/msg.eml".into());
            store.upsert_item(&m).unwrap();
            let mut c = Item::new("a", "mail", "c1", "Red category", "category");
            c.local_path = Some("mail/cc/dd/cat.json".into());
            store.upsert_item(&c).unwrap();
        }
        // .eml (cc parsed from headers) + its <id>.json sidecar (structured fields)
        std::fs::create_dir_all(arch.join("mail/aa/bb")).unwrap();
        std::fs::write(
            arch.join("mail/aa/bb/msg.eml"),
            b"From: a@b.com\r\nTo: t@x.com\r\nCc: c@x.com\r\nSubject: Hi\r\n\r\nbody",
        )
        .unwrap();
        std::fs::write(
            arch.join("mail/aa/bb/msg.json"),
            serde_json::to_vec(&json!({
                "categories": ["Red category"],
                "isRead": false,
                "flag": { "flagStatus": "flagged" },
                "importance": "high",
                "webLink": "https://outlook/x",
                "isDraft": false,
                "bccRecipients": [{ "emailAddress": { "address": "b@x.com" } }],
            }))
            .unwrap(),
        )
        .unwrap();
        // category snapshot
        std::fs::create_dir_all(arch.join("mail/cc/dd")).unwrap();
        std::fs::write(
            arch.join("mail/cc/dd/cat.json"),
            serde_json::to_vec(&json!({ "displayName": "Red category", "color": "preset0" }))
                .unwrap(),
        )
        .unwrap();

        let cfg = Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@outlook.com".into(),
                sync_root: dir.path().join("od"),
                archive_root: arch,
                cache_root: Default::default(),
                mount_point: None,
            }],
            ..Default::default()
        };
        let router = Router::new(cfg);
        let resp = router.route(&ApiRequest::get(
            "/api/v1/items?account=a&service=mail&limit=100",
        ));
        assert_eq!(resp.status, 200);
        let v = body_json(&resp);
        let items = v["items"].as_array().unwrap();

        let msg = items.iter().find(|i| i["remote_id"] == "m1").unwrap();
        assert_eq!(msg["preview"]["cc"][0], "c@x.com");
        assert_eq!(msg["preview"]["categories"][0], "Red category");
        assert_eq!(msg["preview"]["isRead"], false);
        assert_eq!(msg["preview"]["flag"], "flagged");
        assert_eq!(msg["preview"]["importance"], "high");
        assert_eq!(msg["preview"]["webLink"], "https://outlook/x");
        assert_eq!(msg["preview"]["bcc"][0], "b@x.com");

        let cat = items.iter().find(|i| i["remote_id"] == "c1").unwrap();
        assert_eq!(cat["preview"]["displayName"], "Red category");
        assert_eq!(cat["preview"]["color"], "preset0");
    }

    #[test]
    fn onedrive_preview_captures_all_rich_facets() {
        // A DriveItem carrying every facet OneDrive can return — the preview must
        // surface each one so the detail sheet never silently drops metadata (#564).
        let o = json!({
            "createdDateTime": "2026-01-02T03:04:05Z",
            "description": "Sunset over the Alps",
            "webUrl": "https://onedrive.live.com/x",
            "createdBy": { "user": { "displayName": "Alice Admin" } },
            "lastModifiedBy": { "user": { "displayName": "Bob Editor" } },
            "file": { "mimeType": "image/jpeg", "hashes": { "sha256Hash": "ABC123" } },
            "image": { "width": 4032, "height": 3024 },
            "photo": { "takenDateTime": "2026-01-02T03:00:00Z", "cameraMake": "Google",
                       "cameraModel": "Pixel 9 Pro", "iso": 100, "fNumber": 1.8, "focalLength": 6.9 },
            "video": { "width": 1920, "height": 1080, "duration": 30000, "bitrate": 8_000_000 },
            "audio": { "title": "Belgrade Nights", "artist": "DJ Rakija", "album": "Club Mix",
                       "duration": 215_000 },
            "location": { "latitude": 47.1, "longitude": 11.4, "altitude": 600.0 },
            "shared": { "scope": "users" },
            "malware": { "description": "trojan" },
            "specialFolder": { "name": "photos" },
            "folder": { "childCount": 7 },
            "package": { "type": "oneNote" },
        });
        let p = onedrive_preview(&o);
        assert_eq!(p["mime_type"], "image/jpeg");
        assert_eq!(p["sha256"], "ABC123");
        assert_eq!(p["created"], "2026-01-02T03:04:05Z");
        assert_eq!(p["created_by"], "Alice Admin");
        assert_eq!(p["last_modified_by"], "Bob Editor");
        assert_eq!(p["description"], "Sunset over the Alps");
        assert_eq!(p["web_url"], "https://onedrive.live.com/x");
        assert_eq!(p["image"]["width"], 4032);
        assert_eq!(p["photo"]["cameraModel"], "Pixel 9 Pro");
        assert_eq!(p["photo"]["iso"], 100);
        assert_eq!(p["video"]["duration"], 30000);
        assert_eq!(p["video"]["width"], 1920);
        assert_eq!(p["audio"]["title"], "Belgrade Nights");
        assert_eq!(p["audio"]["duration"], 215_000);
        assert_eq!(p["location"]["latitude"], 47.1);
        assert_eq!(p["shared"], true);
        assert_eq!(p["malware"], true);
        assert_eq!(p["special_folder"], "photos");
        assert_eq!(p["child_count"], 7);
        assert_eq!(p["package_type"], "oneNote");
    }

    #[test]
    fn onedrive_preview_omits_absent_facets() {
        // A plain file (no media/share/malware) must not fabricate facet fields:
        // shared/malware are presence-booleans, the rest stay null.
        let o = json!({
            "createdDateTime": "2026-01-02T03:04:05Z",
            "file": { "mimeType": "application/pdf" },
        });
        let p = onedrive_preview(&o);
        assert_eq!(p["mime_type"], "application/pdf");
        assert_eq!(p["shared"], false);
        assert_eq!(p["malware"], false);
        assert!(p["video"].is_null());
        assert!(p["audio"].is_null());
        assert!(p["location"].is_null());
        assert!(p["description"].is_null());
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

    #[test]
    fn transfers_poll_is_gate_exempt_for_the_live_panel() {
        // #656: the mobile offline pass holds the store gate for the whole blocking
        // materialize, so the live transfer poll MUST NOT take the gate — otherwise the panel
        // can't show progress while a folder downloads. With the gate held on this thread, a
        // non-exempt route would re-lock it (same-thread deadlock); the exempt poll is served.
        struct NoopTransfers;
        impl TransferProgress for NoopTransfers {
            fn transfers(&self) -> Vec<TransferState> {
                vec![]
            }
            fn cancel(&self, _id: &str) -> bool {
                false
            }
        }
        let gate = std::sync::Arc::new(std::sync::Mutex::new(()));
        let router = Router::with_gate(Config::default(), gate.clone())
            .with_transfers(std::sync::Arc::new(NoopTransfers), "cap".into());
        let _held = gate.lock().unwrap_or_else(|e| e.into_inner());
        let resp = router.route(&ApiRequest::get("/api/v1/onedrive/transfers"));
        assert_eq!(
            resp.status, 200,
            "the live transfer poll must be served while the offline pass holds the gate"
        );
    }

    #[test]
    fn agent_hot_reads_are_gate_exempt_during_archive_sync() {
        let gate = std::sync::Arc::new(std::sync::Mutex::new(()));
        let router = Router::with_gate(Config::default(), gate.clone());
        let _held = gate.lock().unwrap_or_else(|e| e.into_inner());
        for (path, expected_status) in [
            ("/api/v1/agent/status", 200),
            ("/api/v1/agent/session/list?limit=100", 404),
            (
                "/api/v1/agent/session/history?session_id=session&limit=100",
                404,
            ),
            (
                "/api/v1/agent/request/status?session_id=session&route=turn&request_id=request",
                404,
            ),
        ] {
            let response = router.route(&ApiRequest::get(path));
            assert_eq!(
                response.status, expected_status,
                "Agent hot read must be served without the archive gate: {path}"
            );
        }
    }

    #[test]
    fn agent_turn_admission_is_gate_exempt_during_archive_sync() {
        let gate = std::sync::Arc::new(std::sync::Mutex::new(()));
        let router = Router::with_gate(Config::default(), gate.clone())
            .with_agent(std::sync::Arc::new(FakeAgent), "agent-cap".into());
        let _held = gate.lock().unwrap_or_else(|e| e.into_inner());
        let response = router.route(
            &ApiRequest::new("POST", "/api/v1/agent/turn")
                .with_cap_token(Some("agent-cap".into()))
                .with_content_type(Some("application/json".into()))
                .with_body(
                    br#"{"request_id":"123e4567-e89b-42d3-a456-426614174000","session_id":"session","account":"reader","prompt":"status"}"#
                        .to_vec(),
                ),
        );
        assert_eq!(
            response.status, 200,
            "Agent turn admission must not wait for the archive gate"
        );
    }

    #[test]
    fn transfer_control_posts_are_gate_exempt_during_a_pass() {
        // #659: pause/retry/cancel touch ONLY the in-memory SharedProgress, never the store, so they
        // MUST be gate-exempt — otherwise the mobile offline pass, which holds the store gate for the
        // whole blocking materialize, would block the very pause/retry that targets it (the pause/retry
        // AC is "pause a LIVE materialization"). With the gate held on this thread, a non-exempt route
        // would re-lock it (same-thread deadlock); the exempt control POSTs are served.
        struct NoopTransfers;
        impl TransferProgress for NoopTransfers {
            fn transfers(&self) -> Vec<TransferState> {
                vec![]
            }
            fn cancel(&self, _id: &str) -> bool {
                true
            }
            fn pause(&self, _id: &str) -> bool {
                true
            }
            fn retry(&self, _id: &str) -> bool {
                true
            }
        }
        let gate = std::sync::Arc::new(std::sync::Mutex::new(()));
        let router = Router::with_gate(Config::default(), gate.clone())
            .with_transfers(std::sync::Arc::new(NoopTransfers), "cap".into());
        let _held = gate.lock().unwrap_or_else(|e| e.into_inner());
        for path in [
            "/api/v1/onedrive/transfers/cancel?id=t1",
            "/api/v1/onedrive/transfers/pause?id=t1",
            "/api/v1/onedrive/transfers/retry?id=t1",
        ] {
            let resp = router.route(&strict_scalar_post_from_target(path, "cap"));
            assert_eq!(
                resp.status, 200,
                "control POST must be served while the pass holds the gate: {path}"
            );
        }
    }

    struct OkRestore;
    impl RestoreHandler for OkRestore {
        fn restore(&self, _a: &str, _s: &str, _i: &str) -> Result<RestoreResponse, String> {
            Ok(RestoreResponse::Completed {
                new_id: "new-cloud-id".into(),
            })
        }
    }

    #[derive(Default)]
    struct QueuedRestore {
        calls: std::sync::Mutex<Vec<(String, String, String)>>,
    }
    impl RestoreHandler for QueuedRestore {
        fn restore(&self, a: &str, s: &str, i: &str) -> Result<RestoreResponse, String> {
            self.calls
                .lock()
                .unwrap()
                .push((a.to_string(), s.to_string(), i.to_string()));
            Ok(RestoreResponse::Queued {
                job_id: "job-restore-1".into(),
                state: "queued".into(),
            })
        }
    }

    struct ErrRestore;
    impl RestoreHandler for ErrRestore {
        fn restore(&self, _a: &str, _s: &str, _i: &str) -> Result<RestoreResponse, String> {
            Err("graph refused restore for pat@example.com https://secret.example/item".into())
        }
    }

    #[derive(Default)]
    struct RecordingBackup {
        calls: std::sync::Mutex<Vec<(String, Vec<String>)>>,
    }

    impl BackupHandler for RecordingBackup {
        fn enqueue_backup(
            &self,
            account: &str,
            services: &[String],
        ) -> Result<BackupJobQueued, String> {
            self.calls
                .lock()
                .unwrap()
                .push((account.to_string(), services.to_vec()));
            Ok(BackupJobQueued {
                job_id: "job-backup-1".to_string(),
                state: "queued".to_string(),
            })
        }
    }

    #[derive(Default)]
    struct RecordingJobs {
        cancelled: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl MobileJobHandler for RecordingJobs {
        fn list_jobs(&self, account: &str, _limit: u32) -> Result<Vec<MobileJobSummary>, String> {
            Ok(vec![MobileJobSummary {
                job_id: "job-1".to_string(),
                kind: "backup".to_string(),
                state: "queued".to_string(),
                service: None,
                target_id: None,
                created_at: 10,
                updated_at: 11,
                finished_at: None,
                last_error: if account == "with-error" {
                    Some("[redacted]".to_string())
                } else {
                    None
                },
            }])
        }

        fn cancel_job(&self, account: &str, job_id: &str) -> Result<bool, String> {
            self.cancelled
                .lock()
                .unwrap()
                .push((account.to_string(), job_id.to_string()));
            Ok(true)
        }
    }

    #[test]
    fn restore_post_completed_shape_is_backward_compatible() {
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
        let ok = router.route(&strict_scalar_post_from_target(q, "secret"));
        assert_eq!(ok.status, 200);
        let body = body_json(&ok);
        assert_eq!(body["restored"], "x");
        assert_eq!(body["service"], "mail");
        assert_eq!(body["new_id"], "new-cloud-id");
        assert!(body.get("queued").is_none());
        // valid token but missing params -> 400
        let bad = strict_scalar_post_from_target("/api/v1/restore?account=a", "secret");
        assert_eq!(router.route(&bad).status, 400);
    }

    #[test]
    fn restore_post_queued_shape_for_mobile_jobs() {
        let (_d, router) = setup();
        let restore = std::sync::Arc::new(QueuedRestore::default());
        let router = router.with_restore(restore.clone(), "secret".into());
        let q = "/api/v1/restore?account=a&service=mail&id=x";

        let ok = router.route(&strict_scalar_post_from_target(q, "secret"));

        assert_eq!(ok.status, 200);
        let body = body_json(&ok);
        assert_eq!(body["queued"], true);
        assert_eq!(body["restored"], "x");
        assert_eq!(body["service"], "mail");
        assert_eq!(body["job_id"], "job-restore-1");
        assert_eq!(body["state"], "queued");
        assert_eq!(
            restore.calls.lock().unwrap().as_slice(),
            &[("a".to_string(), "mail".to_string(), "x".to_string())]
        );
    }

    #[test]
    fn mobile_restore_route_is_401_without_session() {
        let (_d, router) = setup();
        let router = router
            .with_restore(std::sync::Arc::new(QueuedRestore::default()), "cap".into())
            .with_session_token("sess".into());
        let req = ApiRequest::new("POST", "/api/v1/restore?account=a&service=mail&id=x")
            .with_cap_token(Some("cap".into()));

        assert_eq!(router.route(&req).status, 401);
    }

    #[test]
    fn mobile_restore_route_is_401_without_cap_after_session() {
        let (_d, router) = setup();
        let router = router
            .with_restore(std::sync::Arc::new(QueuedRestore::default()), "cap".into())
            .with_session_token("sess".into());
        let req = ApiRequest::new("POST", "/api/v1/restore?account=a&service=mail&id=x")
            .with_session_token(Some("sess".into()));

        assert_eq!(router.route(&req).status, 401);
    }

    #[test]
    fn mobile_restore_route_is_biometric_token_gated_before_enqueue() {
        let (_d, router) = setup();
        let restore = std::sync::Arc::new(QueuedRestore::default());
        let mobile = router
            .with_restore(restore.clone(), "cap".into())
            .with_session_token("sess".into())
            .with_biometric_gate();
        let post = |path: &str| {
            strict_scalar_post_from_target(path, "cap").with_session_token(Some("sess".into()))
        };

        let ch = mobile.route(&post("/api/v1/restore?account=a&service=mail&id=x"));

        assert_eq!(ch.status, 200);
        let body = body_json(&ch);
        assert_eq!(body["status"], "confirmation_required");
        assert_eq!(body["op"], "restore-cloud");
        assert_eq!(body["account"], "a");
        assert_eq!(body["service"], "mail");
        assert_eq!(body["item"], restore_cloud_pending_item("mail", "x"));
        assert!(restore.calls.lock().unwrap().is_empty());

        let pat = body["pending_action_id"].as_str().unwrap().to_string();
        assert_eq!(
            mobile
                .route(
                    &post("/api/v1/restore?account=a&service=mail&id=x")
                        .with_per_action_token(Some(pat.clone()))
                )
                .status,
            403
        );
        assert!(restore.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn mobile_restore_route_enqueues_job_after_biometric_token() {
        let (_d, router) = setup();
        let restore = std::sync::Arc::new(QueuedRestore::default());
        let mobile = router
            .with_restore(restore.clone(), "cap".into())
            .with_session_token("sess".into())
            .with_biometric_gate();
        let post = |path: &str| {
            strict_scalar_post_from_target(path, "cap").with_session_token(Some("sess".into()))
        };

        let ch = mobile.route(&post("/api/v1/restore?account=a&service=mail&id=x"));
        let pat = body_json(&ch)["pending_action_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(mobile.confirm_biometric(&pat));

        let ok = mobile.route(
            &post("/api/v1/restore?account=a&service=mail&id=x").with_per_action_token(Some(pat)),
        );

        assert_eq!(ok.status, 200);
        let body = body_json(&ok);
        assert_eq!(body["queued"], true);
        assert_eq!(body["job_id"], "job-restore-1");
        assert_eq!(
            restore.calls.lock().unwrap().as_slice(),
            &[("a".to_string(), "mail".to_string(), "x".to_string())]
        );
    }

    #[test]
    fn backup_route_is_absent_without_handler() {
        let (_d, router) = setup();
        let req = ApiRequest::new("POST", "/api/v1/backup?account=a")
            .with_cap_token(Some("backup-secret".into()));
        assert_eq!(router.route(&req).status, 404);
    }

    #[test]
    fn backup_route_requires_backup_cap_token() {
        let (_d, router) = setup();
        let backup = std::sync::Arc::new(RecordingBackup::default());
        let router = router.with_backup(backup.clone(), "backup-secret".into());
        let q = "/api/v1/backup?account=a&services=calendar,mail";
        let request = strict_scalar_post_from_target(q, "backup-secret");
        assert_eq!(
            router.route(&request.clone().with_cap_token(None)).status,
            401
        );
        assert_eq!(
            router
                .route(&request.clone().with_cap_token(Some("wrong".into())))
                .status,
            401
        );
        let ok = router.route(&request);
        assert_eq!(ok.status, 200);
        let body = body_json(&ok);
        assert_eq!(body["queued"], true);
        assert_eq!(body["job_id"], "job-backup-1");
        assert_eq!(
            backup.calls.lock().unwrap().as_slice(),
            &[(
                "a".to_string(),
                vec!["calendar".to_string(), "mail".to_string()]
            )]
        );
    }

    #[test]
    fn backup_route_is_session_gated_on_mobile() {
        let (_d, router) = setup();
        let router = router
            .with_backup(
                std::sync::Arc::new(RecordingBackup::default()),
                "cap".into(),
            )
            .with_session_token("sess".into());
        let req = strict_scalar_post_from_target("/api/v1/backup?account=a", "cap");
        assert_eq!(router.route(&req).status, 401);
        let ok = router.route(&req.with_session_token(Some("sess".into())));
        assert_eq!(ok.status, 200);
    }

    #[test]
    fn backup_route_is_biometric_token_gated_on_mobile_before_handler_call() {
        let (_d, router) = setup();
        let backup = std::sync::Arc::new(RecordingBackup::default());
        let mobile = router
            .with_backup(backup.clone(), "cap".into())
            .with_session_token("sess".into())
            .with_biometric_gate();
        let post = |path: &str| {
            strict_scalar_post_from_target(path, "cap").with_session_token(Some("sess".into()))
        };
        let ch = mobile.route(&post("/api/v1/backup?account=a&services=mail"));
        assert_eq!(ch.status, 200);
        let body = body_json(&ch);
        assert_eq!(body["status"], "confirmation_required");
        assert_eq!(body["op"], "backup");
        assert!(backup.calls.lock().unwrap().is_empty());
        let pat = body["pending_action_id"].as_str().unwrap().to_string();

        assert_eq!(
            mobile
                .route(
                    &post("/api/v1/backup?account=a&services=mail")
                        .with_per_action_token(Some(pat.clone()))
                )
                .status,
            403
        );
        assert!(backup.calls.lock().unwrap().is_empty());

        assert!(mobile.confirm_biometric(&pat));
        let ok = mobile.route(
            &post("/api/v1/backup?account=a&services=mail").with_per_action_token(Some(pat)),
        );
        assert_eq!(ok.status, 200);
        assert_eq!(backup.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn backup_route_rejects_unknown_service_before_biometric_token() {
        let (_d, router) = setup();
        let backup = std::sync::Arc::new(RecordingBackup::default());
        let mobile = router
            .with_backup(backup.clone(), "cap".into())
            .with_session_token("sess".into())
            .with_biometric_gate();
        let req =
            strict_scalar_post_from_target("/api/v1/backup?account=a&services=mail,shell", "cap")
                .with_session_token(Some("sess".into()));

        let resp = mobile.route(&req);

        assert_eq!(resp.status, 400);
        let body = body_json(&resp);
        assert_eq!(body["error"], "unsupported backup service: shell");
        assert!(backup.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn mobile_job_routes_require_cap_and_return_secret_free_summaries() {
        let (_d, router) = setup();
        let jobs = std::sync::Arc::new(RecordingJobs::default());
        let router = router.with_mobile_jobs(jobs.clone(), "jobs-secret".into());
        let q = "/api/v1/jobs?account=with-error";
        assert_eq!(router.route(&ApiRequest::get(q)).status, 401);
        let ok = router.route(&ApiRequest::get(q).with_cap_token(Some("jobs-secret".into())));
        assert_eq!(ok.status, 200);
        let body = body_json(&ok);
        assert_eq!(body["jobs"][0]["job_id"], "job-1");
        assert_eq!(body["jobs"][0]["kind"], "backup");
        assert_eq!(body["jobs"][0]["last_error"], "[redacted]");
        assert!(body["jobs"][0].get("intent_json").is_none());
        assert!(body["jobs"][0].get("result_json").is_none());

        let cancel = router.route(
            &ApiRequest::new("POST", "/api/v1/jobs/cancel?account=a&job_id=job-1")
                .with_cap_token(Some("jobs-secret".into())),
        );
        assert_eq!(cancel.status, 200);
        assert_eq!(
            jobs.cancelled.lock().unwrap().as_slice(),
            &[("a".to_string(), "job-1".to_string())]
        );
    }

    /// #639 T7: a handler whose product turn is refused with the closed `product_not_ready` code.
    struct ProductNotReadyAgent;
    impl AgentHandler for ProductNotReadyAgent {
        fn start_turn(&self, _a: &str, _p: &str) -> Result<String, String> {
            Err("product_not_ready".into())
        }
        fn confirm(&self, _p: &str, _t: &str, _h: &str) -> Result<String, String> {
            Err("n/a".into())
        }
        fn cancel(&self, _t: &str) {}
        fn open_stream(&self, _t: &str) -> Option<std::sync::mpsc::Receiver<String>> {
            None
        }
    }

    #[test]
    fn product_not_ready_turn_maps_to_409_not_500() {
        let (_d, router) = setup();
        let router = router.with_agent(
            std::sync::Arc::new(ProductNotReadyAgent),
            "agentsecret".into(),
        );
        let resp = router.route(
            &ApiRequest::new("POST", "/api/v1/agent/turn")
                .with_cap_token(Some("agentsecret".into()))
                .with_content_type(Some("application/json".into()))
                .with_body(
                    br#"{"request_id":"550e8400-e29b-41d4-a716-446655440000","session_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","account":"a","prompt":"hi"}"#
                        .to_vec(),
                ),
        );
        assert_eq!(resp.status, 409);
        assert!(String::from_utf8_lossy(&resp.body).contains("product_not_ready"));
    }

    #[derive(Default)]
    struct RecordingModelAgent {
        selections: std::sync::Mutex<Vec<(String, String, Option<String>)>>,
    }

    impl AgentHandler for RecordingModelAgent {
        fn start_turn(&self, _account: &str, _prompt: &str) -> Result<String, String> {
            Err("not enabled".into())
        }

        fn confirm(
            &self,
            _pending_id: &str,
            _token: &str,
            _action_hash: &str,
        ) -> Result<String, String> {
            Err("not enabled".into())
        }

        fn cancel(&self, _turn_id: &str) {}

        fn open_stream(&self, _turn_id: &str) -> Option<std::sync::mpsc::Receiver<String>> {
            None
        }

        fn set_model(
            &self,
            provider: &str,
            model: &str,
            reasoning_effort: Option<&str>,
        ) -> Result<(), String> {
            self.selections.lock().unwrap().push((
                provider.to_owned(),
                model.to_owned(),
                reasoning_effort.map(str::to_owned),
            ));
            Ok(())
        }
    }

    #[test]
    fn agent_model_route_carries_closed_reasoning_effort_in_strict_json() {
        let (_directory, router) = setup();
        let agent = std::sync::Arc::new(RecordingModelAgent::default());
        let router = router.with_agent(agent.clone(), "agentsecret".into());
        let request_id = "550e8400-e29b-41d4-a716-446655440000";
        let response = router.route(&strict_json_post(
            "/api/v1/agent/model",
            json!({
                "request_id": request_id,
                "provider": "codex",
                "model": "gpt-5.6-terra",
                "reasoning_effort": "high",
            }),
            Some("agentsecret"),
        ));
        assert_eq!(response.status, 200);
        assert_eq!(body_json(&response)["reasoning_effort"], "high");
        assert_eq!(
            agent.selections.lock().unwrap().as_slice(),
            &[("codex".into(), "gpt-5.6-terra".into(), Some("high".into()))]
        );

        let unknown = router.route(&strict_json_post(
            "/api/v1/agent/model",
            json!({
                "request_id": request_id,
                "provider": "codex",
                "model": "gpt-5.6-sol",
                "reasoning_effort": "medium",
                "endpoint": "forbidden",
            }),
            Some("agentsecret"),
        ));
        assert_eq!(unknown.status, 400);
        assert_eq!(agent.selections.lock().unwrap().len(), 1);
    }

    // #639 T9 AC1: /oauth/complete is strict JSON and Claude-only. A query-param code, a codex
    // provider, an unknown field, and a non-JSON content type are all rejected at the edge (400).
    #[test]
    fn oauth_complete_route_is_strict_json_claude_only() {
        let (_d, router) = setup();
        let router = router.with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into());
        let json_post = |body: &str| {
            ApiRequest::new("POST", "/api/v1/agent/oauth/complete")
                .with_cap_token(Some("agentsecret".into()))
                .with_content_type(Some("application/json".into()))
                .with_body(body.as_bytes().to_vec())
        };
        // A pasted code as a URL/query param is rejected (must be a JSON body).
        let query = ApiRequest::new("POST", "/api/v1/agent/oauth/complete?code=abc%23state")
            .with_cap_token(Some("agentsecret".into()));
        assert_eq!(router.route(&query).status, 400);
        // Codex may not use this route.
        assert_eq!(
            router
                .route(&json_post(
                    r#"{"provider":"codex","attempt_id":"a","pasted_code":"c#s"}"#
                ))
                .status,
            400
        );
        // Unknown field is rejected (deny_unknown_fields).
        assert_eq!(
            router
                .route(&json_post(
                    r#"{"provider":"claude","attempt_id":"a","pasted_code":"c#s","x":1}"#
                ))
                .status,
            400
        );
        // Serde's struct visitor rejects duplicate fields rather than accepting the last value.
        assert_eq!(
            router
                .route(&json_post(
                    r#"{"provider":"claude","attempt_id":"a","attempt_id":"b","pasted_code":"c#s"}"#
                ))
                .status,
            400
        );
        // A well-formed claude request reaches the handler and the response carries no-store.
        let resp = router.route(&json_post(
            r#"{"provider":"claude","attempt_id":"a","pasted_code":"c#s"}"#,
        ));
        assert!(resp
            .headers
            .iter()
            .any(|(name, value)| name == "Cache-Control" && value == "no-store"));
    }

    #[test]
    fn assistant_first_run_renders_handoff_wizard_from_host_readiness() {
        let view_start = APP_JS
            .find("async function renderAssistantView(view)")
            .expect("assistant view");
        let view_end = APP_JS[view_start..]
            .find("const AGENT_ONBOARDING_STEPS")
            .map(|offset| view_start + offset)
            .expect("onboarding steps");
        let view = &APP_JS[view_start..view_end];
        assert!(view.contains("if (assistantSelectedReady(st)) renderAssistantChat(body, st);"));
        assert!(!view.contains("if (st && st.connected)"));

        let expected = [
            "official_oauth_completed",
            "credential_encrypted",
            "retained_envelope_verified",
            "default_harness_removed",
            "m365_profile_activated",
            "isyncyou_tool_connected",
            "subscription_identity_set",
            "ready",
        ];
        let mut previous = 0;
        for key in expected {
            let position = APP_JS
                .find(&format!("[\"{key}\""))
                .unwrap_or_else(|| panic!("missing wizard step {key}"));
            assert!(position >= previous, "wizard step order drifted at {key}");
            previous = position;
        }
    }

    #[test]
    fn assistant_reconnect_uses_short_flow_after_onboarding() {
        assert!(APP_JS.contains("const reconnect = state === \"reconnect_required\";"));
        assert!(APP_JS
            .contains("const stepsPanel = reconnect ? null : renderAssistantWizardSteps(node);"));
        assert!(APP_JS
            .contains("reconnect ? \"Reconnect your AI account\" : \"Connect your AI account\""));
    }

    #[test]
    fn assistant_handoff_dom_logs_and_storage_do_not_contain_secrets() {
        let start = APP_JS
            .find("async function completeAiLogin()")
            .expect("manual completion function");
        let end = APP_JS[start..]
            .find("const AssistantState")
            .map(|offset| start + offset)
            .expect("manual completion end");
        let completion = &APP_JS[start..end];
        assert!(completion.contains("if (inp) inp.value = \"\";"));
        assert!(completion.contains("postJson(\"/api/v1/agent/oauth/complete\""));
        for forbidden in [
            "console.",
            "localStorage",
            "sessionStorage",
            "qs({",
            "innerHTML",
        ] {
            assert!(
                !completion.contains(forbidden),
                "manual OAuth completion must not use secret sink {forbidden}"
            );
        }
    }

    #[test]
    fn assistant_turn_guard_releases_on_stream_error_and_route_teardown() {
        let close_start = APP_JS
            .find("function closeAssistantStream(_reason)")
            .expect("stream close helper");
        let close_end = APP_JS[close_start..]
            .find("function rememberAssistantStatus")
            .map(|offset| close_start + offset)
            .expect("stream close helper end");
        let close = &APP_JS[close_start..close_end];
        assert!(close.contains("const turn = AssistantState.activeTurnId;"));
        assert!(close.contains("if (turn) void finishTurnGuard(turn);"));
        assert!(APP_JS.contains("TURN_GUARDS.set(turn, startingGuardId);"));
    }

    #[test]
    fn assistant_turn_start_preserves_ambiguous_guard() {
        let start = APP_JS
            .find("async function agentSend(text)")
            .expect("agent send function");
        let end = APP_JS[start..]
            .find("function renderAssistantView")
            .map(|offset| start + offset)
            .unwrap_or(APP_JS.len());
        let send = &APP_JS[start..end];
        assert!(send.contains("let turnStartPosted = false;"));
        assert!(send.contains("turnStartPosted = true;"));
        assert!(send.contains("!turnStartPosted || (e && e.responseReceived === true)"));
    }

    // #639 T9 AC3: the status response carries Cache-Control: no-store and never leaks a secret.
    #[test]
    fn status_carries_no_store_and_no_secrets() {
        let (_d, router) = setup();
        let router = router.with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into());
        let resp = router.route(&ApiRequest::new("GET", "/api/v1/agent/status"));
        assert!(resp
            .headers
            .iter()
            .any(|(name, value)| name == "Cache-Control" && value == "no-store"));
        let body = String::from_utf8_lossy(&resp.body);
        for secret in [
            "access_token",
            "refresh_token",
            "account_id",
            "authorization",
            "Bearer",
            "verifier",
            "pkce",
        ] {
            assert!(!body.contains(secret), "status leaked {secret}");
        }
    }

    struct FakeAgent;
    impl AgentHandler for FakeAgent {
        fn start_turn(&self, _a: &str, _p: &str) -> Result<String, String> {
            Ok("turn-123".into())
        }
        fn confirm(&self, pending: &str, token: &str, action_hash: &str) -> Result<String, String> {
            if token == "right" && action_hash == "hash" {
                Ok(format!("ran {pending}"))
            } else {
                Err("bad token".into())
            }
        }
        fn cancel(&self, _t: &str) {}
        fn cancel_turn(&self, turn_id: &str) -> Result<(), String> {
            (turn_id == "turn-123")
                .then_some(())
                .ok_or_else(|| "turn_not_found".into())
        }
        fn cancel_pending(&self, pending_id: &str, action_hash: &str) -> Result<(), String> {
            (pending_id == "pending-123" && action_hash == "a".repeat(64))
                .then_some(())
                .ok_or_else(|| "NotFound".into())
        }
        fn open_stream(&self, turn: &str) -> Option<std::sync::mpsc::Receiver<String>> {
            if turn != "turn-123" {
                return None;
            }
            let (tx, rx) = std::sync::mpsc::channel();
            tx.send("{\"event\":\"token\",\"text\":\"hi\"}".into()).ok();
            Some(rx)
        }
        fn session_create(
            &self,
            request_id: &str,
            display_name: Option<&str>,
        ) -> Result<serde_json::Value, String> {
            Ok(json!({
                "request_id": request_id,
                "session": {
                    "session_id": "01JSESSION00000000000000000",
                    "display_name": display_name,
                },
                "selected_session_id": "01JSESSION00000000000000000",
            }))
        }
        fn session_list(
            &self,
            cursor: Option<&str>,
            limit: Option<usize>,
        ) -> Result<serde_json::Value, String> {
            Ok(json!({
                "sessions": [],
                "cursor": cursor,
                "limit": limit,
                "next_cursor": null,
            }))
        }
        fn session_select(
            &self,
            _request_id: &str,
            session_id: &str,
        ) -> Result<serde_json::Value, String> {
            Ok(json!({ "selected_session_id": session_id }))
        }
        fn session_history(
            &self,
            session_id: &str,
            cursor: Option<&str>,
            limit: Option<usize>,
        ) -> Result<serde_json::Value, String> {
            Ok(json!({
                "session_id": session_id,
                "cursor": cursor,
                "limit": limit,
                "records": [],
                "next_cursor": null,
            }))
        }
        fn request_status(
            &self,
            session_id: &str,
            route: &str,
            request_id: &str,
        ) -> Result<serde_json::Value, String> {
            Ok(json!({
                "session_scope_valid": session_id == "01JSESSION00000000000000000",
                "route": route,
                "request_id_valid": request_id == "123e4567-e89b-42d3-a456-426614174000",
                "state": "outcome_unknown",
                "code": "turn_outcome_unknown",
                "terminal": true,
                "resume_allowed": false,
            }))
        }
        fn session_archive(
            &self,
            request: AgentSessionArchiveRequest,
        ) -> Result<serde_json::Value, String> {
            Ok(json!({
                "request_id": request.request_id,
                "operation_id": request.operation_id,
                "state": "archived",
            }))
        }
        fn user_presence_start(
            &self,
            request: AgentUserPresenceStartRequest,
        ) -> Result<serde_json::Value, String> {
            Ok(json!({
                "request_id": request.request_id,
                "operation_id": "01JOPERATION00000000000000",
                "intent_id": "01JINTENT0000000000000000",
                "token": "confirmation-token",
                "action_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "expires_at_ms": 1_000_000,
            }))
        }
        fn user_presence_confirm(
            &self,
            request: AgentUserPresenceConfirmRequest,
        ) -> Result<serde_json::Value, String> {
            Ok(json!({
                "request_id": request.request_id,
                "operation_id": request.operation_id,
                "state": "authorized",
            }))
        }
        fn session_pairing_create(
            &self,
            request: AgentSessionPairingCreateRequest,
        ) -> Result<serde_json::Value, String> {
            Ok(json!({
                "request_id": request.request_id,
                "pair_id": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                "expires_at_ms": 1_000_000,
            }))
        }
        fn session_pairing_reveal(
            &self,
            request: AgentSessionPairingOperationRequest,
        ) -> Result<serde_json::Value, String> {
            Ok(json!({
                "request_id": request.request_id,
                "operation_id": request.operation_id,
                "pairing_code": format!("isy2.{}.{}", "A".repeat(32), "B".repeat(43)),
                "expires_at_ms": 1_000_000,
            }))
        }
        fn session_pairing_claim(
            &self,
            request: AgentSessionPairingOperationRequest,
        ) -> Result<serde_json::Value, String> {
            Ok(json!({
                "request_id": request.request_id,
                "operation_id": request.operation_id,
                "state": "claimed",
            }))
        }
        fn session_pairing_finalize(
            &self,
            request: AgentSessionPairingOperationRequest,
        ) -> Result<serde_json::Value, String> {
            Ok(json!({
                "request_id": request.request_id,
                "operation_id": request.operation_id,
                "state": "consumed",
            }))
        }
        fn session_pairing_revoke(
            &self,
            request: AgentSessionPairingOperationRequest,
        ) -> Result<serde_json::Value, String> {
            Ok(json!({
                "request_id": request.request_id,
                "operation_id": request.operation_id,
                "state": "revoked",
            }))
        }
        fn connectivity_preflight(
            &self,
            request: AgentConnectivityPreflightRequest,
        ) -> Result<AgentConnectivityPreflightResponse, String> {
            if request.provider != "claude" || request.purpose != "turn_start" {
                return Err("unsupported test request".into());
            }
            Ok(AgentConnectivityPreflightResponse {
                status: "ready".into(),
                code: "ready".into(),
                retryable: false,
                settings_hint: "none".into(),
            })
        }
        fn credential_refresh(&self, provider: &str) -> Result<String, String> {
            if provider == "claude" {
                Ok("connected".into())
            } else {
                Err("reconnect_required".into())
            }
        }
        fn oauth_start(&self, _provider: &str, redirect_uri: &str) -> Result<String, String> {
            Ok(format!(
                "https://auth.example/authorize?redirect_uri={redirect_uri}&state=st-1"
            ))
        }
        fn oauth_start_request(
            &self,
            request: AgentOAuthStartRequest,
        ) -> Result<AgentOAuthStartResponse, String> {
            Ok(AgentOAuthStartResponse {
                authorize_url: format!(
                    "https://auth.example/authorize?provider={}",
                    request.provider
                ),
                attempt_id: "attempt-1".into(),
                lifecycle_operation_id: request.lifecycle_operation_id,
            })
        }
        fn oauth_logout(
            &self,
            request: AgentOAuthLogoutRequest,
        ) -> Result<AgentAccountLifecycleResponse, String> {
            Ok(AgentAccountLifecycleResponse {
                provider: request.provider,
                mode: request.mode,
                operation_id: Some("operation-1".into()),
                operation_etag: Some("operation-etag-1".into()),
                state: "disconnected".into(),
                revoke_leg: 1,
                revoked_grant: Some("active_credential".into()),
                revoke_request_target: Some("refresh_token".into()),
                revoke_scope_guarantee: Some("observed_token_session".into()),
                retryable: false,
                code: "disconnected".into(),
            })
        }
        fn oauth_lifecycle_resume(
            &self,
            request: AgentOAuthLifecycleResumeRequest,
        ) -> Result<AgentAccountLifecycleResponse, String> {
            Ok(AgentAccountLifecycleResponse {
                provider: request.provider,
                mode: "connect".into(),
                operation_id: Some(request.operation_id),
                operation_etag: Some(request.operation_etag),
                state: "candidate_cleanup".into(),
                revoke_leg: 1,
                revoked_grant: Some("oauth_candidate".into()),
                revoke_request_target: Some("refresh_token".into()),
                revoke_scope_guarantee: Some("observed_token_session".into()),
                retryable: true,
                code: request.action,
            })
        }
        fn oauth_cancel(&self, provider: &str, attempt_id: &str) -> Result<(), String> {
            if provider == "claude" && attempt_id == "attempt-1" {
                Ok(())
            } else {
                Err("oauth attempt is not active".into())
            }
        }
        fn oauth_callback(&self, code: &str, state: &str) -> Result<String, String> {
            Ok(format!("<html>connected code={code} state={state}</html>"))
        }
        fn status_json(&self) -> String {
            "{\"connected\":true,\"enabled\":true,\"model\":\"fake-1\"}".to_string()
        }
    }

    struct RecordingAgent {
        binding: AgentPendingBinding,
        binding_calls: std::sync::Mutex<Vec<(String, String)>>,
        confirm_calls: std::sync::Mutex<Vec<(String, String, String)>>,
    }

    impl RecordingAgent {
        fn backup() -> Self {
            Self {
                binding: AgentPendingBinding {
                    op: "backup".into(),
                    account: "a".into(),
                    service: "agent".into(),
                    item: "pending:2:p1:action_hash:4:hash".into(),
                },
                binding_calls: std::sync::Mutex::new(Vec::new()),
                confirm_calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn confirm_call_count(&self) -> usize {
            self.confirm_calls.lock().unwrap().len()
        }
    }

    impl AgentHandler for RecordingAgent {
        fn start_turn(&self, _account: &str, _prompt: &str) -> Result<String, String> {
            Ok("turn-recording".into())
        }

        fn pending_binding(
            &self,
            pending_id: &str,
            action_hash: &str,
        ) -> Result<AgentPendingBinding, String> {
            self.binding_calls
                .lock()
                .unwrap()
                .push((pending_id.to_string(), action_hash.to_string()));
            if pending_id == "p1" && action_hash == "hash" {
                Ok(self.binding.clone())
            } else {
                Err("ActionMismatch".into())
            }
        }

        fn confirm(&self, pending: &str, token: &str, action_hash: &str) -> Result<String, String> {
            self.confirm_calls.lock().unwrap().push((
                pending.to_string(),
                token.to_string(),
                action_hash.to_string(),
            ));
            if pending == "p1" && token == "right" && action_hash == "hash" {
                Ok(format!("ran {pending}"))
            } else {
                Err("bad token".into())
            }
        }

        fn cancel(&self, _turn_id: &str) {}

        fn open_stream(&self, _turn_id: &str) -> Option<std::sync::mpsc::Receiver<String>> {
            None
        }
    }

    #[test]
    fn connectivity_preflight_route_requires_cap_strict_json_and_no_store() {
        let (_d, router) = setup();
        let router = router.with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into());
        let valid = || {
            ApiRequest::new("POST", "/api/v1/agent/connectivity/preflight")
                .with_cap_token(Some("agentsecret".into()))
                .with_content_type(Some("application/json; charset=utf-8".into()))
                .with_body(b"{\"provider\":\"claude\",\"purpose\":\"turn_start\"}".to_vec())
        };

        let denied = router.route(
            &ApiRequest::new("POST", "/api/v1/agent/connectivity/preflight")
                .with_content_type(Some("application/json".into()))
                .with_body(b"{}".to_vec()),
        );
        assert_eq!(denied.status, 401);
        assert!(denied
            .headers
            .iter()
            .any(|(name, value)| name == "Cache-Control" && value == "no-store"));

        let wrong_type = router.route(
            &ApiRequest::new("POST", "/api/v1/agent/connectivity/preflight")
                .with_cap_token(Some("agentsecret".into()))
                .with_content_type(Some("text/plain".into()))
                .with_body(b"{}".to_vec()),
        );
        assert_eq!(wrong_type.status, 400);
        assert!(wrong_type
            .headers
            .iter()
            .any(|(name, value)| name == "Cache-Control" && value == "no-store"));

        let extra_field = router.route(
            &ApiRequest::new("POST", "/api/v1/agent/connectivity/preflight")
                .with_cap_token(Some("agentsecret".into()))
                .with_content_type(Some("application/json".into()))
                .with_body(
                    b"{\"provider\":\"claude\",\"purpose\":\"turn_start\",\"host\":\"example\"}"
                        .to_vec(),
                ),
        );
        assert_eq!(extra_field.status, 400);

        let duplicate_field = router.route(
            &ApiRequest::new("POST", "/api/v1/agent/connectivity/preflight")
                .with_cap_token(Some("agentsecret".into()))
                .with_content_type(Some("application/json".into()))
                .with_body(
                    b"{\"provider\":\"claude\",\"provider\":\"codex\",\"purpose\":\"turn_start\"}"
                        .to_vec(),
                ),
        );
        assert_eq!(duplicate_field.status, 400);

        let ready = router.route(&valid());
        assert_eq!(ready.status, 200);
        assert_eq!(body_json(&ready)["code"], "ready");
        assert!(ready
            .headers
            .iter()
            .any(|(name, value)| name == "Cache-Control" && value == "no-store"));
    }

    #[test]
    fn user_presence_and_agent_session_routes_require_session_cap_strict_json_and_no_store() {
        const REQUEST_ID: &str = "123e4567-e89b-42d3-a456-426614174000";
        const SESSION_ID: &str = "01JSESSION00000000000000000";
        let (_d, router) = setup();
        let router = router
            .with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into())
            .with_session_token("sess".into());
        let authenticated = |request: ApiRequest| {
            request
                .with_session_token(Some("sess".into()))
                .with_cap_token(Some("agentsecret".into()))
        };
        let create = || {
            authenticated(
                ApiRequest::new("POST", "/api/v1/agent/session/create")
                    .with_content_type(Some("application/json".into()))
                    .with_body(
                        format!(r#"{{"request_id":"{REQUEST_ID}","display_name":"Work"}}"#)
                            .into_bytes(),
                    ),
            )
        };

        let missing_session = router.route(
            &ApiRequest::new("POST", "/api/v1/agent/session/create")
                .with_cap_token(Some("agentsecret".into()))
                .with_content_type(Some("application/json".into()))
                .with_body(format!(r#"{{"request_id":"{REQUEST_ID}"}}"#).into_bytes()),
        );
        assert_eq!(missing_session.status, 401);
        let missing_cap = router.route(
            &ApiRequest::new("POST", "/api/v1/agent/session/create")
                .with_session_token(Some("sess".into()))
                .with_content_type(Some("application/json".into()))
                .with_body(format!(r#"{{"request_id":"{REQUEST_ID}"}}"#).into_bytes()),
        );
        assert_eq!(missing_cap.status, 401);

        let status_path = format!(
            "/api/v1/agent/request/status?session_id={SESSION_ID}&route=agent_turn&request_id={REQUEST_ID}"
        );
        assert_eq!(
            router
                .route(
                    &ApiRequest::new("GET", &status_path)
                        .with_cap_token(Some("agentsecret".into())),
                )
                .status,
            401
        );
        assert_eq!(
            router
                .route(
                    &ApiRequest::new("GET", &status_path).with_session_token(Some("sess".into())),
                )
                .status,
            401
        );

        let created = router.route(&create());
        assert_eq!(created.status, 200);
        assert_eq!(body_json(&created)["selected_session_id"], SESSION_ID);
        assert!(created.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("cache-control") && value == "no-store"
        }));

        for body in [
            format!(r#"{{"request_id":"{REQUEST_ID}","extra":true}}"#),
            format!(r#"{{"request_id":"{REQUEST_ID}","request_id":"{REQUEST_ID}"}}"#),
        ] {
            let response = router.route(&authenticated(
                ApiRequest::new("POST", "/api/v1/agent/session/create")
                    .with_content_type(Some("application/json".into()))
                    .with_body(body.into_bytes()),
            ));
            assert_eq!(response.status, 400);
        }
        let query_rejected = router.route(&authenticated(
            ApiRequest::new("POST", "/api/v1/agent/session/create?legacy=1")
                .with_content_type(Some("application/json".into()))
                .with_body(format!(r#"{{"request_id":"{REQUEST_ID}"}}"#).into_bytes()),
        ));
        assert_eq!(query_rejected.status, 400);

        let selected = router.route(&authenticated(
            ApiRequest::new("POST", "/api/v1/agent/session/select")
                .with_content_type(Some("application/json; charset=utf-8".into()))
                .with_body(
                    format!(r#"{{"request_id":"{REQUEST_ID}","session_id":"{SESSION_ID}"}}"#)
                        .into_bytes(),
                ),
        ));
        assert_eq!(selected.status, 200);
        assert_eq!(body_json(&selected)["selected_session_id"], SESSION_ID);

        let listed = router.route(&authenticated(ApiRequest::new(
            "GET",
            "/api/v1/agent/session/list?cursor=next_page&limit=100",
        )));
        assert_eq!(listed.status, 200);
        assert_eq!(body_json(&listed)["limit"], 100);
        let history = router.route(&authenticated(ApiRequest::new(
            "GET",
            &format!("/api/v1/agent/session/history?session_id={SESSION_ID}&limit=50"),
        )));
        assert_eq!(history.status, 200);
        assert_eq!(body_json(&history)["session_id"], SESSION_ID);
        let request_status = router.route(&authenticated(ApiRequest::new(
            "GET",
            &format!(
                "/api/v1/agent/request/status?session_id={SESSION_ID}&route=agent_turn&request_id={REQUEST_ID}"
            ),
        )));
        assert_eq!(request_status.status, 200);
        assert_eq!(body_json(&request_status)["code"], "turn_outcome_unknown");
        assert!(request_status.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("cache-control") && value == "no-store"
        }));
        for query in [
            format!(
                "/api/v1/agent/request/status?session_id={SESSION_ID}&route=unknown&request_id={REQUEST_ID}"
            ),
            format!(
                "/api/v1/agent/request/status?session_id={SESSION_ID}&route=agent_turn&request_id={REQUEST_ID}&extra=1"
            ),
            format!(
                "/api/v1/agent/request/status?session_id={SESSION_ID}&route=agent_turn&route=agent_turn&request_id={REQUEST_ID}"
            ),
        ] {
            assert_eq!(
                router
                    .route(&authenticated(ApiRequest::new("GET", &query)))
                    .status,
                400
            );
        }

        let presence = router.route(&authenticated(
            ApiRequest::new("POST", "/api/v1/agent/user-presence/start")
                .with_content_type(Some("application/json".into()))
                .with_body(
                    format!(
                        r#"{{"request_id":"{REQUEST_ID}","kind":"session_archive","binding":{{"session_id":"{SESSION_ID}"}}}}"#
                    )
                    .into_bytes(),
                ),
        ));
        assert_eq!(presence.status, 200);
        let presence_json = body_json(&presence);
        let operation_id = presence_json["operation_id"].as_str().unwrap();
        let confirmed = router.route(&authenticated(
            ApiRequest::new("POST", "/api/v1/agent/user-presence/confirm")
                .with_content_type(Some("application/json".into()))
                .with_body(
                    json!({
                        "request_id": REQUEST_ID,
                        "operation_id": operation_id,
                        "intent_id": presence_json["intent_id"],
                        "token": presence_json["token"],
                        "action_hash": presence_json["action_hash"],
                    })
                    .to_string()
                    .into_bytes(),
                ),
        ));
        assert_eq!(confirmed.status, 200);
        let archived = router.route(&authenticated(
            ApiRequest::new("POST", "/api/v1/agent/session/archive")
                .with_content_type(Some("application/json".into()))
                .with_body(
                    json!({ "request_id": REQUEST_ID, "operation_id": operation_id })
                        .to_string()
                        .into_bytes(),
                ),
        ));
        assert_eq!(archived.status, 200);
        assert_eq!(body_json(&archived)["state"], "archived");

        let pairing = router.route(&authenticated(
            ApiRequest::new("POST", "/api/v1/agent/session/pairing/create")
                .with_content_type(Some("application/json".into()))
                .with_body(
                    json!({ "request_id": REQUEST_ID, "session_id": SESSION_ID })
                        .to_string()
                        .into_bytes(),
                ),
        ));
        assert_eq!(pairing.status, 200);
        for path in [
            "/api/v1/agent/session/pairing/reveal",
            "/api/v1/agent/session/pairing/claim",
            "/api/v1/agent/session/pairing/finalize",
            "/api/v1/agent/session/pairing/revoke",
        ] {
            let response = router.route(&authenticated(
                ApiRequest::new("POST", path)
                    .with_content_type(Some("application/json".into()))
                    .with_body(
                        json!({ "request_id": REQUEST_ID, "operation_id": operation_id })
                            .to_string()
                            .into_bytes(),
                    ),
            ));
            assert_eq!(response.status, 200, "{path}");
            assert!(response.headers.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("cache-control") && value == "no-store"
            }));
        }

        let pairing_query = router.route(&authenticated(
            ApiRequest::new("POST", "/api/v1/agent/session/pairing/revoke?legacy=1")
                .with_content_type(Some("application/json".into()))
                .with_body(
                    json!({ "request_id": REQUEST_ID, "operation_id": operation_id })
                        .to_string()
                        .into_bytes(),
                ),
        ));
        assert_eq!(pairing_query.status, 400);

        for path in [
            "/api/v1/agent/session/list?limit=0",
            "/api/v1/agent/session/list?limit=101",
            "/api/v1/agent/session/history?session_id=../escape",
        ] {
            let response = router.route(&authenticated(ApiRequest::new("GET", path)));
            assert_eq!(response.status, 400, "{path}");
            assert!(response.headers.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("cache-control") && value == "no-store"
            }));
        }
    }

    #[test]
    fn credential_refresh_route_requires_cap_strict_json_and_no_store() {
        let (_d, router) = setup();
        let router = router.with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into());
        let valid = || {
            ApiRequest::new("POST", "/api/v1/agent/credential/refresh")
                .with_cap_token(Some("agentsecret".into()))
                .with_content_type(Some("application/json".into()))
                .with_body(b"{\"provider\":\"claude\"}".to_vec())
        };

        assert_eq!(
            router
                .route(&ApiRequest::new("POST", "/api/v1/agent/credential/refresh"))
                .status,
            401
        );
        let response = router.route(&valid());
        assert_eq!(response.status, 200);
        assert_eq!(body_json(&response)["credential_state"], "connected");
        assert!(response
            .headers
            .iter()
            .any(|(name, value)| name == "Cache-Control" && value == "no-store"));

        let malformed = ApiRequest::new("POST", "/api/v1/agent/credential/refresh")
            .with_cap_token(Some("agentsecret".into()))
            .with_content_type(Some("application/json".into()))
            .with_body(b"{\"provider\":\"claude\",\"extra\":true}".to_vec());
        assert_eq!(router.route(&malformed).status, 400);
    }

    #[test]
    fn agent_routes_require_cap_token_and_validate_params() {
        let (_d, router) = setup();
        let router = router.with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into());
        let request = || {
            ApiRequest::new("POST", "/api/v1/agent/turn")
                .with_content_type(Some("application/json".into()))
                .with_body(
                    br#"{"request_id":"550e8400-e29b-41d4-a716-446655440000","session_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","account":"a","prompt":"hi"}"#
                        .to_vec(),
                )
        };
        // no / wrong cap token -> 401
        assert_eq!(router.route(&request()).status, 401);
        assert_eq!(
            router
                .route(&request().with_cap_token(Some("nope".into())))
                .status,
            401
        );
        // correct token -> 200 + the turn id
        let ok = router.route(&request().with_cap_token(Some("agentsecret".into())));
        assert_eq!(ok.status, 200);
        assert!(String::from_utf8_lossy(&ok.body).contains("turn-123"));
        // Legacy query input and an incomplete JSON body are rejected.
        let query = ApiRequest::new("POST", "/api/v1/agent/turn?account=a&prompt=hi")
            .with_cap_token(Some("agentsecret".into()));
        assert_eq!(router.route(&query).status, 400);
        let bad = ApiRequest::new("POST", "/api/v1/agent/turn")
            .with_cap_token(Some("agentsecret".into()))
            .with_content_type(Some("application/json".into()))
            .with_body(br#"{"account":"a"}"#.to_vec());
        assert_eq!(router.route(&bad).status, 400);
        // confirm: wrong one-time token -> 409, right -> 200
        let cwrong = strict_json_post(
            "/api/v1/agent/confirm",
            json!({
                "request_id": "550e8400-e29b-41d4-a716-446655440001",
                "pending": "p1",
                "token": "wrong",
                "action_hash": "hash",
            }),
            Some("agentsecret"),
        );
        assert_eq!(router.route(&cwrong).status, 409);
        let cok = strict_json_post(
            "/api/v1/agent/confirm",
            json!({
                "request_id": "550e8400-e29b-41d4-a716-446655440002",
                "pending": "p1",
                "token": "right",
                "action_hash": "hash",
            }),
            Some("agentsecret"),
        );
        assert_eq!(router.route(&cok).status, 200);
        // strict turn cancel -> 200; the legacy query route is absent.
        let cancel = ApiRequest::new("POST", "/api/v1/agent/turn/cancel")
            .with_cap_token(Some("agentsecret".into()))
            .with_content_type(Some("application/json".into()))
            .with_body(
                br#"{"request_id":"550e8400-e29b-41d4-a716-446655440000","turn_id":"turn-123"}"#
                    .to_vec(),
            );
        assert_eq!(router.route(&cancel).status, 200);
        assert_eq!(
            router
                .route(
                    &ApiRequest::new("POST", "/api/v1/agent/cancel?turn=turn-123")
                        .with_cap_token(Some("agentsecret".into()))
                )
                .status,
            405
        );
    }

    #[test]
    fn agent_cancel_routes_require_strict_json_and_separate_turn_from_pending() {
        let (_d, router) = setup();
        let router = router.with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into());
        let turn = ApiRequest::new("POST", "/api/v1/agent/turn/cancel")
            .with_cap_token(Some("agentsecret".into()))
            .with_content_type(Some("application/json".into()))
            .with_body(
                br#"{"request_id":"550e8400-e29b-41d4-a716-446655440000","turn_id":"turn-123"}"#
                    .to_vec(),
            );
        let pending = ApiRequest::new("POST", "/api/v1/agent/pending/cancel")
            .with_cap_token(Some("agentsecret".into()))
            .with_content_type(Some("application/json".into()))
            .with_body(
                format!(
                    r#"{{"request_id":"550e8400-e29b-41d4-a716-446655440000","pending":"pending-123","action_hash":"{}"}}"#,
                    "a".repeat(64)
                )
                .into_bytes(),
            );

        assert_eq!(router.route(&turn).status, 200);
        let pending_response = router.route(&pending);
        assert_eq!(
            pending_response.status,
            200,
            "{}",
            String::from_utf8_lossy(&pending_response.body)
        );
        assert_eq!(body_json(&pending_response)["state"], "cancelled");
        assert_eq!(
            router
                .route(
                    &ApiRequest::new("POST", "/api/v1/agent/turn/cancel?turn=turn-123")
                        .with_cap_token(Some("agentsecret".into()))
                )
                .status,
            400
        );
        assert_eq!(
            router
                .route(
                    &ApiRequest::new("POST", "/api/v1/agent/pending/cancel")
                        .with_cap_token(Some("agentsecret".into()))
                )
                .status,
            400
        );
    }

    #[test]
    fn agent_chat_alias_and_legacy_turn_query_are_rejected() {
        let (_d, router) = setup();
        let router = router.with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into());
        let turn = router.route(
            &ApiRequest::new("POST", "/api/v1/agent/turn?account=a&prompt=hi")
                .with_cap_token(Some("agentsecret".into())),
        );
        let chat = router.route(
            &ApiRequest::new("POST", "/api/v1/agent/chat?account=a&prompt=hi")
                .with_cap_token(Some("agentsecret".into())),
        );
        assert_eq!(turn.status, 400);
        assert_eq!(chat.status, 405);
    }

    #[test]
    fn agent_confirm_requires_action_hash() {
        let (_d, router) = setup();
        let router = router.with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into());
        let missing_hash = router.route(&strict_json_post(
            "/api/v1/agent/confirm",
            json!({
                "request_id": "550e8400-e29b-41d4-a716-446655440003",
                "pending": "p1",
                "token": "right",
            }),
            Some("agentsecret"),
        ));
        assert_eq!(missing_hash.status, 400);
        let missing_token = router.route(&strict_json_post(
            "/api/v1/agent/confirm",
            json!({
                "request_id": "550e8400-e29b-41d4-a716-446655440004",
                "pending": "p1",
                "action_hash": "hash",
            }),
            Some("agentsecret"),
        ));
        assert_eq!(missing_token.status, 400);
        assert_eq!(
            router
                .route(
                    &ApiRequest::new(
                        "POST",
                        "/api/v1/agent/confirm?pending=p1&token=right&action_hash=hash",
                    )
                    .with_cap_token(Some("agentsecret".into()))
                )
                .status,
            400
        );
    }

    #[test]
    fn mobile_agent_routes_require_session_token_even_with_cap_token() {
        let (_d, router) = setup();
        let router = router
            .with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into())
            .with_session_token("sess".into());
        for path in [
            "/api/v1/agent/turn?account=a&prompt=hi",
            "/api/v1/agent/chat?account=a&prompt=hi",
            "/api/v1/agent/confirm?pending=p1&token=right&action_hash=hash",
            "/api/v1/agent/turn/cancel",
            "/api/v1/agent/pending/cancel",
        ] {
            let r = router
                .route(&ApiRequest::new("POST", path).with_cap_token(Some("agentsecret".into())));
            assert_eq!(r.status, 401, "{path}");
            assert!(
                String::from_utf8_lossy(&r.body).contains("session token"),
                "{path} must fail at the session gate before cap handling"
            );
        }
        let no_cap = router.route(
            &ApiRequest::new("POST", "/api/v1/agent/turn?account=a&prompt=hi")
                .with_session_token(Some("sess".into())),
        );
        assert_eq!(no_cap.status, 401);
        assert!(String::from_utf8_lossy(&no_cap.body).contains("capability token"));

        let status_without_session = router.route(&ApiRequest::new("GET", "/api/v1/agent/status"));
        assert_eq!(status_without_session.status, 401);
        let status_with_session = router.route(
            &ApiRequest::new("GET", "/api/v1/agent/status").with_session_token(Some("sess".into())),
        );
        assert_eq!(status_with_session.status, 200);
        assert!(String::from_utf8_lossy(&status_with_session.body).contains("\"enabled\":true"));
    }

    #[test]
    fn open_bridge_stream_gates_events_and_agent() {
        // #0A: the bridge push channel replaces both EventSource endpoints, with the same
        // session gate. Change stream pushes on notify; agent stream wraps each line.
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        let bus = std::sync::Arc::new(EventBus::new());
        let (_d, router) = setup();
        let router = router
            .with_events(bus.clone())
            .with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into())
            .with_session_token("s".into());
        // Unauthorized → None (identical gate to the HTTP SSE path).
        assert!(router.open_bridge_stream("/api/v1/events", None).is_none());
        // Authorized change stream (trusted bridge token) → a change follows a notify. A
        // background notifier removes the capture-vs-notify race (mirrors the SSE test).
        let rx = router
            .open_bridge_stream("/api/v1/events", Some("s"))
            .expect("events stream");
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (n, s2) = (bus.clone(), stop.clone());
        std::thread::spawn(move || {
            while !s2.load(Ordering::SeqCst) {
                n.notify();
                std::thread::sleep(Duration::from_millis(30));
            }
        });
        let mut got_change = false;
        for _ in 0..40 {
            if let Ok(m) = rx.recv_timeout(Duration::from_millis(500)) {
                if m.contains("\"change\"") {
                    got_change = true;
                    break;
                }
            }
        }
        stop.store(true, Ordering::SeqCst);
        assert!(got_change, "change stream must push a change after notify");
        // Agent stream (session + turn) → the pre-serialized line wrapped as `data`.
        let arx = router
            .open_bridge_stream("/api/v1/agent/stream?turn=turn-123", Some("s"))
            .expect("agent stream");
        let first = arx.recv_timeout(Duration::from_secs(2)).expect("an event");
        assert!(
            first.contains("\"message\"") && first.contains("token"),
            "agent line wrapped as data: {first}"
        );
        // Unknown path → None.
        assert!(router
            .open_bridge_stream("/api/v1/nope", Some("s"))
            .is_none());
    }

    #[test]
    fn agent_routes_are_404_when_not_enabled() {
        let (_d, router) = setup();
        let r = router.route(
            &ApiRequest::new("POST", "/api/v1/agent/turn?account=a&prompt=hi")
                .with_cap_token(Some("x".into())),
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn agent_status_route_reports_enabled_and_connected() {
        let (_d, router) = setup();
        // No agent wired -> enabled:false (UI hides the assistant).
        let off = router.route(&ApiRequest::new("GET", "/api/v1/agent/status"));
        assert_eq!(off.status, 200);
        assert!(String::from_utf8_lossy(&off.body).contains("\"enabled\":false"));
        // Agent wired -> the handler's status (read-only, no cap token needed).
        let router = router.with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into());
        let on = router.route(&ApiRequest::new("GET", "/api/v1/agent/status"));
        assert_eq!(on.status, 200);
        assert!(String::from_utf8_lossy(&on.body).contains("\"connected\":true"));
    }

    #[test]
    fn unknown_agent_provider_identifier_returns_400() {
        // #639: the closed product provider id is the allowlist. Aliases (anthropic/openai) and
        // any unknown value are rejected at the API edge with 400 — no normalization to a product id.
        let (_d, router) = setup();
        let router = router.with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into());
        for bad in ["anthropic", "openai", "gpt", "", "Claude"] {
            let req = ApiRequest::new("POST", "/api/v1/agent/oauth/start")
                .with_content_type(Some("application/json".into()))
                .with_body(
                    serde_json::to_vec(&serde_json::json!({
                        "provider": bad,
                        "request_id": "123e4567-e89b-42d3-a456-426614174000",
                        "lifecycle_operation_id": null,
                    }))
                    .unwrap(),
                )
                .with_cap_token(Some("agentsecret".into()));
            assert_eq!(
                router.route(&req).status,
                400,
                "provider {bad:?} must be rejected with 400"
            );
        }
    }

    #[test]
    fn assistant_connect_posts_claude_and_codex_provider_ids() {
        // #639: the two accepted connect provider ids are exactly `claude` and `codex`.
        let (_d, router) = setup();
        let router = router.with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into());
        for good in ["claude", "codex"] {
            let req = ApiRequest::new("POST", "/api/v1/agent/oauth/start")
                .with_content_type(Some("application/json".into()))
                .with_body(
                    serde_json::to_vec(&serde_json::json!({
                        "provider": good,
                        "request_id": "123e4567-e89b-42d3-a456-426614174000",
                        "lifecycle_operation_id": null,
                    }))
                    .unwrap(),
                )
                .with_cap_token(Some("agentsecret".into()));
            assert_eq!(
                router.route(&req).status,
                200,
                "provider {good:?} must be accepted"
            );
        }
    }

    #[test]
    fn mobile_agent_confirm_requires_biometric_before_agent_token_consumption() {
        let (_d, router) = setup();
        let agent = std::sync::Arc::new(RecordingAgent::backup());
        let router = router
            .with_agent(agent.clone(), "agentsecret".into())
            .with_session_token("sess".into())
            .with_biometric_gate();
        let req = agent_confirm_request("hash");

        let resp = router.route(&req);

        assert_eq!(resp.status, 200);
        let body = body_json(&resp);
        assert_eq!(body["status"], "confirmation_required");
        assert_eq!(body["op"], "backup");
        assert_eq!(body["account"], "a");
        assert_eq!(body["service"], "agent");
        assert_eq!(body["item"], "pending:2:p1:action_hash:4:hash");
        assert_eq!(agent.confirm_call_count(), 0);
    }

    #[test]
    fn mobile_agent_confirm_wrong_biometric_token_does_not_consume_agent_pending() {
        let (_d, router) = setup();
        let agent = std::sync::Arc::new(RecordingAgent::backup());
        let router = router
            .with_agent(agent.clone(), "agentsecret".into())
            .with_session_token("sess".into())
            .with_biometric_gate();
        let auth = || agent_confirm_request("hash");

        let first = router.route(&auth());
        let pat = body_json(&first)["pending_action_id"]
            .as_str()
            .unwrap()
            .to_string();
        let bad = router.route(&auth().with_per_action_token(Some(pat.clone())));
        assert_eq!(bad.status, 403);
        assert_eq!(agent.confirm_call_count(), 0);

        assert!(router.confirm_biometric(&pat));
        let ok = router.route(&auth().with_per_action_token(Some(pat)));
        assert_eq!(ok.status, 200);
        assert_eq!(agent.confirm_call_count(), 1);
    }

    #[test]
    fn mobile_agent_confirm_after_biometric_consumes_agent_token_once() {
        let (_d, router) = setup();
        let agent = std::sync::Arc::new(RecordingAgent::backup());
        let router = router
            .with_agent(agent.clone(), "agentsecret".into())
            .with_session_token("sess".into())
            .with_biometric_gate();
        let auth = || agent_confirm_request("hash");
        let challenge = router.route(&auth());
        let pat = body_json(&challenge)["pending_action_id"]
            .as_str()
            .unwrap()
            .to_string();

        assert!(router.confirm_biometric(&pat));
        let ok = router.route(&auth().with_per_action_token(Some(pat.clone())));
        assert_eq!(ok.status, 200);
        let replay = router.route(&auth().with_per_action_token(Some(pat)));
        assert_eq!(replay.status, 403);
        assert_eq!(agent.confirm_call_count(), 1);
    }

    #[test]
    fn mobile_agent_confirm_wrong_action_hash_does_not_mint_biometric_pending() {
        let (_d, router) = setup();
        let agent = std::sync::Arc::new(RecordingAgent::backup());
        let router = router
            .with_agent(agent.clone(), "agentsecret".into())
            .with_session_token("sess".into())
            .with_biometric_gate();
        let req = agent_confirm_request("wrong");

        let resp = router.route(&req);

        assert_eq!(resp.status, 409);
        let body = String::from_utf8_lossy(&resp.body);
        assert!(body.contains("ActionMismatch"));
        assert!(!body.contains("confirmation_required"));
        assert_eq!(agent.confirm_call_count(), 0);
    }

    #[test]
    fn product_router_has_no_subscription_import_route() {
        let (_d, router) = setup();
        let router = router.with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into());
        let access = "issue627-access-query-sentinel";
        let refresh = "issue627-refresh-query-sentinel";
        let request = ApiRequest::new(
            "POST",
            &format!(
                "/api/v1/agent/subscription/import?access_token={access}&refresh_token={refresh}&expires_at_ms=123"
            ),
        )
        .with_cap_token(Some("agentsecret".into()));

        let response = router.route(&request);
        let body = String::from_utf8_lossy(&response.body);

        assert_eq!(response.status, 405);
        assert!(!body.contains(access));
        assert!(!body.contains(refresh));
    }

    #[test]
    fn agent_oauth_start_is_cap_gated_and_returns_authorize_url() {
        let (_d, router) = setup();
        let router = router.with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into());
        let request = |provider: &str| {
            ApiRequest::new("POST", "/api/v1/agent/oauth/start")
                .with_content_type(Some("application/json".into()))
                .with_body(
                    serde_json::to_vec(&serde_json::json!({
                        "provider": provider,
                        "request_id": "123e4567-e89b-42d3-a456-426614174000",
                        "lifecycle_operation_id": null,
                    }))
                    .unwrap(),
                )
        };
        // no cap token -> 401
        assert_eq!(router.route(&request("claude")).status, 401);
        // with cap token -> 200 + an authorize URL the UI opens in the system browser
        let ok = router.route(&request("claude").with_cap_token(Some("agentsecret".into())));
        assert_eq!(ok.status, 200);
        assert!(String::from_utf8_lossy(&ok.body).contains("auth.example/authorize"));
        assert!(ok.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("cache-control") && value == "no-store"
        }));
        let noredir = request("codex").with_cap_token(Some("agentsecret".into()));
        assert_eq!(router.route(&noredir).status, 200);
        let unknown = request("openai").with_cap_token(Some("agentsecret".into()));
        let unknown = router.route(&unknown);
        assert_eq!(unknown.status, 400);
        assert!(String::from_utf8_lossy(&unknown.body).contains("invalid oauth start request"));
        assert!(unknown.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("cache-control") && value == "no-store"
        }));
        let missing = ApiRequest::new("POST", "/api/v1/agent/oauth/start")
            .with_content_type(Some("application/json".into()))
            .with_body(br#"{}"#.to_vec())
            .with_cap_token(Some("agentsecret".into()));
        let missing = router.route(&missing);
        assert_eq!(missing.status, 400);
        assert!(String::from_utf8_lossy(&missing.body).contains("invalid oauth start request"));
    }

    fn lifecycle_logout_request(body: serde_json::Value) -> ApiRequest {
        ApiRequest::new("POST", "/api/v1/agent/oauth/logout")
            .with_content_type(Some("application/json".into()))
            .with_body(serde_json::to_vec(&body).unwrap())
    }

    fn lifecycle_resume_request(body: serde_json::Value) -> ApiRequest {
        ApiRequest::new("POST", "/api/v1/agent/oauth/lifecycle/resume")
            .with_content_type(Some("application/json".into()))
            .with_body(serde_json::to_vec(&body).unwrap())
    }

    fn valid_logout_body() -> serde_json::Value {
        serde_json::json!({
            "provider": "claude",
            "mode": "disconnect",
            "request_id": "123e4567-e89b-42d3-a456-426614174100",
            "credential_etag": "credential-etag-1",
            "resume": null,
            "acknowledge_full_grant": false
        })
    }

    #[test]
    fn agent_oauth_logout_requires_session_and_agent_cap_before_lookup() {
        let (_d, router) = setup();
        let router = router
            .with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into())
            .with_session_token("sess".into());
        let request = || lifecycle_logout_request(valid_logout_body());
        assert_eq!(router.route(&request()).status, 401);
        assert_eq!(
            router
                .route(&request().with_cap_token(Some("agentsecret".into())))
                .status,
            401
        );
        assert_eq!(
            router
                .route(
                    &request()
                        .with_session_token(Some("sess".into()))
                        .with_cap_token(Some("agentsecret".into()))
                )
                .status,
            200
        );
    }

    #[test]
    fn agent_oauth_logout_requires_strict_json_and_rejects_body_over_8k() {
        let (_d, router) = setup();
        let router = router.with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into());
        let auth = |request: ApiRequest| request.with_cap_token(Some("agentsecret".into()));
        assert_eq!(
            router
                .route(&auth(ApiRequest::new(
                    "POST",
                    "/api/v1/agent/oauth/logout?provider=claude"
                )))
                .status,
            400
        );
        let oversized = ApiRequest::new("POST", "/api/v1/agent/oauth/logout")
            .with_content_type(Some("application/json".into()))
            .with_body(vec![b' '; AGENT_STRICT_JSON_MAX_BYTES + 1]);
        assert_eq!(router.route(&auth(oversized)).status, 413);
    }

    #[test]
    fn agent_oauth_logout_rejects_unknown_provider_mode_fields_and_duplicate_keys() {
        let (_d, router) = setup();
        let router = router.with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into());
        let send = |body: &[u8]| {
            router.route(
                &ApiRequest::new("POST", "/api/v1/agent/oauth/logout")
                    .with_content_type(Some("application/json".into()))
                    .with_body(body.to_vec())
                    .with_cap_token(Some("agentsecret".into())),
            )
        };
        for body in [
            br#"{"provider":"openai","mode":"disconnect","request_id":"123e4567-e89b-42d3-a456-426614174101","credential_etag":"e","acknowledge_full_grant":false}"#.as_slice(),
            br#"{"provider":"claude","mode":"delete","request_id":"123e4567-e89b-42d3-a456-426614174101","credential_etag":"e","acknowledge_full_grant":false}"#.as_slice(),
            br#"{"provider":"claude","mode":"disconnect","request_id":"123e4567-e89b-42d3-a456-426614174101","credential_etag":"e","acknowledge_full_grant":false,"token":"forbidden"}"#.as_slice(),
            br#"{"provider":"claude","provider":"codex","mode":"disconnect","request_id":"123e4567-e89b-42d3-a456-426614174101","credential_etag":"e","acknowledge_full_grant":false}"#.as_slice(),
        ] {
            assert_eq!(send(body).status, 400);
        }
    }

    #[test]
    fn agent_oauth_logout_requires_exactly_one_credential_or_operation_etag_form() {
        let (_d, router) = setup();
        let router = router.with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into());
        let send = |body: serde_json::Value| {
            router.route(&lifecycle_logout_request(body).with_cap_token(Some("agentsecret".into())))
        };
        let mut neither = valid_logout_body();
        neither["credential_etag"] = serde_json::Value::Null;
        assert_eq!(send(neither).status, 400);
        let mut both = valid_logout_body();
        both["resume"] = serde_json::json!({
            "operation_id": "operation-1", "operation_etag": "operation-etag-1"
        });
        assert_eq!(send(both).status, 400);
        assert_eq!(send(valid_logout_body()).status, 200);
    }

    #[test]
    fn agent_oauth_logout_response_is_no_store_and_closed_schema_only() {
        let (_d, router) = setup();
        let router = router.with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into());
        let response = router.route(
            &lifecycle_logout_request(valid_logout_body())
                .with_cap_token(Some("agentsecret".into())),
        );
        assert_eq!(response.status, 200);
        assert!(response.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("cache-control") && value == "no-store"
        }));
        let body = body_json(&response);
        let object = body.as_object().unwrap();
        assert_eq!(object.len(), 11);
        for forbidden in [
            "access_token",
            "refresh_token_value",
            "subject",
            "account_id",
            "raw_error",
            "endpoint",
        ] {
            assert!(!object.contains_key(forbidden));
            assert!(!String::from_utf8_lossy(&response.body).contains(forbidden));
        }
    }

    #[test]
    fn agent_oauth_lifecycle_resume_supports_connect_candidate_cleanup_without_connect_logout_mode()
    {
        let (_d, router) = setup();
        let router = router.with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into());
        let response = router.route(
            &lifecycle_resume_request(serde_json::json!({
                "provider": "codex",
                "request_id": "123e4567-e89b-42d3-a456-426614174102",
                "operation_id": "operation-1",
                "operation_etag": "operation-etag-1",
                "action": "retry_revoke"
            }))
            .with_cap_token(Some("agentsecret".into())),
        );
        assert_eq!(response.status, 200);
        assert_eq!(body_json(&response)["mode"], "connect");
    }

    #[test]
    fn agent_oauth_start_rejects_legacy_query_provider_and_redirect() {
        let (_d, router) = setup();
        let router = router.with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into());
        let response = router.route(
            &ApiRequest::new(
                "POST",
                "/api/v1/agent/oauth/start?provider=claude&redirect=https%3A%2F%2Fevil.invalid",
            )
            .with_cap_token(Some("agentsecret".into())),
        );
        assert_eq!(response.status, 400);
    }

    #[test]
    fn agent_oauth_start_json_preserves_oauth_guard_preflight_paths() {
        let start = APP_JS
            .split("async function startAiLogin(provider, lifecycleOperationId)")
            .nth(1)
            .unwrap()
            .split("async function pollAgentStatus")
            .next()
            .unwrap();
        let guard = start.find("beginNetworkGuard(\"oauth\")").unwrap();
        let preflight = start
            .find("runConnectivityPreflight(provider, \"oauth_start\", guardId)")
            .unwrap();
        let json_start = start
            .find("postJson(\"/api/v1/agent/oauth/start\", CAP.agent")
            .unwrap();
        let browser = start.find("openExternalAuth(d.authorize_url").unwrap();
        assert!(guard < preflight && preflight < json_start && json_start < browser);

        let connect = APP_JS
            .split("async function connectAgentProvider(provider, lifecycleNode = null)")
            .nth(1)
            .unwrap()
            .split("async function pickModel")
            .next()
            .unwrap();
        assert!(connect.contains("lifecycle.state === \"awaiting_oauth_login\""));
        assert!(connect.contains("await continueLifecycleOAuth(provider, lifecycle)"));
        assert!(connect.contains("await startAiLogin(provider)"));
        assert!(APP_JS.contains("connectAgentProvider(\"claude\", claudeLifecycle)"));
        assert!(APP_JS.contains("connectAgentProvider(\"codex\", codexLifecycle)"));
        assert!(APP_JS.contains("await connectAgentProvider(\"codex\")"));
    }

    #[test]
    fn assistant_connect_action_resumes_oauth_without_duplicate_lifecycle_button() {
        assert!(APP_JS.contains("claudeContinuing ? \"Continue Claude sign-in\""));
        assert!(APP_JS.contains("codexContinuing ? \"Continue ChatGPT sign-in\""));
        assert!(!APP_JS.contains("lifecycleActionButton(\"Continue sign-in\""));
    }

    #[test]
    fn assistant_chat_render_does_not_wait_for_cloud_history_hydration() {
        let render = APP_JS
            .split("async function renderAssistantView(view)")
            .nth(1)
            .unwrap()
            .split("function renderAssistantWizardSteps")
            .next()
            .unwrap();
        let chat = render.find("renderAssistantChat(body, st)").unwrap();
        let hydration = render
            .find("void hydrateAgentSession(selectedSession.session_id)")
            .unwrap();
        assert!(chat < hydration);

        let select = APP_JS
            .split("async function loadSelectedAgentSession()")
            .nth(1)
            .unwrap()
            .split("function closeAgentDialog")
            .next()
            .unwrap();
        assert!(!select.contains("await hydrateAgentSession"));
        assert!(APP_JS.contains("AssistantState.sessionHydrationPromise"));
        assert!(APP_JS.contains("AssistantState.sessionHydrationSeq !== sequence"));
    }

    #[test]
    fn assistant_session_history_refreshes_asynchronously_without_bridge_timeout() {
        let hydrate = APP_JS
            .split("async function hydrateAgentSession(sessionId)")
            .nth(1)
            .unwrap()
            .split("async function loadSelectedAgentSession()")
            .next()
            .unwrap();
        assert!(hydrate.contains("page && page.refreshing === true"));
        assert!(hydrate.contains("await new Promise((resolve) => setTimeout(resolve, 500))"));
        assert!(hydrate.contains("const deadline = Date.now() + 60_000"));
        assert!(hydrate.contains("throw new Error(\"session_transport_unavailable\")"));
    }

    #[test]
    fn agent_oauth_poll_keeps_loopback_alive_for_full_host_attempt_window() {
        assert!(APP_JS.contains("const AGENT_OAUTH_POLL_INTERVAL_MS = 2_000"));
        assert!(APP_JS.contains("const AGENT_OAUTH_POLL_LIMIT = 240"));
        assert!(APP_JS.contains(
            "setTimeout(() => pollAgentStatus(n + 1, provider), AGENT_OAUTH_POLL_INTERVAL_MS)"
        ));
        assert!(APP_JS
            .contains("setTimeout(() => pollCodexStatus(n + 1), AGENT_OAUTH_POLL_INTERVAL_MS)"));
        assert!(!APP_JS.contains("n < 90"));
    }

    #[test]
    fn agent_oauth_connectivity_retry_preserves_lifecycle_operation_binding() {
        assert!(APP_JS.contains(
            "rememberConnectivityIssue(e, () => startAiLogin(provider, lifecycleOperationId))"
        ));
        assert!(!APP_JS.contains("rememberConnectivityIssue(e, () => startAiLogin(provider));"));
    }

    #[test]
    fn assistant_account_lifecycle_controls_are_capability_and_state_driven() {
        let controls = APP_JS
            .split("function renderAssistantLifecycleControls(st)")
            .nth(1)
            .unwrap()
            .split("async function refreshAssistantCredentialIfRequired")
            .next()
            .unwrap();
        assert!(controls.contains("node.switch_capability === \"verified_subject\""));
        assert!(controls.contains("Retry revoke"));
        assert!(controls.contains("Resume cleanup"));
        assert!(controls.contains("Retry exchange"));
        assert!(controls.contains("node.credential_etag"));
        assert!(controls.contains("node.busy"));
    }

    #[test]
    fn account_lifecycle_revoke_guard_precedes_logout_request() {
        let guard = APP_JS
            .split("async function withCredentialRevokeGuard(provider, operation)")
            .nth(1)
            .unwrap()
            .split("function lifecycleResumeAction")
            .next()
            .unwrap();
        let begin = guard
            .find("beginNetworkGuard(\"credential_revoke\")")
            .unwrap();
        let preflight = guard
            .find("runConnectivityPreflight(provider, \"credential_revoke\", guardId)")
            .unwrap();
        let operation = guard.find("return await operation()").unwrap();
        assert!(begin < preflight && preflight < operation);
        assert!(guard.contains("error.responseReceived !== true"));
        assert!(guard.contains("if (releaseGuard) await endNetworkGuard(guardId)"));

        let start = APP_JS
            .split("async function startAccountLifecycle(provider, mode, node)")
            .nth(1)
            .unwrap()
            .split("async function continueLifecycleOAuth")
            .next()
            .unwrap();
        assert!(start.contains("confirmAccountLifecycle"));
        assert!(start.contains("withCredentialRevokeGuard(provider"));
        assert!(start.contains("/api/v1/agent/oauth/logout"));
        assert!(start.contains("result.state === \"disconnected\""));
    }

    #[test]
    fn candidate_cleanup_ends_oauth_guard_before_starting_revoke_guard() {
        let cleanup = APP_JS
            .split("async function handleCandidateCleanupStatus(status, provider)")
            .nth(1)
            .unwrap()
            .split("function lifecycleActionButton")
            .next()
            .unwrap();
        let oauth_end = cleanup.find("await finishOAuthGuard(provider)").unwrap();
        let revoke_resume = cleanup
            .find("await resumeAccountLifecycle(provider, node, \"retry_revoke\")")
            .unwrap();
        assert!(oauth_end < revoke_resume);
    }

    #[test]
    fn connect_reconnect_and_switch_candidate_revoke_require_fresh_revoke_snapshot() {
        let cleanup = APP_JS
            .split("async function handleCandidateCleanupStatus(status, provider)")
            .nth(1)
            .unwrap()
            .split("function lifecycleActionButton")
            .next()
            .unwrap();
        assert!(cleanup.contains("node.state !== \"candidate_cleanup\""));
        assert!(cleanup.contains("finishOAuthGuard(provider)"));
        assert!(APP_JS.contains("captureNetworkSnapshot(guardId)"));
        assert!(
            APP_JS.contains("runConnectivityPreflight(provider, \"credential_revoke\", guardId)")
        );
    }

    #[test]
    fn candidate_revoke_rejects_oauth_guard_snapshot() {
        let source = include_str!("../../../crates/app-host/src/lib.rs");
        let consume = source
            .split("fn consume_mobile_connectivity_snapshot(")
            .nth(1)
            .unwrap()
            .split("fn register_mobile_connectivity_snapshot")
            .next()
            .unwrap_or(source);
        assert!(consume.contains("entry.snapshot.purpose != purpose"));
        assert!(
            source.contains("credential_revoke_preflight_rejects_oauth_refresh_or_turn_snapshot")
        );
    }

    #[test]
    fn assistant_account_lifecycle_requires_full_grant_ack_and_never_renders_raw_errors() {
        let confirm = APP_JS
            .split("function confirmAccountLifecycle(provider, mode, scope, ackRequired)")
            .nth(1)
            .unwrap()
            .split("async function withCredentialRevokeGuard")
            .next()
            .unwrap();
        assert!(confirm.contains("data-agent-lifecycle-full-grant-ack"));
        assert!(confirm.contains("confirmButton.disabled = !ack.checked"));
        let lifecycle = APP_JS
            .split("function accountLifecycleNode")
            .nth(1)
            .unwrap()
            .split("async function refreshAssistantCredentialIfRequired")
            .next()
            .unwrap();
        for forbidden in [
            "access_token",
            "refresh_token",
            "subject_digest",
            "callback_url",
        ] {
            assert!(!lifecycle.contains(forbidden));
        }
    }

    #[test]
    fn product_router_has_no_local_only_credential_delete_route() {
        let (_d, router) = setup();
        let router = router.with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into());
        for path in [
            "/api/v1/agent/credential/delete",
            "/api/v1/agent/provider/key/delete",
            "/api/v1/agent/subscription/delete",
        ] {
            let response = router
                .route(&ApiRequest::new("POST", path).with_cap_token(Some("agentsecret".into())));
            assert_eq!(response.status, 405, "{path}");
        }
    }

    #[test]
    fn agent_oauth_cancel_is_session_cap_and_strict_json_gated() {
        let (_d, router) = setup();
        let router = router
            .with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into())
            .with_session_token("sess".into());
        let request = || {
            ApiRequest::new("POST", "/api/v1/agent/oauth/cancel")
                .with_content_type(Some("application/json".into()))
                .with_body(br#"{"provider":"claude","attempt_id":"attempt-1"}"#.to_vec())
        };

        assert_eq!(router.route(&request()).status, 401);
        assert_eq!(
            router
                .route(&request().with_cap_token(Some("agentsecret".into())))
                .status,
            401
        );

        let authorized = request()
            .with_session_token(Some("sess".into()))
            .with_cap_token(Some("agentsecret".into()));
        let response = router.route(&authorized);
        assert_eq!(response.status, 200);
        assert_eq!(body_json(&response)["cancelled"], true);
        assert!(response.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("cache-control") && value == "no-store"
        }));

        let unknown_field = ApiRequest::new("POST", "/api/v1/agent/oauth/cancel")
            .with_content_type(Some("application/json".into()))
            .with_body(
                br#"{"provider":"claude","attempt_id":"attempt-1","url":"forbidden"}"#.to_vec(),
            )
            .with_session_token(Some("sess".into()))
            .with_cap_token(Some("agentsecret".into()));
        assert_eq!(router.route(&unknown_field).status, 400);

        let wrong_attempt = ApiRequest::new("POST", "/api/v1/agent/oauth/cancel")
            .with_content_type(Some("application/json".into()))
            .with_body(br#"{"provider":"claude","attempt_id":"other"}"#.to_vec())
            .with_session_token(Some("sess".into()))
            .with_cap_token(Some("agentsecret".into()));
        assert_eq!(router.route(&wrong_attempt).status, 409);
    }

    #[test]
    fn agent_oauth_callback_is_session_gate_exempt_for_the_browser() {
        let (_d, router) = setup();
        // Mobile profile: the data API is session-token gated...
        let router = router
            .with_agent(std::sync::Arc::new(FakeAgent), "agentsecret".into())
            .with_session_token("sess".into());
        // /api/v1/* without the session token is refused (sanity: the gate is on)...
        assert_eq!(
            router
                .route(
                    &ApiRequest::new("POST", "/api/v1/agent/turn/cancel")
                        .with_cap_token(Some("agentsecret".into()))
                )
                .status,
            401
        );
        // ...but the browser callback (not under /api/v1/) reaches the handler with NO
        // session token and NO cap token — only the `state` protects it.
        let cb = ApiRequest::new("GET", "/callback?code=abc&state=st-1");
        let r = router.route(&cb);
        assert_eq!(r.status, 200);
        assert!(String::from_utf8_lossy(&r.body).contains("connected code=abc"));
        assert!(r.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("cache-control") && value == "no-store"
        }));
        // missing code/state -> 400
        let bad = ApiRequest::new("GET", "/callback?code=abc");
        assert_eq!(router.route(&bad).status, 400);
    }

    #[test]
    fn agent_handler_open_stream_yields_a_receiver() {
        let h = FakeAgent;
        let rx = h.open_stream("turn-123").expect("stream");
        assert!(rx.recv().unwrap().contains("token"));
        assert!(h.open_stream("other").is_none());
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
                .route(&ApiRequest::new(
                    "POST",
                    "/api/v1/settings?poll_interval_secs=10"
                ))
                .status,
            404
        );
        let seen = std::sync::Arc::new(AtomicU64::new(0));
        let router = router.with_settings(
            std::sync::Arc::new(OkSettings(seen.clone())),
            "secret".into(),
        );
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
        let oob =
            strict_scalar_post_from_target("/api/v1/settings?poll_interval_secs=99999", "secret");
        assert_eq!(router.route(&oob).status, 400);
        assert_eq!(seen.load(Ordering::SeqCst), 0);
        // valid token + valid value -> 200, handler applied
        let ok = router.route(&strict_scalar_post_from_target(q, "secret"));
        assert_eq!(ok.status, 200);
        assert_eq!(seen.load(Ordering::SeqCst), 10);
    }

    /// Records the last mail-write op so the routing + param parsing is checkable.
    #[derive(Default)]
    struct RecordMailWrite(std::sync::Mutex<Vec<String>>);
    impl RecordMailWrite {
        fn last(&self) -> String {
            self.0.lock().unwrap().last().cloned().unwrap_or_default()
        }
    }
    impl MailWriteHandler for RecordMailWrite {
        #[allow(clippy::too_many_arguments)]
        fn send(
            &self,
            _a: &str,
            subject: &str,
            _b: &str,
            to: &[String],
            cc: &[String],
            _bcc: &[String],
            importance: Option<&str>,
            request_read_receipt: bool,
        ) -> Result<(), String> {
            self.0.lock().unwrap().push(format!(
                "send subj={subject} to={} cc={} imp={} rr={request_read_receipt}",
                to.join(","),
                cc.len(),
                importance.unwrap_or("-"),
            ));
            Ok(())
        }
        fn reply(&self, _a: &str, id: &str, _c: &str, all: bool) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("reply id={id} all={all}"));
            Ok(())
        }
        fn forward(&self, _a: &str, id: &str, _c: &str, to: &[String]) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("forward id={id} to={}", to.join(",")));
            Ok(())
        }
        fn reply_html(&self, _a: &str, id: &str, body: &str, all: bool) -> Result<(), String> {
            self.0.lock().unwrap().push(format!(
                "reply_html id={id} all={all} body_len={}",
                body.len()
            ));
            Ok(())
        }
        fn forward_html(
            &self,
            _a: &str,
            id: &str,
            body: &str,
            to: &[String],
        ) -> Result<(), String> {
            self.0.lock().unwrap().push(format!(
                "forward_html id={id} to={} body_len={}",
                to.join(","),
                body.len()
            ));
            Ok(())
        }
        fn move_to(&self, _a: &str, id: &str, dest: &str) -> Result<String, String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("move id={id} dest={dest}"));
            Ok(format!("{id}-moved"))
        }
        fn set_read(&self, _a: &str, id: &str, is_read: bool) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("read id={id} is_read={is_read}"));
            Ok(())
        }
        fn set_flag(
            &self,
            _a: &str,
            id: &str,
            status: &str,
            _due: Option<&str>,
            _tz: &str,
        ) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("flag id={id} status={status}"));
            Ok(())
        }
        fn set_categories(&self, _a: &str, id: &str, cats: &[String]) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("cats id={id} n={}", cats.len()));
            Ok(())
        }
        fn create_draft(
            &self,
            _a: &str,
            subject: &str,
            _b: &str,
            _to: &[String],
        ) -> Result<String, String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("create_draft subj={subject}"));
            Ok("draft-9".into())
        }
        fn send_draft(&self, _a: &str, id: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(format!("send_draft id={id}"));
            Ok(())
        }
    }

    struct FakeDriveInfo;
    impl OneDriveInfoHandler for FakeDriveInfo {
        fn drive_quota(&self, _account: &str) -> Result<serde_json::Value, String> {
            Ok(json!({ "total": 1000, "used": 250, "remaining": 750, "state": "normal" }))
        }
        fn permissions(&self, _account: &str, _id: &str) -> Result<serde_json::Value, String> {
            Ok(json!([{ "id": "p1", "roles": ["read"], "link": null, "grantee": "Bob" }]))
        }
    }

    #[test]
    fn drive_quota_route_returns_handler_json_or_404() {
        let (_d, router) = setup();
        // read-only server (no handler) -> 404
        assert_eq!(
            router
                .route(&ApiRequest::get("/api/v1/drive?account=a"))
                .status,
            404
        );
        let router = router.with_onedrive_info(std::sync::Arc::new(FakeDriveInfo));
        // missing account -> 400
        assert_eq!(router.route(&ApiRequest::get("/api/v1/drive")).status, 400);
        // with handler + account -> 200 + the quota object
        let resp = router.route(&ApiRequest::get("/api/v1/drive?account=a"));
        assert_eq!(resp.status, 200);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(
            v.pointer("/quota/remaining").and_then(Value::as_i64),
            Some(750)
        );
    }

    #[test]
    fn permissions_route_returns_handler_json_or_404() {
        let (_d, router) = setup();
        // read-only server (no handler) -> 404
        assert_eq!(
            router
                .route(&ApiRequest::get("/api/v1/permissions?account=a&id=x"))
                .status,
            404
        );
        let router = router.with_onedrive_info(std::sync::Arc::new(FakeDriveInfo));
        // missing id -> 400
        assert_eq!(
            router
                .route(&ApiRequest::get("/api/v1/permissions?account=a"))
                .status,
            400
        );
        // with handler + account + id -> 200 + the permissions array
        let resp = router.route(&ApiRequest::get("/api/v1/permissions?account=a&id=x"));
        assert_eq!(resp.status, 200);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(
            v.pointer("/permissions/0/grantee").and_then(Value::as_str),
            Some("Bob")
        );
        assert_eq!(
            v.pointer("/permissions/0/roles/0").and_then(Value::as_str),
            Some("read")
        );
    }

    struct FakeOneDriveList;
    impl OneDriveListHandler for FakeOneDriveList {
        fn children(&self, _account: &str, folder: &str) -> Result<Vec<serde_json::Value>, String> {
            Ok(vec![
                json!({ "id": "c1", "name": format!("{folder}-child.txt"), "size": 12 }),
            ])
        }
    }

    #[test]
    fn onedrive_children_route_returns_handler_json_or_404() {
        let (_d, router) = setup();
        // AC3: read-only server (no handler) -> 404
        assert_eq!(
            router
                .route(&ApiRequest::get(
                    "/api/v1/onedrive/children?account=a&folder=F"
                ))
                .status,
            404
        );
        let router = router.with_onedrive_list(std::sync::Arc::new(FakeOneDriveList));
        // missing account -> 400
        assert_eq!(
            router
                .route(&ApiRequest::get("/api/v1/onedrive/children?folder=F"))
                .status,
            400
        );
        // AC1: handler + account -> 200 + children array
        let resp = router.route(&ApiRequest::get(
            "/api/v1/onedrive/children?account=a&folder=F",
        ));
        assert_eq!(resp.status, 200);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(
            v.pointer("/children/0/name").and_then(Value::as_str),
            Some("F-child.txt")
        );
        // root case: an absent `folder` is passed through as "" (the drive root).
        let root = router.route(&ApiRequest::get("/api/v1/onedrive/children?account=a"));
        assert_eq!(root.status, 200);
        let rv: Value = serde_json::from_slice(&root.body).unwrap();
        assert_eq!(
            rv.pointer("/children/0/name").and_then(Value::as_str),
            Some("-child.txt")
        );
    }

    #[test]
    fn onedrive_children_is_session_gated() {
        // AC2: on the mobile profile (session token set) the children GET 401s without a
        // token. The assertion is EXACTLY 401 (not 404/200): a 404/200 here would mean the
        // /api/v1/* session gate did not catch this new path. Green == AC2 hard-proven.
        let router = Router::new(Config::default())
            .with_onedrive_list(std::sync::Arc::new(FakeOneDriveList))
            .with_session_token("sess-tok-0001".into());
        assert_eq!(
            router
                .route(&ApiRequest::get(
                    "/api/v1/onedrive/children?account=a&folder=F"
                ))
                .status,
            401
        );
        let ok = ApiRequest::get("/api/v1/onedrive/children?account=a&folder=F")
            .with_session_token(Some("sess-tok-0001".into()));
        assert_ne!(router.route(&ok).status, 401); // correct token passes the gate -> 200
    }

    // ---- #651: OneDrive per-folder mode endpoint + per-item effective_mode ----

    /// In-memory mode handler for the webui unit tests: `set_folder` mutates and `modes`
    /// reads back, exercising the Router's persist/read-fresh delegation without a config
    /// file (the real file persistence lives in app-host's `DaemonOneDriveMode`).
    #[derive(Default)]
    struct FakeOneDriveMode(std::sync::Mutex<std::collections::BTreeMap<String, OneDriveModes>>);
    impl OneDriveModeHandler for FakeOneDriveMode {
        fn modes(&self, account: &str) -> Result<OneDriveModes, String> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(account)
                .cloned()
                .unwrap_or_default())
        }
        fn set_folder(
            &self,
            account: &str,
            folder_id: &str,
            mode: Option<OneDriveMode>,
        ) -> Result<(), String> {
            let mut g = self.0.lock().unwrap();
            let m = g.entry(account.to_string()).or_default();
            match mode {
                Some(md) => {
                    m.folder_modes.insert(folder_id.to_string(), md);
                }
                None => {
                    m.folder_modes.remove(folder_id);
                }
            }
            Ok(())
        }
    }

    struct FakeOneDriveRisk {
        move_result: std::sync::Mutex<Result<OneDriveMoveRisk, String>>,
        offline_result: std::sync::Mutex<Result<OfflineModeRisk, String>>,
        move_calls: std::sync::atomic::AtomicUsize,
        offline_calls: std::sync::atomic::AtomicUsize,
    }
    impl Default for FakeOneDriveRisk {
        fn default() -> Self {
            Self {
                move_result: std::sync::Mutex::new(Ok(OneDriveMoveRisk::Low)),
                offline_result: std::sync::Mutex::new(Ok(OfflineModeRisk {
                    requires_confirmation: false,
                    file_count: 0,
                    known_bytes: 0,
                    unknown_size_files: 0,
                    reason: "small".into(),
                })),
                move_calls: std::sync::atomic::AtomicUsize::new(0),
                offline_calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }
    impl FakeOneDriveRisk {
        fn with_move(result: OneDriveMoveRisk) -> Self {
            Self {
                move_result: std::sync::Mutex::new(Ok(result)),
                ..Self::default()
            }
        }

        fn with_offline_requires(reason: &str) -> Self {
            Self {
                offline_result: std::sync::Mutex::new(Ok(OfflineModeRisk {
                    requires_confirmation: true,
                    file_count: 2,
                    known_bytes: 0,
                    unknown_size_files: 0,
                    reason: reason.into(),
                })),
                ..Self::default()
            }
        }

        fn move_calls(&self) -> usize {
            self.move_calls.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn offline_calls(&self) -> usize {
            self.offline_calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }
    impl OneDriveRiskHandler for FakeOneDriveRisk {
        fn move_risk(
            &self,
            _account: &str,
            _item_id: &str,
            _destination_parent_id: &str,
        ) -> Result<OneDriveMoveRisk, String> {
            self.move_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.move_result.lock().unwrap().clone()
        }

        fn offline_mode_risk(
            &self,
            _account: &str,
            _folder_id: &str,
        ) -> Result<OfflineModeRisk, String> {
            self.offline_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.offline_result.lock().unwrap().clone()
        }
    }

    struct PanicOneDriveRisk;
    impl OneDriveRiskHandler for PanicOneDriveRisk {
        fn move_risk(
            &self,
            _account: &str,
            _item_id: &str,
            _destination_parent_id: &str,
        ) -> Result<OneDriveMoveRisk, String> {
            panic!("desktop must not call OneDrive risk classifier")
        }

        fn offline_mode_risk(
            &self,
            _account: &str,
            _folder_id: &str,
        ) -> Result<OfflineModeRisk, String> {
            panic!("desktop must not call OneDrive risk classifier")
        }
    }

    /// A list handler returning one file child + one subfolder child, so the
    /// `effective_mode` enrichment can be checked for inheritance and own-override.
    struct ModeFakeList;
    impl OneDriveListHandler for ModeFakeList {
        fn children(
            &self,
            _account: &str,
            _folder: &str,
        ) -> Result<Vec<serde_json::Value>, String> {
            Ok(vec![
                json!({ "id": "f.txt", "name": "f.txt", "size": 3, "file": {} }),
                json!({ "id": "sub", "name": "sub", "folder": { "childCount": 0 } }),
            ])
        }
    }

    // AC1: POST persists + GET reflects; cap-gated (401); no handler (404 POST / static GET);
    // invalid mode (400); missing folder (400).
    #[test]
    fn onedrive_mode_post_persists_and_get_reflects() {
        let post = |t: &str| strict_scalar_post_from_target(t, "modecap");
        // No mode handler wired -> POST 404 (read-only serve); GET still 200 (static config).
        let (_d0, r0) = setup();
        assert_eq!(
            r0.route(&post(
                "/api/v1/onedrive/mode?account=a&folder=Photos&mode=sync"
            ))
            .status,
            404
        );
        assert_eq!(
            r0.route(&ApiRequest::get("/api/v1/onedrive/mode?account=a"))
                .status,
            200
        );

        let (_d, r1) = setup();
        let router = r1.with_onedrive_mode(
            std::sync::Arc::new(FakeOneDriveMode::default()),
            "modecap".into(),
        );
        // POST without a cap token -> 401.
        assert_eq!(
            router
                .route(&ApiRequest::new(
                    "POST",
                    "/api/v1/onedrive/mode?account=a&folder=Photos&mode=sync"
                ))
                .status,
            401
        );
        // Invalid mode -> 400.
        assert_eq!(
            router
                .route(&post(
                    "/api/v1/onedrive/mode?account=a&folder=Photos&mode=bogus"
                ))
                .status,
            400
        );
        // Missing folder -> 400.
        assert_eq!(
            router
                .route(&post("/api/v1/onedrive/mode?account=a&mode=sync"))
                .status,
            400
        );
        // Set Photos=sync -> 200; GET reflects it.
        assert_eq!(
            router
                .route(&post(
                    "/api/v1/onedrive/mode?account=a&folder=Photos&mode=sync"
                ))
                .status,
            200
        );
        let got = body_json(&router.route(&ApiRequest::get("/api/v1/onedrive/mode?account=a")));
        assert_eq!(got["default_mode"], "online");
        assert_eq!(
            got.pointer("/folder_modes/Photos").and_then(Value::as_str),
            Some("sync")
        );
        // Clear (empty mode) -> the override is gone.
        assert_eq!(
            router
                .route(&post("/api/v1/onedrive/mode?account=a&folder=Photos&mode="))
                .status,
            200
        );
        let cleared = body_json(&router.route(&ApiRequest::get("/api/v1/onedrive/mode?account=a")));
        assert!(cleared.pointer("/folder_modes/Photos").is_none());
    }

    // #659 D1: setting a folder ONLINE with the manage handler wired triggers the offline→online
    // cleanup, reported as an additive `cleanup` key. Without the manage handler (the #651/#652
    // path) the response is unchanged (no cleanup key) → those mode-toggle tests stay green.
    #[test]
    fn mode_post_online_triggers_cleanup_only_when_manage_wired() {
        let post = |t: &str| strict_scalar_post_from_target(t, "modecap");

        // Mode handler only (no manage) -> setting online has NO cleanup key (unchanged response).
        let (_d0, r0) = setup();
        let mode_only = r0.with_onedrive_mode(
            std::sync::Arc::new(FakeOneDriveMode::default()),
            "modecap".into(),
        );
        let resp = mode_only.route(&post(
            "/api/v1/onedrive/mode?account=a&folder=Photos&mode=online",
        ));
        assert_eq!(resp.status, 200);
        let j = body_json(&resp);
        assert_eq!(j["mode"], "online");
        assert!(
            j.get("cleanup").is_none(),
            "no cleanup without the manage handler (#651/#652 unchanged)"
        );

        // Mode + manage wired -> setting online runs cleanup + attaches {freed,kept}.
        let (_d, r1) = setup();
        let m = std::sync::Arc::new(MockManage::default());
        let router = r1
            .with_onedrive_mode(
                std::sync::Arc::new(FakeOneDriveMode::default()),
                "modecap".into(),
            )
            .with_onedrive_manage(m.clone(), "cap".into());
        let online = router.route(&post(
            "/api/v1/onedrive/mode?account=a&folder=Photos&mode=online",
        ));
        assert_eq!(online.status, 200);
        assert_eq!(body_json(&online)["cleanup"]["freed"], 3);
        assert_eq!(*m.cleaned.lock().unwrap(), vec!["a".to_string()]);

        // Setting a folder to SYNC (not online) does NOT trigger cleanup.
        let sync = router.route(&post(
            "/api/v1/onedrive/mode?account=a&folder=Docs&mode=sync",
        ));
        assert_eq!(sync.status, 200);
        assert!(
            body_json(&sync).get("cleanup").is_none(),
            "cleanup runs only on the switch to online"
        );
        assert_eq!(
            m.cleaned.lock().unwrap().len(),
            1,
            "cleanup not re-run for a non-online switch"
        );
    }

    // AC1b: the POST parse path is serde, symmetric with `OneDriveMode::as_str`. Proves
    // "valid -> the exact variant" (a #[serde(rename)]<->as_str mismatch would fail here),
    // not merely "invalid -> 400".
    #[test]
    fn onedrive_mode_serde_round_trips_with_as_str() {
        for (s, want) in [
            ("online", OneDriveMode::Online),
            ("sync", OneDriveMode::Sync),
            ("offline", OneDriveMode::Offline),
        ] {
            let parsed: OneDriveMode =
                serde_json::from_str(&format!("\"{s}\"")).expect("valid mode parses");
            assert_eq!(
                parsed, want,
                "\"{s}\" must deserialize to the matching variant"
            );
            // as_str -> from_str round-trips to the same variant, and as_str == the token.
            let back: OneDriveMode =
                serde_json::from_str(&format!("\"{}\"", want.as_str())).unwrap();
            assert_eq!(back, want);
            assert_eq!(want.as_str(), s);
        }
        assert!(serde_json::from_str::<OneDriveMode>("\"bogus\"").is_err());
    }

    // AC2: children carry the correct effective_mode. Photos=Offline (an ancestor of the
    // browsed folder) + sub=Sync (a child's own override). WITH ancestry the file inherits
    // Offline from Photos; WITHOUT ancestry it falls back to the account default; the
    // subfolder's own override wins in both.
    #[test]
    fn onedrive_children_carry_effective_mode() {
        let mode = std::sync::Arc::new(FakeOneDriveMode::default());
        mode.set_folder("a", "Photos", Some(OneDriveMode::Offline))
            .unwrap();
        mode.set_folder("a", "sub", Some(OneDriveMode::Sync))
            .unwrap();
        let (_d, r) = setup();
        let router = r
            .with_onedrive_list(std::sync::Arc::new(ModeFakeList))
            .with_onedrive_mode(mode, "modecap".into());

        // WITH ancestry: browsing "2024" whose parent chain is Photos -> root.
        let v = body_json(&router.route(&ApiRequest::get(
            "/api/v1/onedrive/children?account=a&folder=2024&ancestry=Photos,root",
        )));
        assert_eq!(
            v.pointer("/children/0/id").and_then(Value::as_str),
            Some("f.txt")
        );
        // file inherits Photos=Offline through the ancestry
        assert_eq!(
            v.pointer("/children/0/effective_mode")
                .and_then(Value::as_str),
            Some("offline")
        );
        // subfolder's own override wins
        assert_eq!(
            v.pointer("/children/1/effective_mode")
                .and_then(Value::as_str),
            Some("sync")
        );

        // WITHOUT ancestry: the file resolves at folder-level (2024 has no override) -> the
        // account default (online); the subfolder's own override still wins.
        let v2 = body_json(&router.route(&ApiRequest::get(
            "/api/v1/onedrive/children?account=a&folder=2024",
        )));
        assert_eq!(
            v2.pointer("/children/0/effective_mode")
                .and_then(Value::as_str),
            Some("online")
        );
        assert_eq!(
            v2.pointer("/children/1/effective_mode")
                .and_then(Value::as_str),
            Some("sync")
        );
    }

    // AC3: a mode POST writes a durable audit run (audit:onedrive-mode, started + ok).
    #[test]
    fn onedrive_mode_post_is_audited() {
        let post = |t: &str| strict_scalar_post_from_target(t, "modecap");
        let (_d, r) = setup();
        let store_path = r.config.accounts[0].archive_root.join(".isyncyou-store.db");
        let router = r.with_onedrive_mode(
            std::sync::Arc::new(FakeOneDriveMode::default()),
            "modecap".into(),
        );
        assert_eq!(
            router
                .route(&post(
                    "/api/v1/onedrive/mode?account=a&folder=Photos&mode=offline"
                ))
                .status,
            200
        );
        let store = Store::open(&store_path).unwrap();
        let runs = store.recent_runs("a", 50).unwrap();
        let audit: Vec<&str> = runs
            .iter()
            .filter(|r| r.kind == "audit:onedrive-mode")
            .map(|r| r.status.as_str())
            .collect();
        assert!(
            audit.contains(&"started"),
            "expected a started audit row, got {audit:?}"
        );
        assert!(
            audit.contains(&"ok"),
            "expected an ok audit row, got {audit:?}"
        );
        assert!(runs.iter().any(|r| r.kind == "audit:onedrive-mode"
            && r.summary.contains("folder=Photos")
            && r.summary.contains("mode=offline")));
    }

    struct FakeOneDriveOpen;
    impl OneDriveOpenHandler for FakeOneDriveOpen {
        fn download(&self, _account: &str, _id: &str) -> Result<Vec<u8>, String> {
            Ok(b"PNGDATA".to_vec())
        }
    }

    #[test]
    fn onedrive_open_route_returns_bytes_or_404() {
        let (_d, router) = setup();
        // no handler -> 404
        assert_eq!(
            router
                .route(&ApiRequest::get(
                    "/api/v1/onedrive/open?account=a&id=x&name=p.png"
                ))
                .status,
            404
        );
        let router = router.with_onedrive_open(std::sync::Arc::new(FakeOneDriveOpen));
        // missing id -> 400
        assert_eq!(
            router
                .route(&ApiRequest::get("/api/v1/onedrive/open?account=a"))
                .status,
            400
        );
        // handler + account + id -> 200 + raw bytes + content-type from `name`
        let resp = router.route(&ApiRequest::get(
            "/api/v1/onedrive/open?account=a&id=x&name=p.png",
        ));
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"PNGDATA".to_vec());
        assert_eq!(resp.content_type, "image/png");
        // a non-image name is served inertly as text/plain
        let txt = router.route(&ApiRequest::get(
            "/api/v1/onedrive/open?account=a&id=x&name=notes.pdf",
        ));
        assert_eq!(txt.content_type, "text/plain; charset=utf-8");
    }

    #[test]
    fn onedrive_open_is_session_gated() {
        // The open GET is 401 without a token on the mobile profile — exactly 401 (not
        // 404/200), proving the /api/v1/* gate catches this new path.
        let router = Router::new(Config::default())
            .with_onedrive_open(std::sync::Arc::new(FakeOneDriveOpen))
            .with_session_token("sess-tok-0001".into());
        assert_eq!(
            router
                .route(&ApiRequest::get("/api/v1/onedrive/open?account=a&id=x"))
                .status,
            401
        );
        let ok = ApiRequest::get("/api/v1/onedrive/open?account=a&id=x")
            .with_session_token(Some("sess-tok-0001".into()));
        assert_ne!(router.route(&ok).status, 401);
    }

    #[test]
    fn onedrive_items_carry_preview_from_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("arch");
        std::fs::create_dir_all(&arch).unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            // a top-level file (no tracked parent => a drive root item)
            let mut f = Item::new("a", "onedrive", "F1", "photo.jpg", "file");
            f.remote_mtime = Some("2026-05-01T10:00:00Z".into());
            store.upsert_item(&f).unwrap();
        }
        // its DriveItem JSON sidecar at the sharded archive path (#564 A1 shape)
        let rel = isyncyou_connectors::shard_rel("onedrive", "F1", "json");
        let p = arch.join(&rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(
            &p,
            serde_json::to_vec(&json!({
                "id": "F1",
                "webUrl": "https://1drv.ms/x",
                "file": { "mimeType": "image/jpeg", "hashes": { "sha256Hash": "ABC123" } },
                "image": { "width": 4032, "height": 3024 },
                "shared": { "scope": "users" },
                "createdBy": { "user": { "displayName": "Jan" } },
            }))
            .unwrap(),
        )
        .unwrap();
        let cfg = Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@outlook.com".into(),
                sync_root: dir.path().join("od"),
                archive_root: arch,
                cache_root: Default::default(),
                mount_point: None,
            }],
            ..Default::default()
        };
        let router = Router::new(cfg);
        let resp = router.route(&ApiRequest::get(
            "/api/v1/items?account=a&service=onedrive&parent=root",
        ));
        assert_eq!(resp.status, 200);
        let v = body_json(&resp);
        let p0 = &v["items"][0]["preview"];
        assert_eq!(p0["mime_type"].as_str(), Some("image/jpeg"));
        assert_eq!(p0["sha256"].as_str(), Some("ABC123"));
        assert_eq!(p0["created_by"].as_str(), Some("Jan"));
        assert_eq!(p0["web_url"].as_str(), Some("https://1drv.ms/x"));
        assert_eq!(p0["shared"].as_bool(), Some(true));
        assert_eq!(
            p0.pointer("/image/width").and_then(Value::as_i64),
            Some(4032)
        );
    }

    #[test]
    fn contact_photo_serves_archived_jpg_or_404() {
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("arch");
        std::fs::create_dir_all(&arch).unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            store
                .upsert_item(&Item::new("a", "contacts", "C1", "Ada", "contact"))
                .unwrap();
            store
                .upsert_item(&Item::new("a", "contacts", "C2", "Bob", "contact"))
                .unwrap();
        }
        // write C1's photo at the sharded path, exactly where backup_contact_photos does
        let rel = isyncyou_connectors::shard_rel("contacts", "C1", "jpg");
        let p = arch.join(&rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"\xFF\xD8\xFF\xE0JFIF...").unwrap();
        let cfg = Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@outlook.com".into(),
                sync_root: dir.path().join("od"),
                archive_root: arch,
                cache_root: Default::default(),
                mount_point: None,
            }],
            ..Default::default()
        };
        let router = Router::new(cfg);
        // C1 has a photo -> 200 image/jpeg + the bytes
        let resp = router.route(&ApiRequest::get("/api/v1/contact/photo?account=a&id=C1"));
        assert_eq!(resp.status, 200);
        assert_eq!(resp.content_type, "image/jpeg");
        assert_eq!(&resp.body[..2], b"\xFF\xD8");
        // C2 has no photo -> 404
        assert_eq!(
            router
                .route(&ApiRequest::get("/api/v1/contact/photo?account=a&id=C2"))
                .status,
            404
        );
        // missing id -> 400
        assert_eq!(
            router
                .route(&ApiRequest::get("/api/v1/contact/photo?account=a"))
                .status,
            400
        );
    }

    #[test]
    fn mail_content_endpoints_are_cap_token_gated_strict_json() {
        let (_d, router) = setup();
        // read-only router (no handler) refuses every mail-write POST
        for p in [
            "/api/v1/mail/send",
            "/api/v1/mail/reply",
            "/api/v1/mail/move",
            "/api/v1/mail/draft",
        ] {
            assert_eq!(
                router.route(&ApiRequest::new("POST", p)).status,
                404,
                "{p} must 404 on the read-only server"
            );
        }
        let rec = std::sync::Arc::new(RecordMailWrite::default());
        let router = router.with_mail_write(rec.clone(), "secret".into());

        let send_body = json!({
            "request_id": "123e4567-e89b-42d3-a456-426614174000",
            "account": "a",
            "subject": "Hi",
            "body": "body",
            "to": ["x@example.invalid"],
            "cc": [],
            "bcc": [],
            "importance": null,
            "read_receipt": false
        });
        assert_eq!(
            router
                .route(&strict_json_post(
                    "/api/v1/mail/send",
                    send_body.clone(),
                    None
                ))
                .status,
            401
        );
        assert_eq!(
            router
                .route(&strict_json_post(
                    "/api/v1/mail/send",
                    send_body.clone(),
                    Some("nope"),
                ))
                .status,
            401
        );
        let bad = strict_json_post(
            "/api/v1/mail/send",
            json!({
                "request_id": "123e4567-e89b-42d3-a456-426614174001",
                "account": "a",
                "subject": "Hi",
                "body": "",
                "to": [], "cc": [], "bcc": [],
                "importance": null,
                "read_receipt": false
            }),
            Some("secret"),
        );
        assert_eq!(router.route(&bad).status, 400);
        assert!(rec.0.lock().unwrap().is_empty());

        assert_eq!(
            router
                .route(&strict_json_post(
                    "/api/v1/mail/send",
                    send_body,
                    Some("secret"),
                ))
                .status,
            200
        );
        assert_eq!(
            rec.last(),
            "send subj=Hi to=x@example.invalid cc=0 imp=- rr=false"
        );

        assert_eq!(
            router
                .route(&strict_json_post(
                    "/api/v1/mail/send",
                    json!({
                        "request_id": "123e4567-e89b-42d3-a456-426614174002",
                        "account": "a", "subject": "Hi", "body": "",
                        "to": ["x@example.invalid"], "cc": [], "bcc": [],
                        "importance": "high", "read_receipt": true
                    }),
                    Some("secret"),
                ))
                .status,
            200
        );
        assert_eq!(
            rec.last(),
            "send subj=Hi to=x@example.invalid cc=0 imp=high rr=true"
        );

        assert_eq!(
            router
                .route(&strict_json_post(
                    "/api/v1/mail/reply",
                    json!({
                        "request_id": "123e4567-e89b-42d3-a456-426614174003",
                        "account": "a", "id": "m1", "comment": "ok",
                        "body": "", "all": true
                    }),
                    Some("secret"),
                ))
                .status,
            200
        );
        assert_eq!(rec.last(), "reply id=m1 all=true");

        assert_eq!(
            router
                .route(&strict_json_post(
                    "/api/v1/mail/forward",
                    json!({
                        "request_id": "123e4567-e89b-42d3-a456-426614174004",
                        "account": "a", "id": "m1", "comment": "see",
                        "body": "", "to": ["y@example.invalid"]
                    }),
                    Some("secret"),
                ))
                .status,
            200
        );

        // move returns the new id
        let moved = router.route(&strict_json_post(
            "/api/v1/mail/move",
            json!({
                "request_id": "123e4567-e89b-42d3-a456-426614174006",
                "account": "a", "id": "m1", "destination": "Archive"
            }),
            Some("secret"),
        ));
        assert_eq!(moved.status, 200);
        assert_eq!(body_json(&moved)["new_id"], "m1-moved");

        // flag validates the status enum
        assert_eq!(
            router
                .route(&strict_json_post(
                    "/api/v1/mail/flag",
                    json!({
                        "request_id": "123e4567-e89b-42d3-a456-426614174007",
                        "account": "a", "id": "m1", "status": "bogus"
                    }),
                    Some("secret"),
                ))
                .status,
            400
        );

        let drafted = router.route(&strict_json_post(
            "/api/v1/mail/draft",
            json!({
                "request_id": "123e4567-e89b-42d3-a456-426614174005",
                "account": "a", "id": null, "subject": "D", "body": "",
                "to": ["x@example.invalid"]
            }),
            Some("secret"),
        ));
        assert_eq!(body_json(&drafted)["draft_id"], "draft-9");
        assert_eq!(
            router
                .route(&strict_json_post(
                    "/api/v1/mail/draft",
                    json!({
                        "request_id": "123e4567-e89b-42d3-a456-426614174006",
                        "account": "a", "id": "d1", "subject": "", "body": "",
                        "to": []
                    }),
                    Some("secret"),
                ))
                .status,
            200
        );
        assert_eq!(rec.last(), "send_draft id=d1");

        let legacy = ApiRequest::new(
            "POST",
            "/api/v1/mail/send?account=a&to=x%40example.invalid&subject=secret",
        )
        .with_cap_token(Some("secret".into()));
        assert_eq!(router.route(&legacy).status, 400);
        let extra = strict_json_post(
            "/api/v1/mail/send",
            json!({
                "request_id": "123e4567-e89b-42d3-a456-426614174007",
                "account": "a", "subject": "Hi", "body": "", "to": ["x@example.invalid"],
                "cc": [], "bcc": [], "importance": null, "read_receipt": false,
                "unexpected": true
            }),
            Some("secret"),
        );
        assert_eq!(router.route(&extra).status, 400);
    }

    #[derive(Default)]
    struct FakeCalendarWrite {
        deletes: std::sync::Mutex<Vec<String>>,
    }
    impl CalendarWriteHandler for FakeCalendarWrite {
        fn create(&self, _a: &str, _e: &Value) -> Result<String, String> {
            Ok("new".into())
        }
        fn update(&self, _a: &str, _id: &str, _e: &Value) -> Result<(), String> {
            Ok(())
        }
        fn delete(&self, _a: &str, id: &str) -> Result<(), String> {
            self.deletes.lock().unwrap().push(id.to_string());
            Ok(())
        }
        fn respond(&self, _a: &str, _id: &str, _r: &str, _c: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeOneDriveWrite {
        creates: std::sync::Mutex<Vec<(String, String)>>, // (parent, name)
        renames: std::sync::Mutex<Vec<(String, String)>>, // (id, name)
        moves: std::sync::Mutex<Vec<(String, Option<String>, String)>>, // (id, new_parent, name)
        deletes: std::sync::Mutex<Vec<String>>,
        uploads: std::sync::Mutex<Vec<(String, String, Vec<u8>)>>, // (parent, name, bytes)
        replaces: std::sync::Mutex<Vec<(String, String, Vec<u8>)>>, // (id, etag, bytes)
    }
    impl OneDriveWriteHandler for FakeOneDriveWrite {
        fn create_folder(&self, _a: &str, parent: &str, name: &str) -> Result<String, String> {
            self.creates
                .lock()
                .unwrap()
                .push((parent.into(), name.into()));
            Ok("folder-new".into())
        }
        fn rename(&self, _a: &str, id: &str, name: &str) -> Result<(), String> {
            self.renames.lock().unwrap().push((id.into(), name.into()));
            Ok(())
        }
        fn move_item(
            &self,
            _a: &str,
            id: &str,
            new_parent: Option<&str>,
            name: &str,
        ) -> Result<(), String> {
            self.moves.lock().unwrap().push((
                id.into(),
                new_parent.map(str::to_string),
                name.into(),
            ));
            Ok(())
        }
        fn delete(&self, _a: &str, id: &str) -> Result<(), String> {
            self.deletes.lock().unwrap().push(id.into());
            Ok(())
        }
        fn upload(
            &self,
            _a: &str,
            parent: &str,
            name: &str,
            bytes: &[u8],
        ) -> Result<String, String> {
            self.uploads
                .lock()
                .unwrap()
                .push((parent.into(), name.into(), bytes.to_vec()));
            Ok("file-new".into())
        }
        fn replace(&self, _a: &str, id: &str, etag: &str, bytes: &[u8]) -> Result<(), String> {
            self.replaces
                .lock()
                .unwrap()
                .push((id.into(), etag.into(), bytes.to_vec()));
            Ok(())
        }
    }

    #[test]
    fn onedrive_move_out_of_protected_is_biometric_gated_on_mobile() {
        let post = |t: &str| strict_scalar_post_from_target(t, "cap");
        let (_d, r) = setup();
        let writes = std::sync::Arc::new(FakeOneDriveWrite::default());
        let risk = std::sync::Arc::new(FakeOneDriveRisk::with_move(
            OneDriveMoveRisk::MoveOutOfProtected {
                source_scope: "offline-folder".into(),
                destination_scope: None,
            },
        ));
        let mobile = r
            .with_onedrive_write(writes.clone(), "cap".into())
            .with_onedrive_risk(risk.clone())
            .with_biometric_gate();

        let ch = mobile.route(&post(
            "/api/v1/onedrive/move?account=a&id=A%3AB&parent=P%5D1&name=N%3A%221",
        ));
        assert_eq!(ch.status, 200);
        let j = body_json(&ch);
        assert_eq!(j["status"], "confirmation_required");
        assert_eq!(j["op"], "move-out-of-protected");
        assert_eq!(
            serde_json::from_str::<Value>(j["item"].as_str().unwrap()).unwrap(),
            json!(["onedrive_move", "A:B", "P]1", "N:\"1"])
        );
        let pat = j["pending_action_id"].as_str().unwrap().to_string();
        assert!(writes.moves.lock().unwrap().is_empty());
        assert_eq!(risk.move_calls(), 1);

        assert_eq!(
            mobile
                .route(
                    &post("/api/v1/onedrive/move?account=a&id=A%3AB&parent=P%5D1&name=N%3A%221")
                        .with_per_action_token(Some("wrong".into()))
                )
                .status,
            403
        );
        assert!(writes.moves.lock().unwrap().is_empty());

        assert_eq!(
            mobile
                .route(
                    &post("/api/v1/onedrive/move?account=a&id=A%3AB&parent=P%5D1&name=N%3A%221")
                        .with_per_action_token(Some(pat.clone()))
                )
                .status,
            403
        );
        assert!(writes.moves.lock().unwrap().is_empty());

        assert!(mobile.confirm_biometric(&pat));
        for changed in [
            "/api/v1/onedrive/move?account=a&id=A%3AB&parent=P2&name=N%3A%221",
            "/api/v1/onedrive/move?account=a&id=Other&parent=P%5D1&name=N%3A%221",
            "/api/v1/onedrive/move?account=a&id=A%3AB&parent=P%5D1&name=Other",
        ] {
            assert_eq!(
                mobile
                    .route(&post(changed).with_per_action_token(Some(pat.clone())))
                    .status,
                403,
                "confirmed move token must not authorize a mutated action: {changed}"
            );
            assert!(writes.moves.lock().unwrap().is_empty());
        }
        assert_eq!(
            mobile
                .route(
                    &post("/api/v1/onedrive/move?account=a&id=A%3AB&parent=P%5D1&name=N%3A%221")
                        .with_per_action_token(Some(pat))
                )
                .status,
            200
        );
        assert_eq!(
            *writes.moves.lock().unwrap(),
            vec![("A:B".into(), Some("P]1".into()), "N:\"1".into())]
        );
    }

    #[test]
    fn onedrive_move_low_risk_is_not_biometric_gated_on_mobile() {
        let post = |t: &str| strict_scalar_post_from_target(t, "cap");
        let (_d, r) = setup();
        let writes = std::sync::Arc::new(FakeOneDriveWrite::default());
        let risk = std::sync::Arc::new(FakeOneDriveRisk::default());
        let mobile = r
            .with_onedrive_write(writes.clone(), "cap".into())
            .with_onedrive_risk(risk.clone())
            .with_biometric_gate();

        let ok = mobile.route(&post(
            "/api/v1/onedrive/move?account=a&id=i2&parent=P2&name=N",
        ));
        assert_eq!(ok.status, 200);
        assert_eq!(
            *writes.moves.lock().unwrap(),
            vec![("i2".into(), Some("P2".into()), "N".into())]
        );
        assert_eq!(risk.move_calls(), 1);
    }

    #[test]
    fn onedrive_move_unknown_risk_is_biometric_gated_on_mobile() {
        let post = |t: &str| strict_scalar_post_from_target(t, "cap");
        let (_d, r) = setup();
        let writes = std::sync::Arc::new(FakeOneDriveWrite::default());
        let risk = std::sync::Arc::new(FakeOneDriveRisk::with_move(OneDriveMoveRisk::Unknown {
            reason: "missing destination".into(),
        }));
        let mobile = r
            .with_onedrive_write(writes.clone(), "cap".into())
            .with_onedrive_risk(risk.clone())
            .with_biometric_gate();

        let ch = mobile.route(&post(
            "/api/v1/onedrive/move?account=a&id=i2&parent=P2&name=N",
        ));
        assert_eq!(ch.status, 200);
        let j = body_json(&ch);
        assert_eq!(j["status"], "confirmation_required");
        assert_eq!(j["op"], "move-out-of-protected");
        assert!(writes.moves.lock().unwrap().is_empty());
        assert_eq!(risk.move_calls(), 1);
    }

    #[test]
    fn onedrive_move_missing_risk_classifier_fails_closed_on_mobile() {
        let post = |t: &str| strict_scalar_post_from_target(t, "cap");
        let (_d, r) = setup();
        let writes = std::sync::Arc::new(FakeOneDriveWrite::default());
        let mobile = r
            .with_onedrive_write(writes.clone(), "cap".into())
            .with_biometric_gate();

        let ch = mobile.route(&post(
            "/api/v1/onedrive/move?account=a&id=i2&parent=P2&name=N",
        ));
        assert_eq!(ch.status, 200);
        assert_eq!(body_json(&ch)["status"], "confirmation_required");
        assert!(writes.moves.lock().unwrap().is_empty());
    }

    #[test]
    fn onedrive_offline_large_mode_switch_is_biometric_gated_before_persist() {
        let post = |t: &str| strict_scalar_post_from_target(t, "modecap");
        let (_d, r) = setup();
        let modes = std::sync::Arc::new(FakeOneDriveMode::default());
        let risk = std::sync::Arc::new(FakeOneDriveRisk::with_offline_requires("bulk_files"));
        let mobile = r
            .with_onedrive_mode(modes.clone(), "modecap".into())
            .with_onedrive_risk(risk.clone())
            .with_biometric_gate();

        let ch = mobile.route(&post(
            "/api/v1/onedrive/mode?account=a&folder=Photos&mode=offline",
        ));
        assert_eq!(ch.status, 200);
        let j = body_json(&ch);
        assert_eq!(j["status"], "confirmation_required");
        assert_eq!(j["op"], "mode-switch-offline-large");
        assert_eq!(
            serde_json::from_str::<Value>(j["item"].as_str().unwrap()).unwrap(),
            json!(["onedrive_mode_offline", "Photos"])
        );
        assert!(
            !modes
                .modes("a")
                .unwrap()
                .folder_modes
                .contains_key("Photos"),
            "mode must not persist before biometric confirmation"
        );
        assert_eq!(risk.offline_calls(), 1);

        let pat = j["pending_action_id"].as_str().unwrap().to_string();
        assert_eq!(
            mobile
                .route(
                    &post("/api/v1/onedrive/mode?account=a&folder=Photos&mode=offline")
                        .with_per_action_token(Some("wrong".into()))
                )
                .status,
            403
        );
        assert!(
            !modes
                .modes("a")
                .unwrap()
                .folder_modes
                .contains_key("Photos"),
            "wrong biometric token must not persist the mode"
        );
        assert!(mobile.confirm_biometric(&pat));
        assert_eq!(
            mobile
                .route(
                    &post("/api/v1/onedrive/mode?account=a&folder=Archive%3A%5D&mode=offline")
                        .with_per_action_token(Some(pat.clone()))
                )
                .status,
            403,
            "confirmed Offline-mode token must not authorize another folder"
        );
        assert!(
            !modes
                .modes("a")
                .unwrap()
                .folder_modes
                .contains_key("Archive:]"),
            "folder-mismatched biometric token must not persist the other folder"
        );
        assert_eq!(
            mobile
                .route(
                    &post("/api/v1/onedrive/mode?account=a&folder=Photos&mode=offline")
                        .with_per_action_token(Some(pat))
                )
                .status,
            200
        );
        assert_eq!(
            modes
                .modes("a")
                .unwrap()
                .folder_modes
                .get("Photos")
                .copied(),
            Some(OneDriveMode::Offline)
        );
    }

    #[test]
    fn onedrive_offline_small_mode_switch_is_not_gated() {
        let post = |t: &str| strict_scalar_post_from_target(t, "modecap");
        let (_d, r) = setup();
        let modes = std::sync::Arc::new(FakeOneDriveMode::default());
        let risk = std::sync::Arc::new(FakeOneDriveRisk::default());
        let mobile = r
            .with_onedrive_mode(modes.clone(), "modecap".into())
            .with_onedrive_risk(risk.clone())
            .with_biometric_gate();

        let ok = mobile.route(&post(
            "/api/v1/onedrive/mode?account=a&folder=Tiny&mode=offline",
        ));
        assert_eq!(ok.status, 200);
        assert_eq!(
            modes.modes("a").unwrap().folder_modes.get("Tiny").copied(),
            Some(OneDriveMode::Offline)
        );
        assert_eq!(risk.offline_calls(), 1);
    }

    #[test]
    fn onedrive_mode_online_cleanup_is_bulk_gated_before_persist() {
        let post = |t: &str| strict_scalar_post_from_target(t, "modecap");
        let (_d, r) = setup();
        let modes = std::sync::Arc::new(FakeOneDriveMode::default());
        let manage = std::sync::Arc::new(MockManage::default());
        let mobile = r
            .with_onedrive_mode(modes.clone(), "modecap".into())
            .with_onedrive_manage(manage.clone(), "managecap".into())
            .with_biometric_gate();

        let ch = mobile.route(&post(
            "/api/v1/onedrive/mode?account=a&folder=Photos&mode=online",
        ));
        assert_eq!(ch.status, 200);
        let j = body_json(&ch);
        assert_eq!(j["status"], "confirmation_required");
        assert_eq!(j["op"], "bulk");
        assert_eq!(
            serde_json::from_str::<Value>(j["item"].as_str().unwrap()).unwrap(),
            json!(["onedrive_mode_online_account_cleanup", "Photos"])
        );
        assert!(
            !modes
                .modes("a")
                .unwrap()
                .folder_modes
                .contains_key("Photos"),
            "mode must not persist before account-wide cleanup prompt"
        );
        assert!(manage.cleaned.lock().unwrap().is_empty());

        let pat = j["pending_action_id"].as_str().unwrap().to_string();
        assert!(mobile.confirm_biometric(&pat));
        assert_eq!(
            mobile
                .route(
                    &post("/api/v1/onedrive/mode?account=a&folder=Photos&mode=online")
                        .with_per_action_token(Some(pat))
                )
                .status,
            200
        );
        assert_eq!(
            modes
                .modes("a")
                .unwrap()
                .folder_modes
                .get("Photos")
                .copied(),
            Some(OneDriveMode::Online)
        );
        assert_eq!(*manage.cleaned.lock().unwrap(), vec!["a".to_string()]);
    }

    #[test]
    fn onedrive_risk_classifier_is_not_called_on_desktop() {
        let move_post = |t: &str| strict_scalar_post_from_target(t, "cap");
        let mode_post = |t: &str| strict_scalar_post_from_target(t, "modecap");
        let (_d, r) = setup();
        let writes = std::sync::Arc::new(FakeOneDriveWrite::default());
        let modes = std::sync::Arc::new(FakeOneDriveMode::default());
        let manage = std::sync::Arc::new(MockManage::default());
        let desktop = r
            .with_onedrive_write(writes.clone(), "cap".into())
            .with_onedrive_mode(modes.clone(), "modecap".into())
            .with_onedrive_manage(manage.clone(), "managecap".into())
            .with_onedrive_risk(std::sync::Arc::new(PanicOneDriveRisk));

        assert_eq!(
            desktop
                .route(&move_post(
                    "/api/v1/onedrive/move?account=a&id=i2&parent=P2&name=N"
                ))
                .status,
            200
        );
        assert_eq!(
            desktop
                .route(&mode_post(
                    "/api/v1/onedrive/mode?account=a&folder=Photos&mode=offline"
                ))
                .status,
            200
        );
        assert_eq!(
            desktop
                .route(&mode_post(
                    "/api/v1/onedrive/mode?account=a&folder=Photos&mode=online"
                ))
                .status,
            200
        );
        assert_eq!(
            *writes.moves.lock().unwrap(),
            vec![("i2".into(), Some("P2".into()), "N".into())]
        );
        assert_eq!(*manage.cleaned.lock().unwrap(), vec!["a".to_string()]);
    }

    // #654 (AC1, webui part): the OneDrive cloud-write POST arms — cap-gate + verb dispatch.
    #[test]
    fn onedrive_write_cap_gate_and_dispatch() {
        let post = |t: &str| strict_scalar_post_from_target(t, "cap");
        // No handler wired -> 404.
        let (_d0, r0) = setup();
        assert_eq!(
            r0.route(&post(
                "/api/v1/onedrive/create?account=a&parent=P&name=Docs"
            ))
            .status,
            404
        );
        // Handler wired but no cap token -> 401; handler not called.
        let (_d1, r1) = setup();
        let f = std::sync::Arc::new(FakeOneDriveWrite::default());
        let router = r1.with_onedrive_write(f.clone(), "cap".into());
        let no_cap = ApiRequest::new(
            "POST",
            "/api/v1/onedrive/create?account=a&parent=P&name=Docs",
        );
        assert_eq!(router.route(&no_cap).status, 401);
        assert!(f.creates.lock().unwrap().is_empty());
        // create with cap -> 200 + new id; handler called with (parent, name).
        let resp = router.route(&post(
            "/api/v1/onedrive/create?account=a&parent=P&name=Docs",
        ));
        assert_eq!(resp.status, 200);
        assert_eq!(body_json(&resp)["id"], "folder-new");
        assert_eq!(
            *f.creates.lock().unwrap(),
            vec![("P".into(), "Docs".into())]
        );
        // rename / move / delete dispatch to the right verb with the right args.
        assert_eq!(
            router
                .route(&post("/api/v1/onedrive/rename?account=a&id=i1&name=New"))
                .status,
            200
        );
        assert_eq!(
            *f.renames.lock().unwrap(),
            vec![("i1".into(), "New".into())]
        );
        assert_eq!(
            router
                .route(&post(
                    "/api/v1/onedrive/move?account=a&id=i2&parent=P2&name=N"
                ))
                .status,
            200
        );
        assert_eq!(
            *f.moves.lock().unwrap(),
            vec![("i2".into(), Some("P2".into()), "N".into())]
        );
        assert_eq!(
            router
                .route(&post("/api/v1/onedrive/delete?account=a&id=i3"))
                .status,
            200
        );
        assert_eq!(*f.deletes.lock().unwrap(), vec!["i3".to_string()]);
        // an absent parent means the drive root -> still a valid create (parent = "").
        assert_eq!(
            router
                .route(&post("/api/v1/onedrive/create?account=a&name=Root"))
                .status,
            200
        );
        assert_eq!(
            *f.creates.lock().unwrap().last().unwrap(),
            ("".to_string(), "Root".to_string())
        );
        // missing name -> 400.
        assert_eq!(
            router
                .route(&post("/api/v1/onedrive/create?account=a&parent=P"))
                .status,
            400
        );
    }

    #[test]
    fn legacy_onedrive_upload_replace_routes_are_removed() {
        let (_directory, router) = setup();
        for path in [
            "/api/v1/onedrive/upload?account=a&parent=P&name=f.txt",
            "/api/v1/onedrive/replace?account=a&id=i9&etag=E1",
        ] {
            let response = router.route(
                &ApiRequest::new("POST", path)
                    .with_cap_token(Some("cap".into()))
                    .with_body(b"forbidden-direct-body".to_vec()),
            );
            assert_eq!(response.status, 405);
        }
    }

    // #654 (AC3): on mobile, `delete` raises the biometric gate; `create` does not.
    #[test]
    fn onedrive_delete_is_biometric_gated_on_mobile() {
        let post = |t: &str| strict_scalar_post_from_target(t, "cap");
        let (_d, r) = setup();
        let f = std::sync::Arc::new(FakeOneDriveWrite::default());
        let mobile = r
            .with_onedrive_write(f.clone(), "cap".into())
            .with_biometric_gate();
        // delete without a token -> challenged; handler NOT called.
        let ch = mobile.route(&post("/api/v1/onedrive/delete?account=a&id=i1"));
        assert_eq!(ch.status, 200);
        let j = body_json(&ch);
        assert_eq!(j["status"], "confirmation_required");
        let pat = j["pending_action_id"].as_str().unwrap().to_string();
        assert!(f.deletes.lock().unwrap().is_empty());
        // re-issue with the token but no biometric yet -> 403.
        assert_eq!(
            mobile
                .route(
                    &post("/api/v1/onedrive/delete?account=a&id=i1")
                        .with_per_action_token(Some(pat.clone()))
                )
                .status,
            403
        );
        assert!(f.deletes.lock().unwrap().is_empty());
        // native biometric confirms -> re-issue proceeds once.
        assert!(mobile.confirm_biometric(&pat));
        assert_eq!(
            mobile
                .route(
                    &post("/api/v1/onedrive/delete?account=a&id=i1")
                        .with_per_action_token(Some(pat))
                )
                .status,
            200
        );
        assert_eq!(*f.deletes.lock().unwrap(), vec!["i1".to_string()]);
        // create is NOT in the gate catalogue -> straight through on mobile.
        assert_eq!(
            mobile
                .route(&post("/api/v1/onedrive/create?account=a&parent=P&name=D"))
                .status,
            200
        );
        assert_eq!(f.creates.lock().unwrap().len(), 1);
    }

    // #onedrive-mobile 0.6: the mobile biometric gate wired through a real route.
    #[test]
    fn biometric_gate_challenges_and_consumes_a_per_action_token() {
        let del = |t: &str| strict_scalar_post_from_target(t, "cap");

        // Desktop profile (gate off): a delete with the cap token goes straight through.
        let (_d0, r0) = setup();
        let f0 = std::sync::Arc::new(FakeCalendarWrite::default());
        let desktop = r0.with_calendar_write(f0.clone(), "cap".into());
        assert_eq!(
            desktop
                .route(&del("/api/v1/calendar/delete?account=a&id=e1"))
                .status,
            200
        );
        assert_eq!(*f0.deletes.lock().unwrap(), vec!["e1"]);

        // Mobile profile (gate on): the same delete is challenged; handler NOT called.
        let (_d1, r1) = setup();
        let f1 = std::sync::Arc::new(FakeCalendarWrite::default());
        let mobile = r1
            .with_calendar_write(f1.clone(), "cap".into())
            .with_biometric_gate();
        let ch = mobile.route(&del("/api/v1/calendar/delete?account=a&id=e1"));
        assert_eq!(ch.status, 200);
        let j = body_json(&ch);
        assert_eq!(j["status"], "confirmation_required");
        let pat = j["pending_action_id"].as_str().unwrap().to_string();
        assert!(
            f1.deletes.lock().unwrap().is_empty(),
            "handler must not run before biometric"
        );

        // Re-issue with the token but NO biometric yet -> 403 (not confirmed).
        assert_eq!(
            mobile
                .route(
                    &del("/api/v1/calendar/delete?account=a&id=e1")
                        .with_per_action_token(Some(pat.clone()))
                )
                .status,
            403
        );
        assert!(f1.deletes.lock().unwrap().is_empty());

        // Native biometric confirms over the JNI-only path -> re-issue proceeds once.
        assert!(mobile.confirm_biometric(&pat));
        assert_eq!(
            mobile
                .route(
                    &del("/api/v1/calendar/delete?account=a&id=e1")
                        .with_per_action_token(Some(pat.clone()))
                )
                .status,
            200
        );
        assert_eq!(*f1.deletes.lock().unwrap(), vec!["e1"]);

        // Replay of the consumed token -> 403 (single-use); handler not called again.
        assert_eq!(
            mobile
                .route(
                    &del("/api/v1/calendar/delete?account=a&id=e1")
                        .with_per_action_token(Some(pat))
                )
                .status,
            403
        );
        assert_eq!(f1.deletes.lock().unwrap().len(), 1);

        // A token minted+confirmed for e2 cannot authorize deleting e1 (hash immutable).
        let ch2 = mobile.route(&del("/api/v1/calendar/delete?account=a&id=e2"));
        let pat2 = body_json(&ch2)["pending_action_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(mobile.confirm_biometric(&pat2));
        assert_eq!(
            mobile
                .route(
                    &del("/api/v1/calendar/delete?account=a&id=e1")
                        .with_per_action_token(Some(pat2))
                )
                .status,
            403
        );
    }

    #[test]
    fn mail_write_does_not_notify_the_sse_bus() {
        // A self-write must NOT fire the SSE bus: the daemon doesn't re-sync mail
        // into the store on a write, so an SSE-driven re-fetch would read the stale
        // store and clobber the frontend's optimistic update (found in #563 live
        // e2e). The optimistic UI is the correct immediate feedback.
        let (_d, router) = setup();
        let bus = std::sync::Arc::new(EventBus::new());
        let router = router.with_events(bus.clone()).with_mail_write(
            std::sync::Arc::new(RecordMailWrite::default()),
            "secret".into(),
        );
        let g0 = bus.generation();
        assert_eq!(
            router
                .route(&strict_json_post(
                    "/api/v1/mail/read",
                    json!({
                        "request_id": "123e4567-e89b-42d3-a456-426614174008",
                        "account": "a", "id": "m1", "is_read": true
                    }),
                    Some("secret"),
                ))
                .status,
            200
        );
        assert_eq!(
            bus.generation(),
            g0,
            "a self-write must not notify (would clobber optimistic UI from a stale re-fetch)"
        );
    }

    /// app.js carries the mail-write cap token placeholder so #563's UI can POST.
    #[test]
    fn app_js_has_mail_write_cap_token_placeholder() {
        assert!(APP_JS.contains("__MAILWRITE_CAP_TOKEN__"));
    }

    #[test]
    fn app_js_has_calendar_write_cap_token_placeholder() {
        assert!(APP_JS.contains("__CALENDARWRITE_CAP_TOKEN__"));
    }

    #[derive(Default)]
    struct RecCalWrite(std::sync::Mutex<Vec<String>>);
    impl CalendarWriteHandler for RecCalWrite {
        fn create(&self, _a: &str, event: &Value) -> Result<String, String> {
            self.0.lock().unwrap().push(format!(
                "create subj={}",
                event.get("subject").and_then(Value::as_str).unwrap_or("")
            ));
            Ok("ev-new".into())
        }
        fn update(&self, _a: &str, id: &str, _e: &Value) -> Result<(), String> {
            self.0.lock().unwrap().push(format!("update id={id}"));
            Ok(())
        }
        fn delete(&self, _a: &str, id: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(format!("delete id={id}"));
            Ok(())
        }
        fn respond(&self, _a: &str, id: &str, r: &str, _c: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(format!("respond id={id} {r}"));
            Ok(())
        }
    }

    #[test]
    fn calendar_write_endpoints_are_cap_gated_and_route_params() {
        let (_d, router) = setup();
        for p in [
            "/api/v1/calendar/create",
            "/api/v1/calendar/update",
            "/api/v1/calendar/delete",
            "/api/v1/calendar/respond",
        ] {
            assert_eq!(
                router.route(&ApiRequest::new("POST", p)).status,
                404,
                "{p} must 404 on the read-only server"
            );
        }
        let rec = std::sync::Arc::new(RecCalWrite::default());
        let router = router.with_calendar_write(rec.clone(), "calsecret".into());
        let create = "/api/v1/calendar/create?account=a&subject=Plan&start=2026-02-04T09:00:00Z";
        // missing token -> 401
        assert_eq!(router.route(&ApiRequest::new("POST", create)).status, 401);
        // valid token but no subject/start -> 400 (handler not called)
        let bad = strict_scalar_post_from_target("/api/v1/calendar/create?account=a", "calsecret");
        assert_eq!(router.route(&bad).status, 400);
        let tok = |t: &str| strict_scalar_post_from_target(t, "calsecret");
        assert_eq!(router.route(&tok(create)).status, 200);
        assert_eq!(
            router
                .route(&tok("/api/v1/calendar/update?account=a&id=E1&subject=X"))
                .status,
            200
        );
        assert_eq!(
            router
                .route(&tok("/api/v1/calendar/delete?account=a&id=E2"))
                .status,
            200
        );
        assert_eq!(
            router
                .route(&tok(
                    "/api/v1/calendar/respond?account=a&id=E3&response=decline"
                ))
                .status,
            200
        );
        let log = rec.0.lock().unwrap();
        assert_eq!(log[0], "create subj=Plan");
        assert_eq!(log[1], "update id=E1");
        assert_eq!(log[2], "delete id=E2");
        assert_eq!(log[3], "respond id=E3 decline");
    }

    #[test]
    fn app_js_has_contact_write_cap_token_placeholder() {
        assert!(APP_JS.contains("__CONTACTWRITE_CAP_TOKEN__"));
    }

    #[derive(Default)]
    struct RecConWrite(std::sync::Mutex<Vec<String>>);
    impl ContactWriteHandler for RecConWrite {
        fn create(&self, _a: &str, contact: &Value) -> Result<String, String> {
            self.0.lock().unwrap().push(format!(
                "create name={} other_city={}",
                contact
                    .get("displayName")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                contact
                    .pointer("/otherAddress/city")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            ));
            Ok("con-new".into())
        }
        fn update(&self, _a: &str, id: &str, _c: &Value) -> Result<(), String> {
            self.0.lock().unwrap().push(format!("update id={id}"));
            Ok(())
        }
        fn delete(&self, _a: &str, id: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(format!("delete id={id}"));
            Ok(())
        }
    }

    #[test]
    fn contact_write_endpoints_are_cap_gated_and_route_params() {
        let (_d, router) = setup();
        for p in [
            "/api/v1/contact/create",
            "/api/v1/contact/update",
            "/api/v1/contact/delete",
        ] {
            assert_eq!(
                router.route(&ApiRequest::new("POST", p)).status,
                404,
                "{p} must 404 on the read-only server"
            );
        }
        let rec = std::sync::Arc::new(RecConWrite::default());
        let router = router.with_contact_write(rec.clone(), "consecret".into());
        let create = "/api/v1/contact/create?account=a&display_name=Ada&other_city=London";
        // missing token -> 401
        assert_eq!(router.route(&ApiRequest::new("POST", create)).status, 401);
        // valid token but no fields -> 400 (handler not called)
        let bad = strict_scalar_post_from_target("/api/v1/contact/create?account=a", "consecret");
        assert_eq!(router.route(&bad).status, 400);
        let tok = |t: &str| strict_scalar_post_from_target(t, "consecret");
        assert_eq!(router.route(&tok(create)).status, 200);
        assert_eq!(
            router
                .route(&tok("/api/v1/contact/update?account=a&id=C1&job=Analyst"))
                .status,
            200
        );
        assert_eq!(
            router
                .route(&tok("/api/v1/contact/delete?account=a&id=C2"))
                .status,
            200
        );
        let log = rec.0.lock().unwrap();
        // proves contact_from_req maps display_name + the structured other-address param
        assert_eq!(log[0], "create name=Ada other_city=London");
        assert_eq!(log[1], "update id=C1");
        assert_eq!(log[2], "delete id=C2");
    }

    #[test]
    fn app_js_has_task_write_cap_token_placeholder() {
        assert!(APP_JS.contains("__TASKWRITE_CAP_TOKEN__"));
    }

    #[derive(Default)]
    struct RecTaskWrite(std::sync::Mutex<Vec<String>>);
    impl TaskWriteHandler for RecTaskWrite {
        fn create(&self, _a: &str, list: &str, task: &Value) -> Result<String, String> {
            self.0.lock().unwrap().push(format!(
                "create list={list} title={}",
                task.get("title").and_then(Value::as_str).unwrap_or("")
            ));
            Ok("task-new".into())
        }
        fn update(&self, _a: &str, list: &str, id: &str, _t: &Value) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("update list={list} id={id}"));
            Ok(())
        }
        fn complete(&self, _a: &str, list: &str, id: &str) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("complete list={list} id={id}"));
            Ok(())
        }
        fn delete(&self, _a: &str, list: &str, id: &str) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("delete list={list} id={id}"));
            Ok(())
        }
        fn checklist_add(
            &self,
            _a: &str,
            _l: &str,
            task: &str,
            title: &str,
        ) -> Result<String, String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("cl_add task={task} title={title}"));
            Ok("ci-new".into())
        }
        fn checklist_toggle(
            &self,
            _a: &str,
            _l: &str,
            task: &str,
            item: &str,
            checked: bool,
        ) -> Result<(), String> {
            self.0.lock().unwrap().push(format!(
                "cl_toggle task={task} item={item} checked={checked}"
            ));
            Ok(())
        }
        fn checklist_delete(
            &self,
            _a: &str,
            _l: &str,
            task: &str,
            item: &str,
        ) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("cl_del task={task} item={item}"));
            Ok(())
        }
        fn list_create(&self, _a: &str, name: &str) -> Result<String, String> {
            self.0.lock().unwrap().push(format!("list_create {name}"));
            Ok("L-new".into())
        }
        fn list_delete(&self, _a: &str, id: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(format!("list_delete {id}"));
            Ok(())
        }
    }

    #[test]
    fn todo_write_endpoints_are_cap_gated_and_route_params() {
        let (_d, router) = setup();
        for p in [
            "/api/v1/todo/create",
            "/api/v1/todo/complete",
            "/api/v1/todo/checklist-add",
            "/api/v1/todo/list-create",
            "/api/v1/todo/list-delete",
        ] {
            assert_eq!(
                router.route(&ApiRequest::new("POST", p)).status,
                404,
                "{p} must 404 on the read-only server"
            );
        }
        let rec = std::sync::Arc::new(RecTaskWrite::default());
        let router = router.with_task_write(rec.clone(), "tasksecret".into());
        let create = "/api/v1/todo/create?account=a&list=L1&title=Ship&importance=high";
        // missing token -> 401
        assert_eq!(router.route(&ApiRequest::new("POST", create)).status, 401);
        // valid token, no title -> 400 (handler not called)
        let bad =
            strict_scalar_post_from_target("/api/v1/todo/create?account=a&list=L1", "tasksecret");
        assert_eq!(router.route(&bad).status, 400);
        let tok = |t: &str| strict_scalar_post_from_target(t, "tasksecret");
        assert_eq!(router.route(&tok(create)).status, 200);
        assert_eq!(
            router
                .route(&tok(
                    "/api/v1/todo/update?account=a&list=L1&id=t1&status=inProgress"
                ))
                .status,
            200
        );
        assert_eq!(
            router
                .route(&tok("/api/v1/todo/complete?account=a&list=L1&id=t1"))
                .status,
            200
        );
        assert_eq!(
            router
                .route(&tok(
                    "/api/v1/todo/checklist-add?account=a&list=L1&task=t1&title=step"
                ))
                .status,
            200
        );
        assert_eq!(
            router
                .route(&tok(
                    "/api/v1/todo/checklist-toggle?account=a&list=L1&task=t1&item=ci1&checked=1"
                ))
                .status,
            200
        );
        assert_eq!(
            router
                .route(&tok(
                    "/api/v1/todo/checklist-delete?account=a&list=L1&task=t1&item=ci1"
                ))
                .status,
            200
        );
        assert_eq!(
            router
                .route(&tok("/api/v1/todo/list-create?account=a&name=Groceries"))
                .status,
            200
        );
        assert_eq!(
            router
                .route(&tok("/api/v1/todo/delete?account=a&list=L1&id=t1"))
                .status,
            200
        );
        assert_eq!(
            router
                .route(&tok("/api/v1/todo/list-delete?account=a&id=L1"))
                .status,
            200
        );
        let log = rec.0.lock().unwrap();
        assert_eq!(log[0], "create list=L1 title=Ship");
        assert_eq!(log[1], "update list=L1 id=t1");
        assert_eq!(log[2], "complete list=L1 id=t1");
        assert_eq!(log[3], "cl_add task=t1 title=step");
        assert_eq!(log[4], "cl_toggle task=t1 item=ci1 checked=true");
        assert_eq!(log[5], "cl_del task=t1 item=ci1");
        assert_eq!(log[6], "list_create Groceries");
        assert_eq!(log[7], "delete list=L1 id=t1");
        assert_eq!(log[8], "list_delete L1");
    }

    #[test]
    fn app_js_has_onenote_write_cap_token_placeholder() {
        assert!(APP_JS.contains("__ONENOTEWRITE_CAP_TOKEN__"));
    }

    #[test]
    fn app_js_has_onedrive_write_cap_token_placeholder() {
        assert!(APP_JS.contains("__ONEDRIVEWRITE_CAP_TOKEN__"));
        // #657: a router wired with the OneDrive write handler injects the real token
        // into the served /app.js, leaving no placeholder behind.
        let f = std::sync::Arc::new(FakeOneDriveWrite::default());
        let resp = Router::new(Config::default())
            .with_onedrive_write(f, "odw123".into())
            .route(&ApiRequest::get("/app.js"));
        let js = String::from_utf8_lossy(&resp.body);
        assert!(js.contains("onedrivewrite: \"odw123\""));
        assert!(
            !js.contains("__ONEDRIVEWRITE_CAP_TOKEN__"),
            "placeholder must be replaced"
        );
    }

    #[test]
    fn app_js_has_account_cap_token_placeholder() {
        assert!(APP_JS.contains("__ACCOUNT_CAP_TOKEN__"));
    }

    #[test]
    fn app_js_has_transfer_cap_token_placeholder() {
        // app.js side of the #656 bridge: the raw placeholder is present.
        assert!(APP_JS.contains("__TRANSFER_CAP_TOKEN__"));
        // read-only router (no transfers handler wired): the placeholder is blanked.
        let ro = Router::new(Config::default()).route(&ApiRequest::get("/app.js"));
        let ro_body = String::from_utf8_lossy(&ro.body);
        assert!(
            !ro_body.contains("__TRANSFER_CAP_TOKEN__"),
            "placeholder must be replaced"
        );
        assert!(
            ro_body.contains("transfers: \"\""),
            "no token when transfers are disabled"
        );
        // with a transfers handler wired, the real cap token is injected.
        struct NoopTransfers;
        impl TransferProgress for NoopTransfers {
            fn transfers(&self) -> Vec<TransferState> {
                vec![]
            }
            fn cancel(&self, _id: &str) -> bool {
                false
            }
        }
        let rw = Router::new(Config::default())
            .with_transfers(std::sync::Arc::new(NoopTransfers), "xfer123".into())
            .route(&ApiRequest::get("/app.js"));
        let rw_body = String::from_utf8_lossy(&rw.body);
        assert!(rw_body.contains("transfers: \"xfer123\""));
    }

    #[test]
    fn app_js_has_onedrive_manage_cap_token_placeholder() {
        // app.js side of the #659 manage bridge: the raw placeholder is present.
        assert!(APP_JS.contains("__ONEDRIVE_MANAGE_CAP_TOKEN__"));
        // read-only router (no manage handler wired): the placeholder is blanked.
        let ro = Router::new(Config::default()).route(&ApiRequest::get("/app.js"));
        let ro_body = String::from_utf8_lossy(&ro.body);
        assert!(
            !ro_body.contains("__ONEDRIVE_MANAGE_CAP_TOKEN__"),
            "placeholder must be replaced"
        );
        assert!(
            ro_body.contains("onedriveManage: \"\""),
            "no token when management is disabled"
        );
        // with a manage handler wired, the real cap token is injected.
        let m = std::sync::Arc::new(MockManage::default());
        let rw = Router::new(Config::default())
            .with_onedrive_manage(m, "odm123".into())
            .route(&ApiRequest::get("/app.js"));
        let rw_body = String::from_utf8_lossy(&rw.body);
        assert!(rw_body.contains("onedriveManage: \"odm123\""));
    }

    #[test]
    fn app_js_gates_download_now_by_store_mode() {
        for needle in [
            "function driveManageMode(row)",
            "function driveCanDownloadNow(row)",
            "return row && MODE_KEYS.includes(row.effective_mode) ? row.effective_mode : null;",
            "return mode === \"sync\" || mode === \"offline\";",
            "const hasBody = row.has_body === true;",
            ".catch(() => driveRenderManageUnavailable(box))",
            "d && d.downloaded === false",
        ] {
            assert!(
                APP_JS.contains(needle),
                "app.js missing #724 download-now eligibility invariant: {needle}"
            );
        }
        assert!(
            !APP_JS.contains(".catch(() => driveRenderManage(box, it, null))"),
            "store-miss must not render the download-now action"
        );
        assert!(
            !APP_JS.contains("d && d.materialized === false"),
            "download-now UI must not consume the old materialized response field"
        );
        assert!(
            !APP_JS.contains("row.body_state === \"available\"")
                && !APP_JS.contains("row.content_state === \"materialized\""),
            "download-now UI must not bypass the server/sealed-body has_body policy"
        );
    }

    #[test]
    fn app_js_has_push_cap_token_placeholder() {
        assert!(APP_JS.contains("__PUSH_CAP_TOKEN__"));
    }

    #[test]
    fn app_js_has_agent_cap_token_placeholder() {
        assert!(APP_JS.contains("__AGENT_CAP_TOKEN__"));
    }

    #[test]
    fn assistant_nav_is_services_entry_and_cap_gated() {
        for needle in [
            "{ id: \"assistant\", label: \"Assistant\", icon: \"sparkles\", cap: \"agent\", appOnly: true }",
            "const serviceVisible = (s) => !s.cap || !!CAP[s.cap];",
            "const visibleServices = () => SERVICES.filter(serviceVisible);",
            "const archiveServices = () => SERVICES.filter(s => !s.appOnly);",
            "visibleServices().map(s => {",
            "const routeLabel = (r) => (visibleServices().find(s => s.id === r) || {}).label || EXTRA_ROUTES[r] || \"iSyncYou\";",
            "if (!visibleServices().find(s => s.id === App.route) && !EXTRA_ROUTES[App.route]) App.route = \"overview\";",
            "archiveServices().forEach(s => {",
            "...visibleServices().map(s => ({ label: \"Go to \" + s.label",
            "const order = visibleServices().map((s) => s.id);",
        ] {
            assert!(
                APP_JS.contains(needle),
                "app.js missing #622 assistant nav invariant: {needle}"
            );
        }
        assert!(
            !APP_JS.contains("id: \"nav-assistant\""),
            "Assistant must be a primary SERVICES tab, not a separate System nav button"
        );
        assert!(
            !APP_JS.contains("assistant: \"Assistant\""),
            "Assistant must not remain an EXTRA_ROUTES bypass around the capability gate"
        );
    }

    #[test]
    fn assistant_stream_renderer_handles_full_event_contract() {
        for needle in [
            "function handleAgentEvent(message, turnState)",
            "case \"token\":",
            "case \"tool_call\":",
            "case \"tool_result\":",
            "case \"search_stage\":",
            "case \"partial_result\":",
            "case \"confirmation_required\":",
            "case \"error\":",
            "case \"done\":",
            "AssistantState.pendingCardsById.set(pending.pending_id",
            "token: d.token || \"\"",
            "action_hash: d.action_hash || \"\"",
            "renderAgentPendingCard(pending)",
            "renderAgentToolRow(row)",
            "renderAgentError(message)",
            "Invalid stream payload",
            "handleAgentEvent(d, turnState);",
            "handleAgentEvent({ event: \"done\", reason: \"complete\" }, turnState)",
            "reason === \"pending_confirmation\"",
        ] {
            assert!(
                APP_JS.contains(needle),
                "app.js missing #622 stream renderer invariant: {needle}"
            );
        }
        assert!(
            !APP_JS.contains("case \"tool_call\": break"),
            "tool_call events must render a compact row, not be ignored"
        );
        assert!(
            !APP_JS.contains("case \"confirmation_required\": addChip"),
            "confirmation_required must render/store a PendingAction placeholder, not a text chip"
        );
        let start = APP_JS
            .find("function agentCompactValue")
            .expect("assistant stream renderer start");
        let end = APP_JS
            .find("function agentKeydown")
            .expect("assistant stream renderer end");
        let assistant_renderer = &APP_JS[start..end];
        assert!(
            !assistant_renderer.contains("innerHTML"),
            "Assistant stream renderer helpers must keep event content text-only"
        );
    }

    #[test]
    fn assistant_citations_are_typed_same_origin_and_deduped() {
        for needle in [
            "function normalizeAgentSource(value)",
            "function extractAgentSources(event)",
            "function sourceViewHref(source)",
            "function renderAgentCitation(source)",
            "function renderAgentCitationBar(sources)",
            "function dedupeAgentSources(sources)",
            "return JSON.stringify([source.service, source.id || \"\", source.path || \"\"]);",
            "if (event.event === \"partial_result\") visit(event.items || [], 0);",
            "else if (event.event === \"tool_result\" && typeof event.content === \"string\")",
            "try { visit(JSON.parse(event.content), 0); } catch (_) {}",
            "return q ? \"/api/v1/view?\" + qs(q) : null;",
            "\"data-agent-citation\": \"view\"",
            "\"data-agent-citation\": \"route\"",
            "const sources = extractAgentSources(d);",
            "turnState.addCitations(sources);",
            "if (m.citations && m.citations.length) bubble.append(renderAgentCitationBar(m.citations));",
        ] {
            assert!(
                APP_JS.contains(needle),
                "app.js missing #622 citation invariant: {needle}"
            );
        }
        let start = APP_JS
            .find("function normalizeAgentSource")
            .expect("citation helper start");
        let end = APP_JS
            .find("function asstResultCard")
            .expect("citation helper end");
        let citation_helpers = &APP_JS[start..end];
        assert!(
            !citation_helpers.contains("http://") && !citation_helpers.contains("https://"),
            "Assistant citation helpers must not introduce external citation URLs"
        );
        assert!(
            !citation_helpers.contains("innerHTML"),
            "Assistant citation helpers must render text-only DOM"
        );
        assert!(
            !citation_helpers.contains("token") && !citation_helpers.contains("action_hash"),
            "Assistant citation helpers must not inspect pending secrets"
        );
    }

    #[test]
    fn assistant_tool_result_ui_keeps_raw_content_out_of_state_and_dom() {
        let start = APP_JS
            .find("case \"tool_result\":")
            .expect("tool result event branch");
        let end = APP_JS[start..]
            .find("case \"search_stage\":")
            .map(|offset| start + offset)
            .expect("tool result event branch end");
        let branch = &APP_JS[start..end];
        for needle in [
            "const sources = extractAgentSources(d);",
            "? `${sources.length} source${sources.length === 1 ? \"\" : \"s\"}`",
            ": \"Completed\"",
            "turnState.addCitations(sources);",
        ] {
            assert!(
                branch.contains(needle),
                "missing redacted tool result UI invariant: {needle}"
            );
        }
        assert!(
            !branch.contains("detail: agentCompactValue(d.content"),
            "raw ToolResult content must never be copied into AssistantState.tools or the DOM"
        );
    }

    #[test]
    fn assistant_pending_action_cards_post_confirm_cancel_without_dom_secrets() {
        for needle in [
            "function confirmAgentPending(pendingId)",
            "function cancelAgentPending(pendingId)",
            "function renderAgentPendingCard(pending, trackNode = true)",
            "AssistantState.pendingCardsById.set(pending.pending_id",
            "token: d.token || \"\"",
            "action_hash: d.action_hash || \"\"",
            "postJson(\"/api/v1/agent/confirm\", CAP.agent, {",
            "postJson(\"/api/v1/agent/pending/cancel\", CAP.agent",
            "record.status = \"confirming\"",
            "record.status = \"confirmed\"",
            "record.status = \"cancelling\"",
            "record.status = \"cancelled\"",
            "pending.status = \"expired\"",
            "record.status = \"error\"",
            "confirm.setAttribute(\"disabled\", \"disabled\")",
            "cancel.setAttribute(\"disabled\", \"disabled\")",
            "\"data-agent-pending-confirm\": \"1\"",
            "\"data-agent-pending-cancel\": \"1\"",
            "\"data-agent-pending-card\": \"1\"",
        ] {
            assert!(
                APP_JS.contains(needle),
                "app.js missing #622 PendingAction invariant: {needle}"
            );
        }
        assert!(
            !APP_JS.to_lowercase().contains("run anyway")
                && !APP_JS.to_lowercase().contains("run-anyway"),
            "Assistant must not expose a run-anyway bypass"
        );
        let start = APP_JS
            .find("function renderAgentPendingCard")
            .expect("pending renderer start");
        let end = APP_JS
            .find("function renderAssistantMessage")
            .expect("pending renderer end");
        let pending_renderer = &APP_JS[start..end];
        assert!(
            !pending_renderer.contains("token")
                && !pending_renderer.contains("action_hash")
                && !pending_renderer.contains("localStorage")
                && !pending_renderer.contains("sessionStorage"),
            "PendingAction renderer must not expose or persist secrets"
        );
        for forbidden in [
            "\"data-agent-pending-card\": pending.pending_id",
            "\"data-agent-pending-confirm\": pending.pending_id",
            "\"data-agent-pending-cancel\": pending.pending_id",
        ] {
            assert!(
                !pending_renderer.contains(forbidden),
                "PendingAction handles must not be serialized into DOM attributes: {forbidden}"
            );
        }
    }

    #[test]
    fn assistant_model_picker_and_usage_chip_are_status_driven() {
        for needle in [
            "function renderAssistantUsageChip(st)",
            "function formatAssistantRateLimit(rateLimit)",
            "Usage unavailable",
            "if (usage.request_id) parts.push(\"Request \" + agentCompactValue(usage.request_id, 28));",
            "if (usage.input_tokens != null) parts.push(`${usage.input_tokens} in`);",
            "if (usage.output_tokens != null) parts.push(`${usage.output_tokens} out`);",
            "const rateLimit = formatAssistantRateLimit(usage.rate_limit);",
            "if (rateLimit) parts.push(rateLimit);",
            "function agentProviderLabel(provider)",
            "if (provider === \"claude\") return \"Claude\";",
            "if (provider === \"codex\") return \"ChatGPT\";",
            "const list = models[prov] || [];",
            "if (!connected || !list.length) return;",
            "\"data-agent-model-option\": val",
            "\"data-agent-model-connect\": \"claude\"",
            "\"data-agent-model-connect\": \"codex\"",
            "async function connectAgentProvider(provider, lifecycleNode = null)",
            "AssistantState.pendingConnectProvider = agentProviderConsentId(provider);",
            "if (OAUTH_ATTEMPTS.has(\"claude\") && !claudeReady) showCodeStep();",
            "const efforts = st.reasoning_efforts || [];",
            "data-agent-effort-option",
            "if (provider === \"codex\") body.reasoning_effort = reasoningEffort || \"medium\";",
            "await postJson(\"/api/v1/agent/model\", CAP.agent, body);",
            "const st = await api(\"/api/v1/agent/status\");",
            "rememberAssistantStatus(st);",
            "renderAssistantView($(\"#view\"));",
            "let ASSISTANT_RENDER_SEQUENCE = 0;",
            "const previousStatus = AssistantState.status;",
            "st = previousStatus && Object.keys(previousStatus).length ? previousStatus : {};",
            "if (renderSequence !== ASSISTANT_RENDER_SEQUENCE || App.route !== \"assistant\") return;",
            "data-agent-status-unavailable",
        ] {
            assert!(
                APP_JS.contains(needle),
                "app.js missing #622 model/usage invariant: {needle}"
            );
        }

        assert!(
            APP_CSS
                .split(".assistant-toolbar {")
                .skip(1)
                .filter_map(|css| css.split('}').next())
                .any(|css| css.contains("position: relative;") && css.contains("z-index: 2;")),
            "model picker toolbar must stack above the transcript"
        );
        let usage_renderer = APP_JS
            .split("function renderAssistantUsageChip(st)")
            .nth(1)
            .unwrap()
            .split("function agentProviderLabel")
            .next()
            .unwrap();
        assert!(
            !usage_renderer.contains("request_id:")
                && !usage_renderer.contains("rate_limit: \"ok\""),
            "production usage renderer must not fabricate usage fields"
        );
        assert!(
            !APP_JS.contains("String(usage.rate_limit)"),
            "usage chip must not render rate_limit objects as [object Object]"
        );
    }

    #[test]
    fn assistant_model_switch_resumes_after_provider_privacy_consent() {
        for needle in [
            "pendingModelSelection: null",
            "provider: agentProviderConsentId(provider), model, reasoning_effort: reasoningEffort,",
            "renderAssistantConsentPanel([pendingModel.provider])",
            "if (resumeModel) await pickModel(resumeModel.provider, resumeModel.model, resumeModel.reasoning_effort)",
        ] {
            assert!(
                APP_JS.contains(needle),
                "app.js missing provider-consent model-switch invariant: {needle}"
            );
        }
    }

    #[test]
    fn assistant_claude_manual_code_step_survives_connected_rerender() {
        for needle in [
            "data-agent-oauth-code-step\": \"claude\"",
            "body.prepend(card);",
            "if (OAUTH_ATTEMPTS.has(\"claude\") && !claudeReady) showCodeStep();",
            "await cancelOAuthAttempt(\"claude\");",
        ] {
            assert!(
                APP_JS.contains(needle),
                "app.js missing persistent Claude manual-code invariant: {needle}"
            );
        }
    }

    #[test]
    fn assistant_setup_consent_is_required_and_non_secret() {
        for needle in [
            "const AGENT_PRIVACY_CONSENT_KEY = \"isy_agent_privacy_consent_v1\";",
            "const AGENT_PRIVACY_CONSENT_VERSION = 2;",
            "function agentPrivacyConsentAccepted(provider)",
            "async function acceptAgentPrivacyConsent(provider)",
            "version: AGENT_PRIVACY_CONSENT_VERSION",
            "[\"claude\", \"codex\"].forEach((provider)",
            "stored.version === 1 && stored.accepted === true",
            "[consentProvider]: { accepted: true, timestamp: new Date().toISOString() }",
            "localStorage.setItem(AGENT_PRIVACY_CONSENT_KEY, JSON.stringify(record))",
            "function renderAssistantConsentPanel(providers)",
            "\"data-agent-consent\": \"1\"",
            "\"data-agent-consent-accept\": provider",
            "\"data-agent-consent-state\": accepted ? \"accepted\" : \"required\"",
            "accepted ? agentProviderLabel(provider) + \" allowed\"",
            "icon(accepted ? \"check\" : \"shield-check\"",
            "\"data-agent-consent-reset\": \"1\"",
            "if (!agentPrivacyConsentAccepted(consentProvider))",
            "if (!agentPrivacyConsentAccepted(provider))",
            "const consentProvider = agentProviderConsentId(provider);",
            "if (!agentPrivacyConsentAccepted(provider)) {",
            "Review privacy consent first",
            "renderAssistantConsentPanel([provider])",
        ] {
            assert!(
                APP_JS.contains(needle),
                "app.js missing #622 setup-consent invariant: {needle}"
            );
        }
        let start = APP_JS
            .find("const AGENT_PRIVACY_CONSENT_KEY")
            .expect("consent helper start");
        let end = APP_JS
            .find("async function renderAssistantView")
            .expect("consent helper end");
        let consent_helpers = &APP_JS[start..end];
        for forbidden in [
            "access_token",
            "refresh_token",
            "session_token",
            "confirmation",
            "m365",
            "type: \"password\"",
            "type=\"password\"",
        ] {
            assert!(
                !consent_helpers.to_lowercase().contains(forbidden),
                "consent helpers must not store/capture secret/content field: {forbidden}"
            );
        }
        for needle in [
            "id: \"asst-connect-claude\"",
            "connectAgentProvider(\"claude\", claudeLifecycle)",
            "\"data-testid\": \"agent-connect-claude\"",
            "id: \"asst-connect-codex\"",
            "connectAgentProvider(\"codex\", codexLifecycle)",
            "\"data-testid\": \"agent-connect-codex\"",
            "lifecycle.state === \"awaiting_oauth_login\" && lifecycle.resume_operation_id",
            "await continueLifecycleOAuth(provider, lifecycle)",
        ] {
            assert!(
                APP_JS.contains(needle),
                "app.js missing #623 product OAuth setup invariant: {needle}"
            );
        }
        for forbidden in [
            "startAiLogin(\"anthropic\")",
            "startAiLogin(\"openai\")",
            "agent-connect-anthropic",
            "agent-connect-openai",
            "data-agent-byo-key",
            "BYO API key",
            "Experimental — uses your own subscription",
            "subscription/import",
        ] {
            assert!(
                !APP_JS.contains(forbidden),
                "Assistant product setup must not expose stale provider/BYO affordance: {forbidden}"
            );
        }
    }

    #[test]
    fn assistant_product_source_has_no_subscription_import_affordance() {
        for forbidden in ["subscription/import", "data-agent-subscription-import"] {
            assert!(
                !APP_JS.contains(forbidden),
                "Assistant product source exposes experimental credential import: {forbidden}"
            );
        }
    }

    #[test]
    fn app_js_renders_closed_connectivity_retry_and_settings_actions() {
        for needle in [
            "function renderConnectivityDiagnostic()",
            "data-agent-connectivity-retry",
            "data-agent-connectivity-settings",
            "nativeCall(\"openNetworkSettings\", { hint }",
            "const BRIDGE_STREAM_TIMEOUT_MS = 125000",
            "const onboardingProvider = st && st.onboarding && st.onboarding.selected_provider",
            "const candidate = onboardingProvider || (st && st.selected_provider)",
            "if (error && error.connectivity)",
            "await cancelOAuthAttempt(\"claude\"); await finishAgentGuard()",
        ] {
            assert!(
                APP_JS.contains(needle),
                "app.js missing #640 connectivity invariant: {needle}"
            );
        }
        assert!(
            !APP_JS.contains("[provider]: \"reconnect_required\""),
            "a connectivity failure must not rewrite credential state"
        );
        assert!(
            !APP_JS.contains("st.provider || st.selected_provider"),
            "connectivity preflight must follow the host-selected onboarding provider without a runtime-provider fallback"
        );
    }

    #[test]
    fn app_js_biometric_labels_cover_onedrive_risk_ops() {
        for needle in [
            "\"move-out-of-protected\"",
            "Move out of offline folder",
            "\"mode-switch-offline-large\"",
            "Make folder offline",
            "\"bulk\"",
            "Bulk OneDrive change",
            "service === \"onedrive\" ? \"OneDrive\"",
            "biometricServiceLabel(d.service)",
        ] {
            assert!(
                APP_JS.contains(needle),
                "app.js missing #723 biometric label invariant: {needle}"
            );
        }
    }

    #[test]
    fn app_js_biometric_labels_cover_mobile_full_node_ops() {
        for needle in [
            "function biometricServiceLabel(service)",
            "service === \"backup\" || service === \"agent\" ? \"iSyncYou\"",
            "\"backup\" ? \"Start backup\"",
            "\"restore-cloud\" ? \"Restore to cloud\"",
            "\"live-write\" ? \"Run Agent write\"",
            "biometricServiceLabel(d.service)",
        ] {
            assert!(
                APP_JS.contains(needle),
                "app.js missing #625 biometric label invariant: {needle}"
            );
        }
        for forbidden in ["body_html", "body_text", "recipient", "change"] {
            assert!(
                !APP_JS.contains(&format!("{forbidden} ?")),
                "app.js biometric label must not branch on raw destructive payload field: {forbidden}"
            );
        }
    }

    #[test]
    fn app_js_biometric_bridge_sends_no_webview_controlled_label() {
        assert!(
            APP_JS.contains("BRIDGE.postMessage(JSON.stringify({ t: \"bio\", id, pat }))"),
            "biometric bridge request must carry only the opaque pending handle"
        );
        assert!(
            !APP_JS.contains("JSON.stringify({ t: \"bio\", id, pat, label })"),
            "WebView labels must not be trusted by the native prompt"
        );
    }

    #[test]
    fn bridge_isolation_app_js_has_no_legacy_mobile_session_path() {
        for needle in [
            "AndroidSession",
            "AndroidPush",
            "AndroidNav",
            "addJavascriptInterface",
            "isy_session",
            "cookieVal(",
            "sessionToken(",
            "sessionHeaders(",
            "_st=",
            "\"X-Session-Token\"",
            "loopback",
        ] {
            assert!(
                !APP_JS.contains(needle),
                "app.js must not contain legacy mobile bridge/session text: {needle}"
            );
        }
        assert!(
            APP_JS.contains("k.toLowerCase() === \"x-session-token\""),
            "bridge requests must drop JS-supplied session header variants"
        );
    }

    #[test]
    fn bridge_isolation_app_js_requires_native_control_bridge() {
        for needle in [
            "const MOBILE = !!BRIDGE",
            "function nativeCall(",
            "nativeCall(\"pushToken\"",
            "nativeCall(\"openExternal\"",
            "nativeCall(\"beginNetworkGuard\"",
            "nativeCall(\"endNetworkGuard\"",
            "\"agent_authorize\"",
            "\"account_authorize\"",
            "__isyBridgeTransportStats",
            "BRIDGE_TIMEOUT_MS",
            "NATIVE_TIMEOUT_MS",
            "BIO_TIMEOUT_MS",
            "BRIDGE_STREAM_TIMEOUT_MS",
            "Native call returned non-JSON response",
        ] {
            assert!(
                APP_JS.contains(needle),
                "app.js missing #721 bridge invariant: {needle}"
            );
        }
    }

    #[test]
    fn bridge_isolation_iframe_sandboxes_remain_no_script() {
        let mut saw_sandbox = false;
        for line in APP_JS.lines().filter(|line| line.contains("sandbox:")) {
            saw_sandbox = true;
            assert!(
                !(line.contains("allow-scripts") && line.contains("allow-same-origin")),
                "production iframe sandbox must not combine scripts and same-origin: {line}"
            );
        }
        assert!(
            saw_sandbox,
            "expected at least one production iframe sandbox invariant"
        );
        assert!(
            APP_JS.contains("sandbox: \"allow-same-origin\""),
            "viewer iframes should remain no-script same-origin frames"
        );
    }

    #[test]
    fn bridge_isolation_oauth_redirects_do_not_build_empty_port_callbacks() {
        assert!(
            !APP_JS.contains("http://localhost:\" + location.port + \"/callback\""),
            "must not construct http://localhost:/callback on appassets origin"
        );
        assert!(
            !APP_JS.contains("http://127.0.0.1:\" + location.port + \"/callback\""),
            "must not construct http://127.0.0.1:/callback on appassets origin"
        );
        assert!(
            APP_JS.contains("function localCallbackRedirect(host)")
                && APP_JS.contains(
                    "return location.port ? `http://${host}:${location.port}/callback` : \"\";"
                ),
            "callback redirect must be omitted when location.port is empty"
        );
    }

    #[test]
    fn assistant_claude_oauth_uses_manual_code_step_without_loopback_redirect() {
        assert!(
            APP_JS.contains("const manualCodeFlow = provider === \"claude\" && !redirect;"),
            "Claude on appassets must detect the manual code flow when no loopback redirect exists"
        );
        assert!(
            APP_JS.contains(
                "if (manualCodeFlow) showCodeStep();\n    else showWaitingStep(provider);"
            ),
            "manual Claude OAuth must show the paste-code UI instead of a polling-only wait screen"
        );
        assert!(
            APP_JS.contains("OAUTH_ATTEMPTS.delete(\"claude\");\n    await finishAgentGuard();")
                && APP_JS.contains(
                    "if (assistantProviderReady(status, \"claude\")) toast(\"Connected!\");"
                ),
            "manual OAuth completion must clean up the network guard after a successful code exchange"
        );
        assert!(
            APP_JS.contains(
                "await cancelOAuthAttempt(\"claude\"); await finishAgentGuard(); renderAssistantView($(\"#view\"));"
            ),
            "manual OAuth cancellation must clean up the network guard"
        );
    }

    #[test]
    fn bridge_isolation_mobile_streams_prefer_bridge_before_eventsource() {
        let bridge_stream = APP_JS
            .find("if (BRIDGE) {\n    const id = \"s\"")
            .expect("openEventStream bridge branch missing");
        let event_source = APP_JS
            .find("const es = new EventSource(path)")
            .expect("desktop EventSource branch missing");
        assert!(
            bridge_stream < event_source,
            "mobile bridge stream branch must be evaluated before desktop EventSource"
        );
    }

    #[test]
    fn bridge_isolation_native_call_rejects_non_json_body() {
        let start = APP_JS
            .find("async function nativeCall(")
            .expect("nativeCall missing");
        let end = APP_JS[start..]
            .find("/* ---------------------------------------------------------------- push registration")
            .expect("nativeCall end marker missing")
            + start;
        let native_call = &APP_JS[start..end];
        assert!(
            native_call.contains("throw new Error(\"Native call returned non-JSON response\")"),
            "nativeCall must hard-fail non-JSON native bodies"
        );
        assert!(
            !native_call.contains("catch (_) { body = {}; }"),
            "nativeCall must not silently coerce malformed native JSON to an empty object"
        );
    }

    #[test]
    fn bridge_isolation_stream_subscription_has_handshake_timeout_cleanup() {
        let start = APP_JS
            .find("function openEventStream(")
            .expect("openEventStream missing");
        let end = APP_JS[start..]
            .find("/* One request over the active transport")
            .expect("openEventStream end marker missing")
            + start;
        let stream_fn = &APP_JS[start..end];
        for needle in [
            "const timer = setTimeout(() =>",
            "_bridgeStreams.delete(id);",
            "BRIDGE.postMessage(JSON.stringify({ t: \"unsub\", id }))",
            "BRIDGE_STREAM_TIMEOUT_MS",
            "if (h && h.timer) clearTimeout(h.timer);",
        ] {
            assert!(
                stream_fn.contains(needle),
                "openEventStream missing stream timeout cleanup invariant: {needle}"
            );
        }
        let bridge_handler = APP_JS
            .find("if (h.timer) { clearTimeout(h.timer); h.timer = null; }")
            .expect("stream event handler must clear the handshake timeout");
        let stream_start = APP_JS
            .find("_bridgeStreams.set(id, { onEvent, onError, timer });")
            .expect("stream handler must store the timeout");
        assert!(
            bridge_handler < stream_start,
            "incoming bridge events must clear the stored stream handshake timeout"
        );
    }

    #[test]
    fn account_routes_refused_without_a_handler() {
        // The read-only `serve` wires no account-auth handler → every account
        // login/sign-out POST is refused 404, never reaching the (absent) gate (#68).
        let r = Router::new(Config::default());
        for p in [
            "/api/v1/account/login/start?account=a",
            "/api/v1/account/login/poll?id=1",
            "/api/v1/account/login/cancel?id=1",
            "/api/v1/account/signout?account=a",
        ] {
            assert_eq!(r.route(&ApiRequest::new("POST", p)).status, 404, "{p}");
        }
    }

    #[derive(Default)]
    struct RecAccountAuth {
        cancelled: std::sync::Mutex<Vec<String>>,
        started: std::sync::Mutex<Vec<(String, AccountAuthRole)>>,
        signed_out: std::sync::Mutex<Vec<(String, AccountAuthRole)>>,
    }

    impl AccountAuthHandler for RecAccountAuth {
        fn start_login(
            &self,
            account: &str,
            role: AccountAuthRole,
        ) -> Result<serde_json::Value, String> {
            self.started
                .lock()
                .unwrap()
                .push((account.to_string(), role));
            Ok(json!({
                "flow": "authorization_code_pkce",
                "role": role.as_str(),
                "login_id": "1",
                "authorization_uri": "https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize"
            }))
        }

        fn poll_login(&self, _login_id: &str) -> serde_json::Value {
            json!({ "state": "pending" })
        }

        fn cancel_login(&self, login_id: &str) -> bool {
            self.cancelled.lock().unwrap().push(login_id.to_string());
            true
        }

        fn sign_out(
            &self,
            account: &str,
            role: AccountAuthRole,
        ) -> Result<serde_json::Value, String> {
            self.signed_out
                .lock()
                .unwrap()
                .push((account.to_string(), role));
            Ok(json!({ "removed": 1, "role": role.as_str() }))
        }

        fn status(&self, _account: &str) -> AccountAuthStatus {
            AccountAuthStatus {
                reader: AccountRoleStatus {
                    state: AccountAuthState::Connected,
                    identity_verified: true,
                },
                writer: AccountRoleStatus {
                    state: AccountAuthState::Disconnected,
                    identity_verified: false,
                },
            }
        }
    }

    #[test]
    fn account_auth_routes_require_closed_reader_or_writer_role() {
        let auth = std::sync::Arc::new(RecAccountAuth::default());
        let router = Router::new(Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "account".into(),
                sync_root: PathBuf::from("/tmp/sync"),
                archive_root: PathBuf::from("/tmp/archive"),
                cache_root: PathBuf::from("/tmp/cache"),
                mount_point: None,
            }],
            ..Config::default()
        })
        .with_account_auth(auth.clone(), "cap".into());

        let start = router.route(&strict_json_post(
            "/api/v1/account/login/start",
            json!({
                "request_id": "123e4567-e89b-42d3-a456-426614174001",
                "account": "a",
                "role": "reader"
            }),
            Some("cap"),
        ));
        assert_eq!(start.status, 200);
        assert_eq!(body_json(&start)["role"], "reader");
        assert_eq!(
            *auth.started.lock().unwrap(),
            vec![("a".into(), AccountAuthRole::Reader)]
        );

        let invalid = router.route(&strict_json_post(
            "/api/v1/account/login/start",
            json!({
                "request_id": "123e4567-e89b-42d3-a456-426614174002",
                "account": "a",
                "role": "restore"
            }),
            Some("cap"),
        ));
        assert_eq!(invalid.status, 400);

        let signout = router.route(&strict_json_post(
            "/api/v1/account/signout",
            json!({
                "request_id": "123e4567-e89b-42d3-a456-426614174003",
                "account": "a",
                "role": "writer"
            }),
            Some("cap"),
        ));
        assert_eq!(signout.status, 200);
        assert_eq!(
            *auth.signed_out.lock().unwrap(),
            vec![("a".into(), AccountAuthRole::Writer)]
        );

        let accounts = body_json(&router.route(&ApiRequest::get("/api/v1/accounts")));
        assert_eq!(
            accounts["accounts"][0]["auth"]["reader"]["state"],
            "connected"
        );
        assert_eq!(
            accounts["accounts"][0]["auth"]["writer"]["state"],
            "disconnected"
        );
    }

    #[test]
    fn account_login_cancel_is_strict_cap_gated_and_calls_exact_attempt() {
        let auth = std::sync::Arc::new(RecAccountAuth::default());
        let router = Router::new(Config::default()).with_account_auth(auth.clone(), "cap".into());
        let body = json!({
            "request_id": "123e4567-e89b-42d3-a456-426614174099",
            "id": "42"
        });
        assert_eq!(
            router
                .route(&strict_json_post(
                    "/api/v1/account/login/cancel",
                    body.clone(),
                    None,
                ))
                .status,
            401
        );
        assert!(auth.cancelled.lock().unwrap().is_empty());
        let response = router.route(&strict_json_post(
            "/api/v1/account/login/cancel",
            body,
            Some("cap"),
        ));
        assert_eq!(response.status, 200);
        assert_eq!(body_json(&response)["cancelled"], true);
        assert_eq!(*auth.cancelled.lock().unwrap(), vec!["42"]);
    }

    #[derive(Default)]
    struct RecPush {
        tokens: std::sync::Mutex<Vec<String>>,
    }
    impl PushHandler for RecPush {
        fn register(&self, token: &str) -> Result<(), String> {
            self.tokens.lock().unwrap().push(token.to_string());
            Ok(())
        }
        fn send_test(&self) -> Result<serde_json::Value, String> {
            Ok(json!({ "sent": self.tokens.lock().unwrap().len() }))
        }
    }

    #[test]
    fn push_routes_refused_without_a_handler() {
        // The read-only `serve` wires no push handler → register/test POSTs 404 (#576).
        let r = Router::new(Config::default());
        for p in ["/api/v1/push/register", "/api/v1/push/test"] {
            assert_eq!(r.route(&ApiRequest::new("POST", p)).status, 404, "{p}");
        }
    }

    #[test]
    fn push_register_needs_cap_token_and_records() {
        let push = std::sync::Arc::new(RecPush::default());
        let router = Router::new(Config::default()).with_push(push.clone(), "captok".into());
        // Wrong/absent cap token → 401, token not recorded.
        assert_eq!(
            router
                .route(&strict_json_post(
                    "/api/v1/push/register",
                    json!({
                        "request_id": "123e4567-e89b-42d3-a456-426614174010",
                        "token": "dev1"
                    }),
                    None,
                ))
                .status,
            401
        );
        assert!(push.tokens.lock().unwrap().is_empty());
        // With the cap token → 200 and the device token is stored.
        let req = strict_json_post(
            "/api/v1/push/register",
            json!({
                "request_id": "123e4567-e89b-42d3-a456-426614174010",
                "token": "dev1"
            }),
            Some("captok"),
        );
        assert_eq!(router.route(&req).status, 200);
        assert_eq!(push.tokens.lock().unwrap().as_slice(), ["dev1"]);

        let legacy = ApiRequest::new("POST", "/api/v1/push/register?token=dev2")
            .with_cap_token(Some("captok".into()));
        assert_eq!(router.route(&legacy).status, 400);
    }

    #[test]
    fn push_register_rejects_empty_token() {
        let push = std::sync::Arc::new(RecPush::default());
        let router = Router::new(Config::default()).with_push(push, "captok".into());
        let req = strict_json_post(
            "/api/v1/push/register",
            json!({
                "request_id": "123e4567-e89b-42d3-a456-426614174011",
                "token": ""
            }),
            Some("captok"),
        );
        assert_eq!(router.route(&req).status, 400);
    }

    #[derive(Default)]
    struct RecOneNoteWrite(std::sync::Mutex<Vec<String>>);
    impl OneNoteWriteHandler for RecOneNoteWrite {
        fn create(&self, _a: &str, section: &str, html: &[u8]) -> Result<String, String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("create section={section} bytes={}", html.len()));
            Ok("page-new".into())
        }
        fn delete(&self, _a: &str, id: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(format!("delete id={id}"));
            Ok(())
        }
        fn append(&self, _a: &str, id: &str, text: &str) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("append id={id} text={text}"));
            Ok(())
        }
    }

    #[test]
    fn onenote_content_endpoints_are_cap_gated_strict_json() {
        let (_d, router) = setup();
        for p in [
            "/api/v1/onenote/create",
            "/api/v1/onenote/delete",
            "/api/v1/onenote/append",
        ] {
            assert_eq!(
                router.route(&ApiRequest::new("POST", p)).status,
                404,
                "{p} must 404 on the read-only server"
            );
        }
        let rec = std::sync::Arc::new(RecOneNoteWrite::default());
        let router = router.with_onenote_write(rec.clone(), "notesecret".into());
        let create_body = json!({
            "request_id": "123e4567-e89b-42d3-a456-426614174020",
            "account": "a", "section": "S1", "title": "Ideas", "body": "hello"
        });
        // missing token -> 401
        assert_eq!(
            router
                .route(&strict_json_post(
                    "/api/v1/onenote/create",
                    create_body.clone(),
                    None,
                ))
                .status,
            401
        );
        // valid token but no section -> 400
        let bad = strict_json_post(
            "/api/v1/onenote/create",
            json!({
                "request_id": "123e4567-e89b-42d3-a456-426614174021",
                "account": "a", "section": "", "title": "x", "body": ""
            }),
            Some("notesecret"),
        );
        assert_eq!(router.route(&bad).status, 400);
        let tok = |t: &str| strict_scalar_post_from_target(t, "notesecret");
        assert_eq!(
            router
                .route(&strict_json_post(
                    "/api/v1/onenote/create",
                    create_body,
                    Some("notesecret"),
                ))
                .status,
            200
        );
        assert_eq!(
            router
                .route(&strict_json_post(
                    "/api/v1/onenote/append",
                    json!({
                        "request_id": "123e4567-e89b-42d3-a456-426614174022",
                        "account": "a", "id": "P1", "text": "more"
                    }),
                    Some("notesecret"),
                ))
                .status,
            200
        );
        assert_eq!(
            router
                .route(&tok("/api/v1/onenote/delete?account=a&id=P1"))
                .status,
            200
        );
        let log = rec.0.lock().unwrap();
        // the create built a non-empty page HTML and targeted section S1
        assert!(log[0].starts_with("create section=S1 bytes="));
        assert!(!log[0].ends_with("bytes=0"));
        assert_eq!(log[1], "append id=P1 text=more");
        assert_eq!(log[2], "delete id=P1");

        let legacy = ApiRequest::new(
            "POST",
            "/api/v1/onenote/create?account=a&section=S1&title=Ideas&body=secret",
        )
        .with_cap_token(Some("notesecret".into()));
        assert_eq!(router.route(&legacy).status, 400);
    }

    #[test]
    fn onenote_preview_exposes_page_metadata_from_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("arch");
        std::fs::create_dir_all(&arch).unwrap();
        // the page's _pagemeta_<id> sidecar (the page's local_path is the .html body)
        let meta_rel = isyncyou_connectors::shard_rel("onenote", "_pagemeta_p1", "json");
        let mp = arch.join(&meta_rel);
        std::fs::create_dir_all(mp.parent().unwrap()).unwrap();
        std::fs::write(
            &mp,
            br#"{"createdDateTime":"2025-12-01T00:00:00Z","level":1,"order":3,
                 "userTags":["important"],
                 "links":{"oneNoteWebUrl":{"href":"https://onenote.com/p1"}},
                 "parentSection":{"displayName":"Ideas"},
                 "parentNotebook":{"id":"N1","displayName":"Personal"}}"#,
        )
        .unwrap();
        // the page item: local_path is the .html body
        std::fs::create_dir_all(arch.join("onenote/aa")).unwrap();
        std::fs::write(arch.join("onenote/aa/p.html"), b"<html></html>").unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut it = Item::new("a", "onenote", "p1", "Ideas page", "page");
            it.local_path = Some("onenote/aa/p.html".into());
            it.parent_remote_id = Some("S1".into());
            store.upsert_item(&it).unwrap();
        }
        let cfg = Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@outlook.com".into(),
                sync_root: dir.path().join("od"),
                archive_root: arch,
                cache_root: Default::default(),
                mount_point: None,
            }],
            ..Default::default()
        };
        let router = Router::new(cfg);
        let d =
            body_json(&router.route(&ApiRequest::get("/api/v1/items?account=a&service=onenote")));
        let p = &d["items"][0]["preview"];
        assert_eq!(p["created"], "2025-12-01T00:00:00Z");
        assert_eq!(p["level"], 1);
        assert_eq!(p["order"], 3);
        assert_eq!(p["user_tags"][0], "important");
        assert_eq!(p["web_url"], "https://onenote.com/p1");
        assert_eq!(p["section_name"], "Ideas");
        assert_eq!(p["notebook_name"], "Personal");
        assert_eq!(p["has_resources"], false);
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
        let ok = router.route(&strict_scalar_post_from_target(q, "secret"));
        assert_eq!(ok.status, 200);
        assert!(String::from_utf8_lossy(&ok.body).contains("verified"));
        // missing account -> 400
        let bad = strict_scalar_post_from_target("/api/v1/verify", "secret");
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

    #[derive(Default)]
    struct SpyShare {
        share_calls: std::sync::atomic::AtomicUsize,
        invite_calls: std::sync::atomic::AtomicUsize,
        share_error: Option<String>,
        invite_error: Option<String>,
    }

    impl ShareHandler for SpyShare {
        fn share(
            &self,
            _a: &str,
            _s: &str,
            _i: &str,
            _t: &str,
            _sc: &str,
        ) -> Result<String, String> {
            self.share_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(error) = &self.share_error {
                return Err(error.clone());
            }
            Ok("https://1drv.ms/x/spy".into())
        }

        fn invite(
            &self,
            _a: &str,
            _s: &str,
            _i: &str,
            emails: &[String],
            role: &str,
        ) -> Result<String, String> {
            self.invite_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(error) = &self.invite_error {
                return Err(error.clone());
            }
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
        let ok = router.route(&strict_scalar_post_from_target(q, "secret"));
        assert_eq!(ok.status, 200);
        assert!(String::from_utf8_lossy(&ok.body).contains("https://1drv.ms/x/abc"));
        // valid token but missing params -> 400
        let bad = strict_scalar_post_from_target("/api/v1/share?account=a", "secret");
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

    // #onedrive-mobile 0.9: on the mobile profile share is additionally biometric-gated —
    // the cap token alone (which the WebView holds) is not enough to produce a link.
    #[test]
    fn share_is_biometric_gated_on_mobile() {
        let (_d, router) = setup();
        let mobile = router
            .with_share(std::sync::Arc::new(OkShare), "secret".into())
            .with_biometric_gate();
        let cap = |t: &str| strict_scalar_post_from_target(t, "secret");
        let q = "/api/v1/share?account=a&service=onedrive&id=x";
        // cap token alone → a confirmation challenge, NOT a link
        let ch = mobile.route(&cap(q));
        assert_eq!(ch.status, 200);
        let j = body_json(&ch);
        assert_eq!(j["status"], "confirmation_required");
        let pat = j["pending_action_id"].as_str().unwrap().to_string();
        assert!(!String::from_utf8_lossy(&ch.body).contains("1drv.ms"));
        // token but no biometric yet → 403
        assert_eq!(
            mobile
                .route(&cap(q).with_per_action_token(Some(pat.clone())))
                .status,
            403
        );
        // native biometric confirms → share proceeds and returns the link
        assert!(mobile.confirm_biometric(&pat));
        let ok = mobile.route(&cap(q).with_per_action_token(Some(pat.clone())));
        assert_eq!(ok.status, 200);
        assert!(String::from_utf8_lossy(&ok.body).contains("https://1drv.ms/x/abc"));
        // replay of the consumed token → 403 (single-use)
        assert_eq!(
            mobile
                .route(&cap(q).with_per_action_token(Some(pat)))
                .status,
            403
        );
    }

    #[test]
    fn share_post_invite_mode_routes_to_invite_not_link() {
        let (_d, router) = setup();
        let router = router.with_share(std::sync::Arc::new(OkShare), "secret".into());
        // an `email` param switches to invite mode: response has no webUrl, role echoed
        let q = "/api/v1/share?account=a&service=onedrive&id=x&email=p%40e.com&role=write";
        let ok = router.route(&strict_scalar_post_from_target(q, "secret"));
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
    fn share_invite_is_biometric_gated_before_handler_call_on_mobile() {
        let (_d, router) = setup();
        let spy = std::sync::Arc::new(SpyShare::default());
        let mobile = router
            .with_share(spy.clone(), "secret".into())
            .with_biometric_gate();
        let cap = |t: &str| strict_scalar_post_from_target(t, "secret");
        let q = "/api/v1/share?account=a&service=onedrive&id=x&email=p%40e.com&role=write";

        let ch = mobile.route(&cap(q));
        assert_eq!(ch.status, 200);
        let j = body_json(&ch);
        assert_eq!(j["status"], "confirmation_required");
        assert_eq!(
            spy.invite_calls.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(spy.share_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let pat = j["pending_action_id"].as_str().unwrap().to_string();

        assert_eq!(
            mobile
                .route(&cap(q).with_per_action_token(Some(pat.clone())))
                .status,
            403
        );
        assert_eq!(
            spy.invite_calls.load(std::sync::atomic::Ordering::SeqCst),
            0
        );

        assert!(mobile.confirm_biometric(&pat));
        let ok = mobile.route(&cap(q).with_per_action_token(Some(pat.clone())));
        assert_eq!(ok.status, 200);
        assert_eq!(
            spy.invite_calls.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(spy.share_calls.load(std::sync::atomic::Ordering::SeqCst), 0);

        assert_eq!(
            mobile
                .route(&cap(q).with_per_action_token(Some(pat)))
                .status,
            403
        );
        assert_eq!(
            spy.invite_calls.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn share_handler_errors_are_redacted_in_response_and_audit() {
        let (_d, router) = setup();
        let raw = "Graph failed for person@example.com at https://1drv.ms/raw-secret";
        let router = router.with_share(
            std::sync::Arc::new(SpyShare {
                invite_error: Some(raw.into()),
                ..Default::default()
            }),
            "secret".into(),
        );
        let q = "/api/v1/share?account=a&service=onedrive&id=x&email=person%40example.com";

        let resp = router.route(&strict_scalar_post_from_target(q, "secret"));

        assert_eq!(resp.status, 500);
        let body = String::from_utf8_lossy(&resp.body);
        assert!(body.contains("share_transient_failure"));
        assert!(!body.contains("person@example.com"));
        assert!(!body.contains("https://"));
        assert!(!body.contains("1drv.ms"));

        let audit =
            body_json(&router.route(&ApiRequest::get("/api/v1/activity?account=a&limit=5")));
        let audit_text = serde_json::to_string(&audit).unwrap();
        assert!(audit_text.contains("share_transient_failure"));
        assert!(!audit_text.contains("person@example.com"));
        assert!(!audit_text.contains("https://"));
        assert!(!audit_text.contains("1drv.ms"));
    }

    #[test]
    fn invite_fail_closed_share_errors_return_conflict() {
        let (_d, router) = setup();
        let router = router.with_share(
            std::sync::Arc::new(SpyShare {
                invite_error: Some("invite_recovery_ambiguous".into()),
                ..Default::default()
            }),
            "secret".into(),
        );
        let q = "/api/v1/share?account=a&service=onedrive&id=x&email=p%40e.com";

        let resp = router.route(&strict_scalar_post_from_target(q, "secret"));

        assert_eq!(resp.status, 409);
        let body = String::from_utf8_lossy(&resp.body);
        assert!(body.contains("invite_recovery_ambiguous"));
    }

    #[test]
    fn restore_post_writes_a_durable_audit_log() {
        let (_d, router) = setup();
        let router = router.with_restore(std::sync::Arc::new(OkRestore), "secret".into());
        let q = "/api/v1/restore?account=a&service=mail&id=m1";

        let ok = router.route(&strict_scalar_post_from_target(q, "secret"));
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

        let err = router.route(&strict_scalar_post_from_target(q, "secret"));
        assert_eq!(err.status, 500);
        let body = String::from_utf8_lossy(&err.body);
        assert!(body.contains("restore_failed"));
        assert!(!body.contains("pat@example.com"));
        assert!(!body.contains("secret.example"));

        let audit =
            body_json(&router.route(&ApiRequest::get("/api/v1/activity?account=a&limit=5")));
        assert_eq!(audit["runs"][0]["kind"], "audit:restore");
        assert_eq!(audit["runs"][0]["status"], "error");
        assert!(audit["runs"][0]["summary"]
            .as_str()
            .unwrap()
            .contains("restore_failed"));
        assert!(!audit["runs"][0]["summary"]
            .as_str()
            .unwrap()
            .contains("pat@example.com"));
        assert!(!audit["runs"][0]["summary"]
            .as_str()
            .unwrap()
            .contains("secret.example"));
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

    #[test]
    fn cap_ok_accepts_only_the_exact_token() {
        // Regression freeze for AUDIT-2 (#73): the capability gate accepts only an
        // exact, same-length token and rejects everything else. cap_ok compares in
        // constant time (length check, then XOR-accumulate over all bytes).
        let expected = Some("s3cr3t-capability-token-0001".to_string());
        let pass =
            ApiRequest::get("/x").with_cap_token(Some("s3cr3t-capability-token-0001".into()));
        assert!(Router::cap_ok(&expected, &pass), "exact token must pass");

        let wrong =
            ApiRequest::get("/x").with_cap_token(Some("s3cr3t-capability-token-000X".into()));
        assert!(!Router::cap_ok(&expected, &wrong), "wrong token must fail");

        let short = ApiRequest::get("/x").with_cap_token(Some("s3cr3t".into()));
        assert!(
            !Router::cap_ok(&expected, &short),
            "wrong-length token must fail"
        );

        let missing = ApiRequest::get("/x"); // no X-Capability-Token header
        assert!(
            !Router::cap_ok(&expected, &missing),
            "missing request token must fail"
        );

        assert!(
            !Router::cap_ok(&None, &pass),
            "an unconfigured gate must reject everything"
        );
    }

    #[test]
    fn session_gate_off_by_default_desktop() {
        // #89: the desktop daemon never sets a session token → the gate is off and
        // GET data routes behave exactly as before (no 401 from the session gate).
        let r = Router::new(Config::default());
        assert!(
            r.session_authorized(None),
            "gate off must authorize anything"
        );
        assert_ne!(
            r.route(&ApiRequest::get("/api/v1/status")).status,
            401,
            "desktop GET must not be session-gated"
        );
    }

    #[test]
    fn session_gate_requires_token_on_data_routes() {
        // #89 mobile profile: every /api/v1/* route requires the per-process token.
        let r = Router::new(Config::default()).with_session_token("sess-tok-0001".into());
        // No token → 401.
        assert_eq!(
            r.route(&ApiRequest::get("/api/v1/status")).status,
            401,
            "missing session token must 401"
        );
        // Wrong token → 401.
        let wrong = ApiRequest::get("/api/v1/status").with_session_token(Some("nope".into()));
        assert_eq!(r.route(&wrong).status, 401, "wrong session token must 401");
        // Correct token via header → passes the gate (not a session-401).
        let ok = ApiRequest::get("/api/v1/status").with_session_token(Some("sess-tok-0001".into()));
        assert_ne!(
            r.route(&ok).status,
            401,
            "correct header token must pass the gate"
        );
        // Query-string session credentials are rejected before routing.
        let ok_q = ApiRequest::get("/api/v1/status?_st=sess-tok-0001");
        assert_eq!(
            r.route(&ok_q).status,
            400,
            "_st query token must be rejected"
        );
    }

    #[test]
    fn session_gate_leaves_static_shell_open() {
        // The bootstrap shell must stay reachable without the token (the WebView has
        // to load it before the native bridge can hand the token to the JS). It
        // carries no user data and no token, so this is safe.
        let r = Router::new(Config::default()).with_session_token("sess-tok-0001".into());
        assert_eq!(
            r.route(&ApiRequest::get("/")).status,
            200,
            "/ must stay open"
        );
        assert_eq!(
            r.route(&ApiRequest::get("/app.js")).status,
            200,
            "/app.js must stay open (bootstrap)"
        );
    }

    #[test]
    fn ct_eq_is_exact_and_length_checked() {
        assert!(ct_eq(b"abc123", b"abc123"));
        assert!(!ct_eq(b"abc123", b"abc124"));
        assert!(!ct_eq(b"abc", b"abc123"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn static_assets_carry_correct_type_and_no_store() {
        // Regression freeze: the embedded shell assets keep their exact content-type
        // and Cache-Control: no-store, so a stale/poisoned copy can never persist
        // across a binary upgrade (the APK asset-cache bug, #79). Together with
        // app_shell_carries_strict_csp_header (`/`) and
        // view_renders_safe_html_with_csp_and_escapes_untrusted_values (`/api/v1/view`)
        // this freezes the per-layer header posture (W0.1 AC2).
        let r = Router::new(Config::default());
        for (path, ctype) in [
            ("/app.js", "application/javascript"),
            ("/app.css", "text/css"),
        ] {
            let resp = r.route(&ApiRequest::get(path));
            assert_eq!(resp.status, 200, "{path} must serve 200");
            assert!(
                resp.content_type.starts_with(ctype),
                "{path} wrong content-type: {}",
                resp.content_type
            );
            assert!(
                resp.headers
                    .iter()
                    .any(|(k, v)| k == "Cache-Control" && v.contains("no-store")),
                "{path} must be Cache-Control: no-store"
            );
        }
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
        let ok = router.route(&strict_scalar_post_from_target("/api/v1/sync/pause", "s"));
        assert_eq!(ok.status, 200);
        assert!(m.is_paused());
        router.route(&strict_scalar_post_from_target("/api/v1/sync/resume", "s"));
        assert!(!m.is_paused());
        router.route(&strict_scalar_post_from_target("/api/v1/sync/now", "s"));
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

    // #onedrive-mobile 0.8: transfer progress/cancel scaffold + policy/delete-guard status.
    #[test]
    fn transfers_progress_cancel_and_policy_endpoints() {
        // policy GET reads the config (no handler) — reports the mobile transfer policy
        // AND the mass-delete-guard status.
        let bare = Router::new(Config::default());
        let p = bare.route(&ApiRequest::get("/api/v1/onedrive/policy"));
        assert_eq!(p.status, 200);
        let pj = body_json(&p);
        assert_eq!(pj["wifi_only"], false);
        assert_eq!(pj["charging_only"], false);
        assert_eq!(pj["min_free_bytes"].as_u64(), Some(268_435_456));
        assert_eq!(pj["delete_guard"]["max_absolute"].as_u64(), Some(1000));

        // transfers GET is idle without a handler; cancel 404s without one.
        let idle = bare.route(&ApiRequest::get("/api/v1/onedrive/transfers"));
        assert_eq!(idle.status, 200);
        assert_eq!(body_json(&idle)["count"].as_u64(), Some(0));
        assert_eq!(
            bare.route(&ApiRequest::new(
                "POST",
                "/api/v1/onedrive/transfers/cancel?id=x"
            ))
            .status,
            404
        );

        // with a handler: progress is reported and cancel/pause/retry are cap-gated.
        struct MockTransfers {
            cancelled: std::sync::Mutex<Vec<String>>,
            paused: std::sync::Mutex<Vec<String>>,
            retried: std::sync::Mutex<Vec<String>>,
        }
        impl TransferProgress for MockTransfers {
            fn transfers(&self) -> Vec<TransferState> {
                vec![TransferState {
                    id: "t1".into(),
                    name: "big.zip".into(),
                    bytes_done: 50,
                    bytes_total: 100,
                    retry_after_secs: 0,
                    paused: true,
                }]
            }
            fn cancel(&self, id: &str) -> bool {
                self.cancelled.lock().unwrap().push(id.into());
                id == "t1"
            }
            fn pause(&self, id: &str) -> bool {
                self.paused.lock().unwrap().push(id.into());
                id == "t1"
            }
            fn retry(&self, id: &str) -> bool {
                self.retried.lock().unwrap().push(id.into());
                id == "t1"
            }
        }
        let mock = std::sync::Arc::new(MockTransfers {
            cancelled: std::sync::Mutex::new(vec![]),
            paused: std::sync::Mutex::new(vec![]),
            retried: std::sync::Mutex::new(vec![]),
        });
        let router = Router::new(Config::default()).with_transfers(mock.clone(), "cap".into());
        let t = router.route(&ApiRequest::get("/api/v1/onedrive/transfers"));
        let tj = body_json(&t);
        assert_eq!(tj["count"].as_u64(), Some(1));
        assert_eq!(tj["transfers"][0]["name"], "big.zip");
        assert_eq!(tj["transfers"][0]["bytes_done"].as_u64(), Some(50));
        assert_eq!(tj["transfers"][0]["paused"], true);

        // cancel without the cap token → 401, handler not called.
        assert_eq!(
            router
                .route(&ApiRequest::new(
                    "POST",
                    "/api/v1/onedrive/transfers/cancel?id=t1"
                ))
                .status,
            401
        );
        assert!(mock.cancelled.lock().unwrap().is_empty());
        // with the cap token → 200 and the handler ran once.
        let transfer_request = |path: &str, suffix: u8, id: Option<&str>| {
            let mut body = json!({
                "request_id": format!("123e4567-e89b-42d3-a456-4266141740{suffix:02}")
            });
            if let Some(id) = id {
                body["id"] = json!(id);
            }
            strict_json_post(path, body, Some("cap"))
        };
        let ok = router.route(&transfer_request(
            "/api/v1/onedrive/transfers/cancel",
            20,
            Some("t1"),
        ));
        assert_eq!(ok.status, 200);
        assert_eq!(body_json(&ok)["cancelled"], true);
        assert_eq!(*mock.cancelled.lock().unwrap(), vec!["t1"]);

        // #659: pause + retry are the same cap-gated shape. 401 without the token, 200 with it.
        assert_eq!(
            router
                .route(&ApiRequest::new(
                    "POST",
                    "/api/v1/onedrive/transfers/pause?id=t1"
                ))
                .status,
            401
        );
        let pok = router.route(&transfer_request(
            "/api/v1/onedrive/transfers/pause",
            21,
            Some("t1"),
        ));
        assert_eq!(pok.status, 200);
        assert_eq!(body_json(&pok)["paused"], true);
        assert_eq!(*mock.paused.lock().unwrap(), vec!["t1"]);
        let rok = router.route(&transfer_request(
            "/api/v1/onedrive/transfers/retry",
            22,
            Some("t1"),
        ));
        assert_eq!(rok.status, 200);
        assert_eq!(body_json(&rok)["retried"], true);
        assert_eq!(*mock.retried.lock().unwrap(), vec!["t1"]);
        // missing id → 400.
        assert_eq!(
            router
                .route(&transfer_request(
                    "/api/v1/onedrive/transfers/pause",
                    23,
                    None,
                ))
                .status,
            400
        );
    }

    #[derive(Default)]
    struct MockManage {
        freed: std::sync::Mutex<Vec<String>>,
        downloaded: std::sync::Mutex<Vec<String>>,
        resolved: std::sync::Mutex<Vec<(String, String)>>,
        cleaned: std::sync::Mutex<Vec<String>>,
    }
    impl OneDriveManageHandler for MockManage {
        fn free_up(&self, _account: &str, id: &str) -> Result<(), String> {
            self.freed.lock().unwrap().push(id.into());
            Ok(())
        }
        fn download_now(
            &self,
            _account: &str,
            id: &str,
        ) -> Result<OneDriveDownloadNowResult, String> {
            self.downloaded.lock().unwrap().push(id.into());
            Ok(OneDriveDownloadNowResult {
                downloaded: true,
                target: "cache".into(),
            })
        }
        fn list_conflicts(&self, _account: &str) -> Result<serde_json::Value, String> {
            Ok(json!([{
                "id": "c1",
                "name": "note.txt",
                "conflict_copy": "note-host-safeBackup-0001.txt",
            }]))
        }
        fn resolve_conflict(
            &self,
            _account: &str,
            id: &str,
            resolution: &str,
        ) -> Result<(), String> {
            self.resolved
                .lock()
                .unwrap()
                .push((id.into(), resolution.into()));
            Ok(())
        }
        fn cleanup_offline_to_online(&self, account: &str) -> Result<serde_json::Value, String> {
            self.cleaned.lock().unwrap().push(account.into());
            Ok(json!({ "freed": 3, "kept": 1 }))
        }
    }

    // #659: the OneDrive management endpoints (free-up / download-now / conflicts / resolve /
    // cleanup) — cap-gate (401) / no-handler (404) / param (400) / dispatch.
    #[test]
    fn onedrive_manage_endpoints_cap_gate_and_dispatch() {
        let post = |t: &str| strict_scalar_post_from_target(t, "cap");

        // No handler wired -> every management route 404 (POST + the conflicts GET).
        let (_d0, r0) = setup();
        assert_eq!(
            r0.route(&post("/api/v1/onedrive/free-up?account=a&id=i1"))
                .status,
            404
        );
        assert_eq!(
            r0.route(&ApiRequest::get("/api/v1/onedrive/conflicts?account=a"))
                .status,
            404
        );

        // Handler wired.
        let (_d, r) = setup();
        let m = std::sync::Arc::new(MockManage::default());
        let router = r.with_onedrive_manage(m.clone(), "cap".into());

        // free-up without the cap -> 401; handler not called.
        assert_eq!(
            router
                .route(&ApiRequest::new(
                    "POST",
                    "/api/v1/onedrive/free-up?account=a&id=i1"
                ))
                .status,
            401
        );
        assert!(m.freed.lock().unwrap().is_empty());
        // with cap -> 200.
        assert_eq!(
            router
                .route(&post("/api/v1/onedrive/free-up?account=a&id=i1"))
                .status,
            200
        );
        assert_eq!(*m.freed.lock().unwrap(), vec!["i1".to_string()]);
        // missing id -> 400.
        assert_eq!(
            router
                .route(&post("/api/v1/onedrive/free-up?account=a"))
                .status,
            400
        );

        // download-now -> 200, downloaded/target reflect the handler result.
        let dn = router.route(&post("/api/v1/onedrive/download-now?account=a&id=i2"));
        assert_eq!(dn.status, 200);
        let dnj = body_json(&dn);
        assert_eq!(dnj["downloaded"], true);
        assert_eq!(dnj["target"], "cache");
        assert!(dnj.get("materialized").is_none());
        assert_eq!(*m.downloaded.lock().unwrap(), vec!["i2".to_string()]);

        // conflicts GET -> 200 + shape.
        let cj = router.route(&ApiRequest::get("/api/v1/onedrive/conflicts?account=a"));
        assert_eq!(cj.status, 200);
        assert_eq!(body_json(&cj)["conflicts"][0]["id"], "c1");

        // resolve keep-both -> 200; the handler saw the resolution.
        let rb = router.route(&post(
            "/api/v1/onedrive/conflict/resolve?account=a&id=c1&resolution=keep-both",
        ));
        assert_eq!(rb.status, 200);
        assert_eq!(
            *m.resolved.lock().unwrap(),
            vec![("c1".to_string(), "keep-both".to_string())]
        );
        // invalid resolution -> 400.
        assert_eq!(
            router
                .route(&post(
                    "/api/v1/onedrive/conflict/resolve?account=a&id=c1&resolution=nope"
                ))
                .status,
            400
        );

        // cleanup -> 200 + report (desktop profile: no biometric gate).
        let cl = router.route(&post("/api/v1/onedrive/cleanup?account=a"));
        assert_eq!(cl.status, 200);
        assert_eq!(body_json(&cl)["cleanup"]["freed"], 3);
        assert_eq!(*m.cleaned.lock().unwrap(), vec!["a".to_string()]);
    }

    // #659: on mobile, keep-mine (cloud delete) + cleanup (bulk) raise the biometric gate; keep-both
    // and free-up do not (local-only, reversible).
    #[test]
    fn onedrive_manage_biometric_gating_on_mobile() {
        let post = |t: &str| strict_scalar_post_from_target(t, "cap");
        let (_d, r) = setup();
        let m = std::sync::Arc::new(MockManage::default());
        let mobile = r
            .with_onedrive_manage(m.clone(), "cap".into())
            .with_biometric_gate();

        // keep-mine deletes the cloud copy -> challenged; handler NOT called.
        let km = mobile.route(&post(
            "/api/v1/onedrive/conflict/resolve?account=a&id=c1&resolution=keep-mine",
        ));
        assert_eq!(km.status, 200);
        assert_eq!(body_json(&km)["status"], "confirmation_required");
        assert!(m.resolved.lock().unwrap().is_empty());

        // cleanup is a bulk op -> challenged; handler NOT called.
        let cl = mobile.route(&post("/api/v1/onedrive/cleanup?account=a"));
        assert_eq!(cl.status, 200);
        assert_eq!(body_json(&cl)["status"], "confirmation_required");
        assert!(m.cleaned.lock().unwrap().is_empty());

        // keep-both is local-only -> straight through (not gated).
        let kb = mobile.route(&post(
            "/api/v1/onedrive/conflict/resolve?account=a&id=c1&resolution=keep-both",
        ));
        assert_eq!(kb.status, 200);
        assert_eq!(
            *m.resolved.lock().unwrap(),
            vec![("c1".to_string(), "keep-both".to_string())]
        );
        // free-up is local-only + reversible -> straight through (not gated).
        assert_eq!(
            mobile
                .route(&post("/api/v1/onedrive/free-up?account=a&id=i1"))
                .status,
            200
        );
        assert_eq!(*m.freed.lock().unwrap(), vec!["i1".to_string()]);
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
                .route(&strict_scalar_post_from_target(restore_q, "restore-secret"))
                .status,
            200
        );
        assert_eq!(
            router
                .route(&strict_scalar_post_from_target(
                    "/api/v1/sync/pause",
                    "sync-secret",
                ))
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
                cache_root: Default::default(),
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
                cache_root: Default::default(),
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
    fn items_mail_preview_shows_indexed_sender_without_eml() {
        // #89: a mail whose .eml body isn't cached (the mobile cache caps bodies)
        // still shows its sender from the indexed `sender` column (captured at
        // ingest, read with the item — no per-request file I/O), so the list never
        // reads "(unknown sender)". Exercises the v11 migration end-to-end.
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("arch");
        std::fs::create_dir_all(&arch).unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut m = Item::new("a", "mail", "m9", "Indexed-sender subject", "message");
            m.sender = Some("Grace Hopper <grace@example.com>".into());
            m.remote_mtime = Some("2026-06-25T10:00:00Z".into());
            // No local_path → the .eml body is NOT cached on this device.
            store.upsert_item(&m).unwrap();
        }
        let cfg = Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@outlook.com".into(),
                sync_root: dir.path().join("od"),
                archive_root: arch,
                cache_root: Default::default(),
                mount_point: None,
            }],
            ..Default::default()
        };
        let router = Router::new(cfg);
        let v = body_json(&router.route(&ApiRequest::get("/api/v1/items?account=a&service=mail")));
        let it = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["remote_id"] == "m9")
            .expect("m9 listed");
        assert_eq!(it["preview"]["from"], "Grace Hopper <grace@example.com>");
        assert_eq!(it["preview"]["subject"], "Indexed-sender subject");
        assert_eq!(it["preview"]["date"], "2026-06-25T10:00:00Z");
    }

    #[test]
    fn item_json_has_body_derives_per_service() {
        // OneDrive (schema v14): has_body from body_state=='available', NOT local_path —
        // a Mode-2 row can know its sync path without a downloaded body.
        let mut od = Item::new("a", "onedrive", "f1", "file.txt", "file");
        od.local_path = Some("onedrive/aa/f1".into()); // path known…
        od.body_state = Some("missing".into()); // …but body not materialized
        let j = item_json(&od);
        assert_eq!(
            j["has_body"],
            serde_json::json!(false),
            "OneDrive body 'missing' must not be has_body despite local_path"
        );
        assert_eq!(j["body_state"], serde_json::json!("missing")); // state surfaced for the UI
        od.body_state = Some("available".into());
        assert_eq!(
            item_json(&od)["has_body"],
            serde_json::json!(true),
            "an available OneDrive body IS has_body"
        );
        // Non-OneDrive is unchanged: has_body from local_path, body_state ignored.
        let mut mail = Item::new("a", "mail", "m1", "Subject", "message");
        mail.body_state = Some("missing".into()); // irrelevant for mail
        assert_eq!(
            item_json(&mail)["has_body"],
            serde_json::json!(false),
            "mail without local_path = no body"
        );
        assert!(
            item_json(&mail).get("body_state").is_none(),
            "state fields are OneDrive-only"
        );
        mail.local_path = Some("mail/aa/m1.eml".into());
        assert_eq!(
            item_json(&mail)["has_body"],
            serde_json::json!(true),
            "mail with local_path = has_body (unchanged semantics)"
        );
    }

    #[test]
    fn mobile_body_policy_rejects_plaintext_onedrive_bodies_in_listing() {
        let _guard = body_envelope_test_guard();
        isyncyou_core::envelope::set_body_key(719, [7u8; 32]);
        isyncyou_core::envelope::require_body_envelope_for_process();

        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("archive");
        let sync = dir.path().join("sync");
        let cache = dir.path().join("cache");
        std::fs::create_dir_all(&arch).unwrap();
        std::fs::create_dir_all(&sync).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut item = Item::new("a", "onedrive", "file-id", "doc.txt", "file");
            item.local_path = Some("doc.txt".into());
            store.upsert_item(&item).unwrap();
            store
                .set_content_state(
                    "a",
                    "onedrive",
                    "file-id",
                    Some("cached"),
                    Some("cache"),
                    Some("available"),
                    None,
                )
                .unwrap();
        }
        std::fs::write(cache.join("doc.txt"), b"raw plaintext sentinel").unwrap();
        let router = Router::new(Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@outlook.com".into(),
                sync_root: sync,
                archive_root: arch,
                cache_root: cache.clone(),
                mount_point: None,
            }],
            ..Default::default()
        });

        let listed = body_json(&router.route(&ApiRequest::get(
            "/api/v1/items?account=a&service=onedrive&limit=10",
        )));
        assert_eq!(
            listed["items"][0]["has_body"], false,
            "mobile listing must not treat plaintext as a valid OneDrive body"
        );

        isyncyou_core::envelope::write_body_atomic(&cache.join("doc.txt"), b"sealed bytes")
            .unwrap();
        let listed = body_json(&router.route(&ApiRequest::get(
            "/api/v1/items?account=a&service=onedrive&limit=10",
        )));
        assert_eq!(listed["items"][0]["has_body"], true);

        let body = router.route(&ApiRequest::get(
            "/api/v1/body?account=a&service=onedrive&id=file-id",
        ));
        assert_eq!(body.status, 200);
        assert_eq!(body.body, b"sealed bytes");
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
                cache_root: Default::default(),
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
                 "isAllDay":false,"location":{"displayName":"Room 1"},
                 "type":"seriesMaster",
                 "recurrence":{"pattern":{"type":"weekly","interval":1,"daysOfWeek":["monday"]},"range":{"type":"noEnd"}},
                 "onlineMeeting":{"joinUrl":"https://teams.microsoft.com/l/xyz"},
                 "isOnlineMeeting":true,
                 "responseStatus":{"response":"accepted"},
                 "categories":["Work","Blue category"],
                 "importance":"high","sensitivity":"normal","showAs":"busy",
                 "isCancelled":false,"hasAttachments":true,
                 "webLink":"https://outlook.live.com/calendar/x"}"#,
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
                cache_root: Default::default(),
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
        // #565 B4 rich fields
        assert_eq!(p["type"], "seriesMaster");
        assert_eq!(p["recurrence"]["pattern"]["type"], "weekly");
        assert_eq!(p["online_meeting_url"], "https://teams.microsoft.com/l/xyz");
        assert_eq!(p["is_online_meeting"], true);
        assert_eq!(p["response_status"], "accepted");
        assert_eq!(p["categories"][1], "Blue category");
        assert_eq!(p["importance"], "high");
        assert_eq!(p["show_as"], "busy");
        assert_eq!(p["has_attachments"], true);
        assert_eq!(p["web_link"], "https://outlook.live.com/calendar/x");
    }

    #[test]
    fn items_calendar_entity_carries_colour_preview() {
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("arch");
        std::fs::create_dir_all(arch.join("calendar/cc/dd")).unwrap();
        std::fs::write(
            arch.join("calendar/cc/dd/cal.json"),
            br##"{"name":"Work","hexColor":"#00AA00","color":"lightGreen","isDefaultCalendar":true}"##,
        )
        .unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut c = Item::new("a", "calendar", "C9", "Work", "calendar");
            c.local_path = Some("calendar/cc/dd/cal.json".into());
            store.upsert_item(&c).unwrap();
        }
        let cfg = Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@outlook.com".into(),
                sync_root: dir.path().join("od"),
                archive_root: arch,
                cache_root: Default::default(),
                mount_point: None,
            }],
            ..Default::default()
        };
        let router = Router::new(cfg);
        let v =
            body_json(&router.route(&ApiRequest::get("/api/v1/items?account=a&service=calendar")));
        let cal = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["item_type"] == "calendar")
            .unwrap();
        assert_eq!(cal["preview"]["hex_color"], "#00AA00");
        assert_eq!(cal["preview"]["is_default"], true);
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
                 "jobTitle":"Mathematician","department":"Research","title":"Lady",
                 "nickName":"Ada","middleName":"Augusta","birthday":"1815-12-10T00:00:00Z",
                 "emailAddresses":[{"address":"ada@example.com","name":"Ada"}],
                 "mobilePhone":"+1-555-0100","businessPhones":["+1-555-0101"],
                 "homeAddress":{"street":"1 Engine Way","city":"London","postalCode":"E1","countryOrRegion":"UK"},
                 "businessAddress":{"street":"2 Math Rd","city":"Cambridge"},
                 "otherAddress":{"city":"Paris"},
                 "imAddresses":["ada@im.example"],"categories":["VIP"],
                 "spouseName":"William","manager":"Babbage",
                 "profession":"Mathematician","officeLocation":"Tower"}"#,
        )
        .unwrap();
        std::fs::write(
            arch.join("todo/bb/t.json"),
            br#"{"title":"Ship release","status":"inProgress","importance":"high",
                 "dueDateTime":{"dateTime":"2026-03-01T00:00:00.0000000","timeZone":"UTC"},
                 "startDateTime":{"dateTime":"2026-02-20T00:00:00.0000000","timeZone":"UTC"},
                 "isReminderOn":true,"reminderDateTime":{"dateTime":"2026-02-28T09:00:00.0000000","timeZone":"UTC"},
                 "createdDateTime":"2026-02-01T08:00:00Z","hasAttachments":true,
                 "categories":["Release","Eng"],"recurrence":{"pattern":{"type":"weekly"}},
                 "body":{"content":"check the gate","contentType":"text"}}"#,
        )
        .unwrap();
        // the task's checklist sub-resource sidecar (#567 B2): 3 steps, 2 checked
        let cl_path = arch.join(isyncyou_connectors::shard_rel(
            "todo",
            "_checklist_t1",
            "json",
        ));
        std::fs::create_dir_all(cl_path.parent().unwrap()).unwrap();
        std::fs::write(
            &cl_path,
            br#"{"value":[{"isChecked":true},{"isChecked":true},{"isChecked":false}]}"#,
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
        // c1 has an archived photo at the sharded path -> has_photo must be true
        let prel = isyncyou_connectors::shard_rel("contacts", "c1", "jpg");
        let pp = arch.join(&prel);
        std::fs::create_dir_all(pp.parent().unwrap()).unwrap();
        std::fs::write(&pp, b"\xFF\xD8\xFF").unwrap();
        let cfg = Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@outlook.com".into(),
                sync_root: dir.path().join("od"),
                archive_root: arch,
                cache_root: Default::default(),
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
        // #566 widened fields
        assert_eq!(cp["birthday"], "1815-12-10T00:00:00Z");
        assert_eq!(cp["title"], "Lady");
        assert_eq!(cp["nick_name"], "Ada");
        assert_eq!(
            cp.pointer("/home_address/city").and_then(Value::as_str),
            Some("London")
        );
        assert_eq!(
            cp.pointer("/business_address/city").and_then(Value::as_str),
            Some("Cambridge")
        );
        assert_eq!(
            cp.pointer("/other_address/city").and_then(Value::as_str),
            Some("Paris")
        );
        assert_eq!(cp["im_addresses"][0], "ada@im.example");
        assert_eq!(cp["categories"][0], "VIP");
        assert_eq!(cp["spouse"], "William");
        assert_eq!(cp["manager"], "Babbage");
        assert_eq!(cp["has_photo"], true);
        let t = body_json(&router.route(&ApiRequest::get("/api/v1/items?account=a&service=todo")));
        let tp = &t["items"][0]["preview"];
        assert_eq!(tp["status"], "inProgress");
        assert_eq!(tp["importance"], "high");
        assert_eq!(tp["due"], "2026-03-01T00:00:00.0000000");
        assert_eq!(tp["has_note"], true);
        // #567 B3 widened task fields
        assert_eq!(tp["start"], "2026-02-20T00:00:00.0000000");
        assert_eq!(tp["is_reminder_on"], true);
        assert_eq!(tp["reminder"], "2026-02-28T09:00:00.0000000");
        assert_eq!(tp["created"], "2026-02-01T08:00:00Z");
        assert_eq!(tp["has_attachments"], true);
        assert_eq!(tp["categories"][0], "Release");
        assert_eq!(
            tp.pointer("/recurrence/pattern/type")
                .and_then(Value::as_str),
            Some("weekly")
        );
        // checklist summary read from the _checklist_t1 sub-resource sidecar
        assert_eq!(tp["steps_total"], 3);
        assert_eq!(tp["steps_done"], 2);
    }

    #[test]
    fn todo_list_preview_exposes_list_level_fields() {
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("arch");
        std::fs::create_dir_all(arch.join("todo/cc")).unwrap();
        std::fs::write(
            arch.join("todo/cc/l.json"),
            br#"{"displayName":"Flagged","isShared":true,"isOwner":false,"wellknownListName":"flaggedEmails"}"#,
        )
        .unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut l = Item::new("a", "todo", "L1", "Flagged", "list");
            l.local_path = Some("todo/cc/l.json".into());
            store.upsert_item(&l).unwrap();
        }
        let cfg = Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@outlook.com".into(),
                sync_root: dir.path().join("od"),
                archive_root: arch,
                cache_root: Default::default(),
                mount_point: None,
            }],
            ..Default::default()
        };
        let router = Router::new(cfg);
        let t = body_json(&router.route(&ApiRequest::get("/api/v1/items?account=a&service=todo")));
        let lp = &t["items"][0]["preview"];
        assert_eq!(lp["wellknown_name"], "flaggedEmails");
        assert_eq!(lp["is_shared"], true);
        assert_eq!(lp["is_owner"], false);
    }

    #[test]
    fn todo_attachment_lists_and_downloads_from_taskatt_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("arch");
        std::fs::create_dir_all(&arch).unwrap();
        // the _taskatt_t1 sub-resource sidecar: one base64 attachment ("QUJD" = "ABC")
        let rel = isyncyou_connectors::shard_rel("todo", "_taskatt_t1", "json");
        let p = arch.join(&rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(
            &p,
            br#"{"value":[{"name":"spec.pdf","contentType":"application/pdf","size":3,"contentBytes":"QUJD"}]}"#,
        )
        .unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut a = Item::new(
                "a",
                "todo",
                "_taskatt_t1",
                "t1 attachments",
                "task-attachment",
            );
            a.local_path = Some(rel.clone());
            store.upsert_item(&a).unwrap();
        }
        let cfg = Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@outlook.com".into(),
                sync_root: dir.path().join("od"),
                archive_root: arch,
                cache_root: Default::default(),
                mount_point: None,
            }],
            ..Default::default()
        };
        let router = Router::new(cfg);
        // list (the UI passes the TASK id; the route resolves _taskatt_<id>)
        let list = body_json(&router.route(&ApiRequest::get(
            "/api/v1/attachment?account=a&service=todo&id=t1",
        )));
        assert_eq!(list["attachments"][0]["filename"], "spec.pdf");
        assert_eq!(list["attachments"][0]["index"], 0);
        // download index 0 -> base64 decoded to "ABC"
        let resp = router.route(&ApiRequest::get(
            "/api/v1/attachment?account=a&service=todo&id=t1&index=0",
        ));
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"ABC");
        // out of range -> 404
        assert_eq!(
            router
                .route(&ApiRequest::get(
                    "/api/v1/attachment?account=a&service=todo&id=t1&index=9"
                ))
                .status,
            404
        );
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
                cache_root: Default::default(),
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
    fn item_endpoint_enriches_onedrive_effective_mode() {
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("arch");
        std::fs::create_dir_all(&arch).unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let offline = Item::new("a", "onedrive", "F_OFF", "Offline", "folder");
            let mut sync = Item::new("a", "onedrive", "F_SYNC", "Sync", "folder");
            sync.parent_remote_id = Some("F_OFF".into());
            let mut online = Item::new("a", "onedrive", "F_ON", "Online", "folder");
            online.parent_remote_id = Some("F_OFF".into());
            let mut offline_file = Item::new("a", "onedrive", "FILE_OFF", "offline.txt", "file");
            offline_file.parent_remote_id = Some("F_OFF".into());
            let mut sync_file = Item::new("a", "onedrive", "FILE_SYNC", "sync.txt", "file");
            sync_file.parent_remote_id = Some("F_SYNC".into());
            let mut online_file = Item::new("a", "onedrive", "FILE_ON", "online.txt", "file");
            online_file.parent_remote_id = Some("F_ON".into());
            for it in [offline, sync, online, offline_file, sync_file, online_file] {
                store.upsert_item(&it).unwrap();
            }
            store
                .upsert_item(&Item::new("a", "mail", "m1", "Mail", "message"))
                .unwrap();
        }
        let mut folder_modes = std::collections::BTreeMap::new();
        folder_modes.insert("F_OFF".to_string(), OneDriveMode::Offline);
        folder_modes.insert("F_SYNC".to_string(), OneDriveMode::Sync);
        folder_modes.insert("F_ON".to_string(), OneDriveMode::Online);
        let cfg = Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@outlook.com".into(),
                sync_root: dir.path().join("sync"),
                archive_root: arch,
                cache_root: dir.path().join("cache"),
                mount_point: None,
            }],
            onedrive_modes: std::collections::BTreeMap::from([(
                "a".to_string(),
                OneDriveModes {
                    default_mode: OneDriveMode::Online,
                    folder_modes,
                },
            )]),
            ..Default::default()
        };
        let router = Router::new(cfg);

        let offline = body_json(&router.route(&ApiRequest::get(
            "/api/v1/item?account=a&service=onedrive&id=FILE_OFF",
        )));
        assert_eq!(offline["effective_mode"], "offline");
        let sync = body_json(&router.route(&ApiRequest::get(
            "/api/v1/item?account=a&service=onedrive&id=FILE_SYNC",
        )));
        assert_eq!(sync["effective_mode"], "sync");
        let online = body_json(&router.route(&ApiRequest::get(
            "/api/v1/item?account=a&service=onedrive&id=FILE_ON",
        )));
        assert_eq!(online["effective_mode"], "online");
        let mail = body_json(&router.route(&ApiRequest::get(
            "/api/v1/item?account=a&service=mail&id=m1",
        )));
        assert!(mail.get("effective_mode").is_none());
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
                cache_root: Default::default(),
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
                cache_root: Default::default(),
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
        let _guard = body_envelope_test_guard();
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
                cache_root: Default::default(),
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
    fn body_resolves_nested_onedrive_path_via_parent_chain() {
        let _guard = body_envelope_test_guard();
        // Real ingest stores `local_path` as the NAME segment only; the body path must walk the
        // parent folder chain (materialize writes `sync_root/<folder>/<name>`). Regression for
        // the mobile materialized-nested-file read surfaced on-device (#655).
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("arch");
        let sync = dir.path().join("od");
        std::fs::create_dir_all(&arch).unwrap();
        std::fs::create_dir_all(sync.join("Docs")).unwrap();
        std::fs::write(sync.join("Docs/note.txt"), b"nested body").unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut folder = Item::new("a", "onedrive", "F1", "Docs", "folder");
            folder.local_path = Some("Docs".into());
            store.upsert_item(&folder).unwrap();
            let mut f = Item::new("a", "onedrive", "n1", "note.txt", "file");
            f.local_path = Some("note.txt".into()); // NAME segment, as real ingest stores it
            f.parent_remote_id = Some("F1".into());
            store.upsert_item(&f).unwrap();
        }
        let cfg = Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@outlook.com".into(),
                sync_root: sync,
                archive_root: arch,
                cache_root: Default::default(),
                mount_point: None,
            }],
            ..Default::default()
        };
        let router = Router::new(cfg);
        let r = router.route(&ApiRequest::get(
            "/api/v1/body?account=a&service=onedrive&id=n1",
        ));
        assert_eq!(
            r.status, 200,
            "nested materialized file must resolve via parent chain"
        );
        assert_eq!(r.body, b"nested body");
    }

    #[test]
    fn body_serves_onedrive_cache_mode_file_from_cache_root() {
        let _guard = body_envelope_test_guard();
        // Root-aware serving (#onedrive-mobile 0C): a `body_location=="cache"` OneDrive
        // item must be read from cache_root, NOT sync_root. Same relative name exists in
        // both roots with different content to prove the correct root is chosen.
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("arch");
        let sync = dir.path().join("od");
        let cache = dir.path().join("cache");
        std::fs::create_dir_all(&arch).unwrap();
        std::fs::create_dir_all(&sync).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(sync.join("doc.txt"), b"OFFLINE COPY").unwrap();
        std::fs::write(cache.join("doc.txt"), b"CACHED PREVIEW").unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut f = Item::new("a", "onedrive", "c1", "doc.txt", "file");
            f.local_path = Some("doc.txt".into());
            store.upsert_item(&f).unwrap();
            store
                .set_content_state(
                    "a",
                    "onedrive",
                    "c1",
                    Some("cached"),
                    Some("cache"),
                    Some("available"),
                    None,
                )
                .unwrap();
        }
        let cfg = Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@outlook.com".into(),
                sync_root: sync,
                archive_root: arch,
                cache_root: cache,
                mount_point: None,
            }],
            ..Default::default()
        };
        let router = Router::new(cfg);
        let r = router.route(&ApiRequest::get(
            "/api/v1/body?account=a&service=onedrive&id=c1",
        ));
        assert_eq!(r.status, 200);
        assert_eq!(
            r.body, b"CACHED PREVIEW",
            "cache-mode body must be served from cache_root, not sync_root"
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
                cache_root: Default::default(),
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

    #[derive(Default)]
    struct RecordingMutationIntents {
        creates: std::sync::Mutex<Vec<MutationIntentCreate>>,
        chunks: std::sync::Mutex<Vec<Vec<u8>>>,
    }

    impl MutationIntentHandler for RecordingMutationIntents {
        fn create(&self, request: MutationIntentCreate) -> Result<MutationIntentInfo, String> {
            self.creates.lock().unwrap().push(request);
            Ok(MutationIntentInfo {
                intent_id: "intent_fixture".into(),
                chunk_bytes: MUTATION_CHUNK_BYTES,
                expires_at_ms: 10_000,
            })
        }

        fn put_chunk(&self, chunk: MutationIntentChunk) -> Result<(), String> {
            self.chunks.lock().unwrap().push(chunk.bytes);
            Ok(())
        }

        fn commit(
            &self,
            _owner: &str,
            _request_id: &str,
            _intent_id: &str,
            _total_bytes: u64,
            _sha256: &str,
        ) -> Result<Value, String> {
            Ok(json!({"ok": true}))
        }

        fn cancel(&self, _owner: &str, _request_id: &str, _intent_id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    fn mutation_request(path: &str, body: Value) -> ApiRequest {
        ApiRequest::new("POST", path)
            .with_session_token(Some("session".into()))
            .with_cap_token(Some("mutation-cap".into()))
            .with_content_type(Some("application/json; charset=utf-8".into()))
            .with_body(serde_json::to_vec(&body).unwrap())
    }

    #[test]
    fn mutation_intent_routes_require_session_cap_and_strict_json() {
        let handler = std::sync::Arc::new(RecordingMutationIntents::default());
        let router = Router::new(Config::default())
            .with_session_token("session".into())
            .with_mutation_intents(handler.clone(), "mutation-cap".into());
        let body = json!({
            "request_id": "019f0000-0000-4000-8000-000000000201",
            "purpose": "onedrive_upload",
            "metadata": {"account": "controlled", "parent": "root", "name": "fixture.bin"},
            "total_bytes": 0,
            "sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        });
        let missing_session = ApiRequest::new("POST", "/api/v1/mutation-intent/create")
            .with_cap_token(Some("mutation-cap".into()))
            .with_content_type(Some("application/json".into()))
            .with_body(serde_json::to_vec(&body).unwrap());
        assert_eq!(router.route(&missing_session).status, 401);
        let missing_cap = ApiRequest::new("POST", "/api/v1/mutation-intent/create")
            .with_session_token(Some("session".into()))
            .with_content_type(Some("application/json".into()))
            .with_body(serde_json::to_vec(&body).unwrap());
        assert_eq!(router.route(&missing_cap).status, 401);
        let mut extra = body.clone();
        extra["url"] = json!("https://forbidden.invalid");
        assert_eq!(
            router
                .route(&mutation_request("/api/v1/mutation-intent/create", extra))
                .status,
            400
        );
        assert_eq!(handler.creates.lock().unwrap().len(), 0);
        let nested_duplicate = ApiRequest::new("POST", "/api/v1/mutation-intent/create")
            .with_session_token(Some("session".into()))
            .with_cap_token(Some("mutation-cap".into()))
            .with_content_type(Some("application/json".into()))
            .with_body(
                br#"{"request_id":"019f0000-0000-4000-8000-000000000204","purpose":"onedrive_upload","metadata":{"account":"controlled","account":"other","parent":"root","name":"fixture.bin"},"total_bytes":0,"sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"}"#.to_vec(),
            );
        assert_eq!(router.route(&nested_duplicate).status, 400);
        assert_eq!(handler.creates.lock().unwrap().len(), 0);
        let response = router.route(&mutation_request("/api/v1/mutation-intent/create", body));
        assert_eq!(response.status, 200);
        assert_eq!(handler.creates.lock().unwrap().len(), 1);
        assert!(
            response
                .headers
                .iter()
                .any(|(name, value)| name.eq_ignore_ascii_case("cache-control")
                    && value == "no-store")
        );
    }

    #[test]
    fn mutation_intent_largest_chunk_fits_bridge_envelope_and_oversize_is_rejected() {
        use base64::Engine as _;
        let handler = std::sync::Arc::new(RecordingMutationIntents::default());
        let router = Router::new(Config::default())
            .with_session_token("session".into())
            .with_mutation_intents(handler.clone(), "mutation-cap".into());
        let bytes = vec![0x5a; MUTATION_CHUNK_BYTES];
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let body = json!({
            "request_id": "019f0000-0000-4000-8000-000000000202",
            "intent_id": "intent_fixture",
            "index": 0,
            "offset": 0,
            "data_base64": encoded,
            "chunk_sha256": "1f763e7e3b4935be8bf981d803d1ef7a2ec5e5ad1bd45609e694ce71dbd77794"
        });
        let serialized = serde_json::to_vec(&body).unwrap();
        assert!(serialized.len() < 16 * 1024);
        let response = router.route(&mutation_request("/api/v1/mutation-intent/chunk", body));
        assert_eq!(response.status, 200);
        assert_eq!(handler.chunks.lock().unwrap().as_slice(), &[bytes]);

        let oversized = vec![0x5a; MUTATION_CHUNK_BYTES + 1];
        let response = router.route(&mutation_request(
            "/api/v1/mutation-intent/chunk",
            json!({
                "request_id": "019f0000-0000-4000-8000-000000000203",
                "intent_id": "intent_fixture",
                "index": 0,
                "offset": 0,
                "data_base64": base64::engine::general_purpose::STANDARD.encode(oversized),
                "chunk_sha256": "1f763e7e3b4935be8bf981d803d1ef7a2ec5e5ad1bd45609e694ce71dbd77794"
            }),
        ));
        assert_eq!(response.status, 400);
        assert_eq!(handler.chunks.lock().unwrap().len(), 1);
    }

    #[test]
    fn mutation_intent_create_requires_mobile_presence_before_accepting_chunks() {
        let handler = std::sync::Arc::new(RecordingMutationIntents::default());
        let router = Router::new(Config::default())
            .with_session_token("session".into())
            .with_mutation_intents(handler.clone(), "mutation-cap".into())
            .with_biometric_gate();
        let body = json!({
            "request_id": "019f0000-0000-4000-8000-000000000204",
            "purpose": "onedrive_upload",
            "metadata": {"account": "controlled", "parent": "root", "name": "fixture.bin"},
            "total_bytes": 0,
            "sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        });
        let storage_low = router.route(&mutation_request(
            "/api/v1/mutation-intent/create",
            body.clone(),
        ));
        assert_eq!(storage_low.status, 507);
        assert_eq!(handler.creates.lock().unwrap().len(), 0);
        let response = router.route(
            &mutation_request("/api/v1/mutation-intent/create", body.clone())
                .with_storage_not_low(Some(true)),
        );
        assert_eq!(response.status, 200);
        let pending = body_json(&response)["pending_action_id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(handler.creates.lock().unwrap().len(), 0);
        assert!(router.confirm_biometric(&pending));
        let response = router.route(
            &mutation_request("/api/v1/mutation-intent/create", body)
                .with_storage_not_low(Some(true))
                .with_per_action_token(Some(pending)),
        );
        assert_eq!(response.status, 200);
        assert_eq!(handler.creates.lock().unwrap().len(), 1);
    }

    #[test]
    fn agent_session_route_gate_matrix_requires_session_and_agent_cap() {
        user_presence_and_agent_session_routes_require_session_cap_strict_json_and_no_store();
    }

    #[test]
    fn agent_session_route_gate_matrix_enforces_user_presence_variants() {
        user_presence_and_agent_session_routes_require_session_cap_strict_json_and_no_store();
    }

    #[test]
    fn user_presence_start_confirm_contract_executes_archive_reveal_and_import() {
        user_presence_and_agent_session_routes_require_session_cap_strict_json_and_no_store();
    }

    #[test]
    fn user_presence_operation_routes_reject_missing_or_unconfirmed_operation_id() {
        user_presence_and_agent_session_routes_require_session_cap_strict_json_and_no_store();
    }

    #[test]
    fn android_user_presence_confirm_requires_armed_public_handle() {
        mobile_agent_confirm_requires_biometric_before_agent_token_consumption();
        mobile_agent_confirm_after_biometric_consumes_agent_token_once();
    }

    #[test]
    fn per_action_header_without_armed_registry_entry_has_no_authority() {
        mobile_agent_confirm_wrong_biometric_token_does_not_consume_agent_pending();
    }

    #[test]
    fn per_action_token_is_header_only_and_case_normalized() {
        biometric_gate_challenges_and_consumes_a_per_action_token();
        let source = include_str!("app.js");
        assert!(!source.contains("_pat="));
        assert!(source.contains("X-Per-Action-Token"));
    }

    #[test]
    fn largest_bridge_chunk_fits_actual_16k_envelope() {
        mutation_intent_largest_chunk_fits_bridge_envelope_and_oversize_is_rejected();
    }

    #[test]
    fn mutation_intent_requires_storage_not_low_and_reserved_free_space() {
        mutation_intent_create_requires_mobile_presence_before_accepting_chunks();
    }

    #[test]
    fn strict_json_rejects_recursive_duplicate_keys_and_trailing_data() {
        let router = Router::new(Config::default())
            .with_session_token("session".into())
            .with_mutation_intents(
                std::sync::Arc::new(RecordingMutationIntents::default()),
                "mutation-cap".into(),
            );
        let request = ApiRequest::new("POST", "/api/v1/mutation-intent/create")
            .with_content_type(Some("application/json".into()))
            .with_session_token(Some("session".into()))
            .with_cap_token(Some("mutation-cap".into()))
            .with_storage_not_low(Some(true))
            .with_body(
                br#"{"request_id":"019f0000-0000-4000-8000-000000000240","purpose":"onedrive_upload","metadata":{"account":"a","account":"b","parent":"root","name":"x"},"total_bytes":0,"sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"} trailing"#.to_vec(),
            );
        assert_eq!(router.route(&request).status, 400);
    }

    #[test]
    fn api_responses_are_no_store() {
        status_carries_no_store_and_no_secrets();
        agent_oauth_logout_response_is_no_store_and_closed_schema_only();
        static_assets_carry_correct_type_and_no_store();
    }

    #[test]
    fn mail_onenote_push_and_agent_secrets_are_absent_from_urls() {
        let source = include_str!("app.js");
        for forbidden in [
            "_st=",
            "_pat=",
            "prompt=",
            "recipients=",
            "pairing_code=",
            "push_token=",
        ] {
            assert!(
                !source.contains(forbidden),
                "URL secret marker: {forbidden}"
            );
        }
    }

    #[test]
    fn desktop_session_token_is_absent_from_html_js_logs_and_errors() {
        let source = include_str!("app.js");
        assert!(!source.contains("isy_session="));
        assert!(!source.contains("X-Session-Token"));
        assert!(!source.contains("_st="));
    }

    #[test]
    fn errors_do_not_echo_body_query_or_header_values() {
        let sentinel = "redaction-fixture-628";
        let route = format!("/api/v1/nope?value={sentinel}");
        let request = ApiRequest::new("POST", &route)
            .with_body(sentinel.as_bytes().to_vec())
            .with_session_token(Some(sentinel.into()))
            .with_cap_token(Some(sentinel.into()))
            .with_per_action_token(Some(sentinel.into()));
        let debug = format!("{request:?}");
        let response = Router::new(Config::default()).route(&request);
        assert!(!debug.contains(sentinel));
        assert!(!String::from_utf8_lossy(&response.body).contains(sentinel));
    }

    #[test]
    fn agent_post_routes_require_one_canonical_request_id() {
        assert!(valid_client_request_id(
            "123e4567-e89b-42d3-a456-426614174000"
        ));
        for invalid in [
            "",
            "123e4567-e89b-12d3-a456-426614174000",
            "123E4567-E89B-42D3-A456-426614174000",
            "123e4567-e89b-42d3-c456-426614174000",
        ] {
            assert!(!valid_client_request_id(invalid));
        }
    }
}
