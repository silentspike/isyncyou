//! JMAP mail transport. Store checkpoints and mutation intents belong to the caller.

use reqwest::blocking::{Client, Response};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use crate::request_budget::{account_gate, origin_gate, Gate};
use std::time::Duration;
use url::Url;

const CORE: &str = "urn:ietf:params:jmap:core";
const MAIL: &str = "urn:ietf:params:jmap:mail";
const MAIL_SHARE: &str = "urn:ietf:params:jmap:mail:share";
const SUBMISSION: &str = "urn:ietf:params:jmap:submission";
const VAPID: &str = "urn:ietf:params:jmap:webpush-vapid";
const MAX_JSON: u64 = 16 * 1024 * 1024;
const MAX_BLOB: u64 = 64 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum JmapError {
    InvalidUrl,
    InvalidResponse,
    Unsupported,
    Http(u16),
    Method(String),
    Transport,
    /// The request may have reached the server. Never blindly repeat a mutation.
    OutcomeUnknown,
}

impl std::fmt::Display for JmapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for JmapError {}

/// Native server response for the single requested submission.
#[derive(Debug, PartialEq, Eq)]
pub enum SubmissionResult {
    Created {
        id: String,
        email_update: EmailUpdateResult,
    },
    Rejected {
        kind: String,
    },
}
#[derive(Debug, PartialEq, Eq)]
pub enum EmailUpdateResult {
    Applied,
    Rejected,
    Unknown,
}

/// Session state must be compared to a fresh discovery after registration.
pub struct PushMutation {
    pub session_state: String,
    pub outcome: PushOutcome,
}
pub enum PushOutcome {
    Applied { id: String, expires: Option<String> },
    Rejected { kind: String },
}

pub enum MailboxCreateResult {
    Created { id: String },
    Rejected { kind: String },
}

/// Read-only API-key client for a local exporter. No writable client is exposed.
pub struct LoopbackJmapReader(JmapClient);
impl LoopbackJmapReader {
    pub fn discover(origin: &str, api_key: String, trusted_public_origin: &str) -> Result<Self, JmapError> {
        JmapClient::discover_inner(origin, api_key, Some(trusted_public_origin)).map(Self)
    }
    pub fn account_id(&self) -> &str { self.0.account_id() }
    pub fn max_objects_in_get(&self) -> usize { self.0.max_objects_in_get() }
    pub fn get(&self, kind: &str, ids: Option<&[String]>, properties: Option<&[String]>) -> Result<Value, JmapError> {
        if !matches!(kind, "Email" | "Mailbox") { return Err(JmapError::Unsupported); }
        self.0.get(kind, ids, properties)
    }
    pub fn query_emails(&self, position: usize, limit: usize, filter: Option<Value>) -> Result<Value, JmapError> { self.0.query_emails(position, limit, filter) }
    pub fn query_mailboxes(&self, position: usize, limit: usize) -> Result<Value, JmapError> { self.0.query_mailboxes(position, limit) }
    pub fn changes(&self, kind: &str, since: &str, max: usize) -> Result<Value, JmapError> { self.0.changes(kind, since, max) }
    pub fn download(&self, blob_id: &str, name: &str, media_type: &str) -> Result<Vec<u8>, JmapError> { self.0.download(blob_id, name, media_type) }
}

fn body(response: Response, max: u64) -> Result<Vec<u8>, JmapError> {
    let mut bytes = Vec::new();
    response
        .take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| JmapError::Transport)?;
    if bytes.len() as u64 > max {
        return Err(JmapError::InvalidResponse);
    }
    Ok(bytes)
}
fn endpoint(origin: &Url, value: &str) -> Result<Url, JmapError> {
    let url = origin.join(value).map_err(|_| JmapError::InvalidUrl)?;
    if url.origin() != origin.origin()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(JmapError::InvalidUrl);
    }
    Ok(url)
}

/// Discovery alone may follow at most three same-origin redirects. The HTTP
/// client itself never follows redirects, so a bearer token cannot be sent to
/// an unvalidated target. Loopback exporters may map only their enrolled public
/// origin back to the same local listener.
fn fetch_session(http: &Client, origin: &Url, token: &str, public: Option<&Url>) -> Result<Response, JmapError> {
    let mut current = endpoint(origin, "/.well-known/jmap")?;
    let mut visited = std::collections::HashSet::new();
    for redirects in 0..=3 {
        if !visited.insert(current.clone()) { return Err(JmapError::InvalidUrl); }
        let response = http.get(current.clone()).bearer_auth(token).send().map_err(|_| JmapError::Transport)?;
        if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) { return Ok(response); }
        if redirects == 3 { return Err(JmapError::InvalidUrl); }
        let location = response.headers().get(reqwest::header::LOCATION).and_then(|value| value.to_str().ok()).ok_or(JmapError::InvalidUrl)?;
        let target = current.join(location).map_err(|_| JmapError::InvalidUrl)?;
        if !target.username().is_empty() || target.password().is_some() || target.fragment().is_some() { return Err(JmapError::InvalidUrl); }
        let mapped = match public {
            Some(public) if target.origin() == public.origin() => rewrite_loopback_endpoint(origin, public, target.as_str())?,
            _ => target.to_string(),
        };
        current = endpoint(origin, &mapped)?;
    }
    Err(JmapError::InvalidUrl)
}

fn event_source_endpoint(origin: &Url, template: &str) -> Result<Url, JmapError> {
    if template.len() > 8192 || ["{types}","{closeafter}","{ping}"].iter().any(|part| template.matches(part).count() != 1) { return Err(JmapError::InvalidUrl); }
    let value = template.replace("{types}", "Email%2CMailbox%2CCalendar%2CCalendarEvent%2CAddressBook%2CContactCard").replace("{closeafter}", "no").replace("{ping}", "30");
    if value.contains('{') || value.contains('}') { return Err(JmapError::InvalidUrl); }
    endpoint(origin, &value)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventDomains { pub mail: bool, pub dav: bool }

fn state_change_domains(data: &str, _account: &str) -> Result<EventDomains, JmapError> {
    let value: Value = serde_json::from_str(data).map_err(|_| JmapError::InvalidResponse)?;
    if value["@type"].as_str() != Some("StateChange") { return Err(JmapError::InvalidResponse); }
    let changed = value["changed"].as_object().ok_or(JmapError::InvalidResponse)?;
    let mut domains = EventDomains { mail:false, dav:false };
    for (_changed_account, types) in changed {
        let types = types.as_object().ok_or(JmapError::InvalidResponse)?;
        for name in ["Email","Mailbox","Calendar","CalendarEvent","AddressBook","ContactCard"] {
            if let Some(state) = types.get(name) {
                if state.as_str().is_none_or(str::is_empty) { return Err(JmapError::InvalidResponse); }
                if matches!(name, "Email" | "Mailbox") { domains.mail = true; } else { domains.dav = true; }
            }
        }
    }
    Ok(domains)
}

fn consume_mail_event_stream(reader: &mut impl BufRead, account: &str, stop: &AtomicBool, on_change: &mut impl FnMut(EventDomains) -> Result<(), ()>) -> Result<(), JmapError> {
    let mut event = String::new(); let mut data = String::new();
    while !stop.load(Ordering::Relaxed) {
        let mut line = Vec::new();
        let n = (&mut *reader).take(65_537).read_until(b'\n', &mut line).map_err(|_| JmapError::Transport)?;
        if n == 0 { return Ok(()); }
        if line.len() > 65_536 { return Err(JmapError::InvalidResponse); }
        let line = std::str::from_utf8(&line).map_err(|_| JmapError::InvalidResponse)?.trim_end_matches(['\r','\n']);
        if line.is_empty() {
            if event == "state" {
                let domains = state_change_domains(&data, account)?;
                if domains.mail || domains.dav { on_change(domains).map_err(|_| JmapError::Transport)?; }
            } else if event == "ping" {
                let ping: Value = serde_json::from_str(&data).map_err(|_| JmapError::InvalidResponse)?;
                if ping["interval"].as_u64().is_none_or(|interval| interval == 0 || interval > 3600) { return Err(JmapError::InvalidResponse); }
                on_change(EventDomains {mail:false,dav:false}).map_err(|_| JmapError::Transport)?;
            }
            event.clear(); data.clear(); continue;
        }
        if line.starts_with(':') { continue; }
        let (field,value) = line.split_once(':').unwrap_or((line,""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = value.to_owned(),
            "data" => { if !data.is_empty() { data.push('\n'); } data.push_str(value); if data.len()>65_536 { return Err(JmapError::InvalidResponse); } },
            _ => {}
        }
    }
    Ok(())
}
fn allowed_origin(origin: &Url, loopback: bool) -> bool {
    if loopback {
        origin.scheme() == "http" && (matches!(origin.host(), Some(url::Host::Ipv4(ip)) if ip == std::net::Ipv4Addr::LOCALHOST)
            || matches!(origin.host(), Some(url::Host::Ipv6(ip)) if ip == std::net::Ipv6Addr::LOCALHOST))
    } else { origin.scheme() == "https" }
}
fn rewrite_loopback_endpoint(local: &Url, public: &Url, value: &str) -> Result<String, JmapError> {
    let vars = [("{accountId}", "JMAPVARACCOUNT"), ("{blobId}", "JMAPVARBLOB"), ("{name}", "JMAPVARNAME"), ("{type}", "JMAPVARTYPE")];
    let mut encoded = value.to_owned();
    for (var, marker) in vars { if encoded.contains(marker) { return Err(JmapError::InvalidUrl); } encoded = encoded.replace(var, marker); }
    let url = local.join(&encoded).map_err(|_| JmapError::InvalidUrl)?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() || (url.origin() != local.origin() && url.origin() != public.origin()) { return Err(JmapError::InvalidUrl); }
    let mut mapped = local.clone();
    mapped.set_path(url.path());
    mapped.set_query(url.query());
    let mut result = mapped.to_string();
    for (var, marker) in vars { result = result.replace(marker, var); }
    Ok(result)
}
fn nonempty(value: Option<&Value>) -> Result<&str, JmapError> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(JmapError::InvalidResponse)
}

/// Credentials are private and deliberately excluded from Debug and error text.
pub struct JmapClient {
    http: Client,
    origin: Url,
    token: String,
    account: String,
    primary_account: String,
    mail_accounts: Vec<(String, bool)>,
    read_only: bool,
    api: Url,
    download: String,
    upload: String,
    event_source: Option<Url>,
    submission: bool,
    mail_share: bool,
    gate: Arc<Gate>,
    max_get: usize,
    max_request: usize,
    max_upload: u64,
    session_state: String,
    vapid_public_key: Option<String>,
    metrics: Mutex<JmapSyncMetrics>,
    sync_failure: Arc<Mutex<Option<JmapSyncFailure>>>,
}

/// Bounded per-client diagnostic; never stores identifiers or server text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JmapSyncFailure {
    pub operation: &'static str,
    pub class: &'static str,
    pub http_status: Option<u16>,
    pub method_type: Option<&'static str>,
}

/// Per-client aggregate counters; never contains account identifiers or payloads.
#[derive(Clone, Copy, Default)]
pub struct JmapSyncMetrics {
    pub email_query: u64,
    pub email_get: u64,
    pub email_changes: u64,
    pub email_reconciliations: u64,
    pub body_fetch: u64,
    pub blob_download: u64,
    pub response_bytes: u64,
    pub blob_bytes: u64,
    pub created: u64,
    pub updated: u64,
}

impl JmapClient {
    pub fn discover(origin: &str, token: String) -> Result<Self, JmapError> {
        Self::discover_inner(origin, token, None)
    }
    fn discover_inner(origin: &str, token: String, trusted_public_origin: Option<&str>) -> Result<Self, JmapError> {
        let loopback = trusted_public_origin.is_some();
        let origin = Url::parse(origin).map_err(|_| JmapError::InvalidUrl)?;
        if !allowed_origin(&origin, loopback)
            || origin.host_str().is_none()
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
            || token.is_empty()
        {
            return Err(JmapError::InvalidUrl);
        }
        let public = if let Some(value) = trusted_public_origin {
            let url = Url::parse(value).map_err(|_| JmapError::InvalidUrl)?;
            if !allowed_origin(&url, false) || url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() || url.path() != "/" || url.query().is_some() || url.fragment().is_some() { return Err(JmapError::InvalidUrl); }
            Some(url)
        } else { None };
        let mut builder = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(60));
        if loopback { builder = builder.no_proxy(); }
        let http = builder.build()
            .map_err(|_| JmapError::Transport)?;
        let gate = origin_gate(&origin);
        let discovery_permit = gate.acquire(false);
        let response = fetch_session(&http, &origin, &token, public.as_ref())?;
        if !response.status().is_success() {
            return Err(JmapError::Http(response.status().as_u16()));
        }
        let session: Value = serde_json::from_slice(&body(response, MAX_JSON)?)
            .map_err(|_| JmapError::InvalidResponse)?;
        drop(discovery_permit);
        let capabilities = session
            .get("capabilities")
            .and_then(Value::as_object)
            .ok_or(JmapError::InvalidResponse)?;
        let session_state = nonempty(session.get("state"))?.to_owned();
        let vapid_public_key = match capabilities.get(VAPID) {
            Some(capability) => Some(nonempty(capability.get("applicationServerKey"))?.to_owned()),
            None => None,
        };
        if !capabilities.contains_key(CORE) || !capabilities.contains_key(MAIL) {
            return Err(JmapError::Unsupported);
        }
        let core = capabilities.get(CORE).ok_or(JmapError::InvalidResponse)?;
        let limit = |name: &str| -> Result<u64, JmapError> {
            core.get(name)
                .and_then(Value::as_u64)
                .filter(|n| *n > 0)
                .ok_or(JmapError::InvalidResponse)
        };
        let request_limit = limit("maxConcurrentRequests")?.min(4) as usize;
        let attachment_limit = limit("maxConcurrentUpload")?.min(2) as usize;
        let max_get = limit("maxObjectsInGet")?.min(usize::MAX as u64) as usize;
        let max_request = limit("maxSizeRequest")?.min(MAX_JSON) as usize;
        let max_upload = limit("maxSizeUpload")?.min(MAX_BLOB);
        if limit("maxCallsInRequest")? < 1 || limit("maxObjectsInSet")? < 1 {
            return Err(JmapError::Unsupported);
        }
        let account =
            nonempty(session.pointer("/primaryAccounts/urn:ietf:params:jmap:mail"))?.to_owned();
        let account_caps = session
            .get("accounts")
            .and_then(|v| v.get(&account))
            .and_then(|v| v.get("accountCapabilities"))
            .and_then(Value::as_object)
            .ok_or(JmapError::InvalidResponse)?;
        if !account_caps.contains_key(MAIL) {
            return Err(JmapError::Unsupported);
        }
        let advertised = session.get("accounts").and_then(Value::as_object).ok_or(JmapError::InvalidResponse)?;
        if advertised.len() > 256 { return Err(JmapError::InvalidResponse); }
        let mut mail_accounts = Vec::new();
        for (id, entry) in advertised {
            let caps = entry.get("accountCapabilities").and_then(Value::as_object).ok_or(JmapError::InvalidResponse)?;
            if !caps.contains_key(MAIL) { continue; }
            if id.is_empty() || id.len() > 1024 || id.chars().any(char::is_control) { return Err(JmapError::InvalidResponse); }
            let read_only = entry.get("isReadOnly").and_then(Value::as_bool).unwrap_or(id != &account);
            mail_accounts.push((id.clone(), read_only));
        }
        let read_only = mail_accounts.iter().find(|(id,_)| id == &account).map(|(_,v)| *v).ok_or(JmapError::InvalidResponse)?;
        let submission =
            capabilities.contains_key(SUBMISSION) && account_caps.contains_key(SUBMISSION);
        let mail_share = capabilities.contains_key(MAIL_SHARE) && account_caps.contains_key(MAIL_SHARE);
        let map = |value: &str| -> Result<String, JmapError> { if let Some(public) = &public { rewrite_loopback_endpoint(&origin, public, value) } else { Ok(value.to_owned()) } };
        let api = endpoint(&origin, &map(nonempty(session.get("apiUrl"))?)?)?;
        let download = map(nonempty(session.get("downloadUrl"))?)?;
        let upload = map(nonempty(session.get("uploadUrl"))?)?;
        let event_source = if loopback { None } else {
            match session.get("eventSourceUrl") {
                Some(Value::String(template)) => Some(event_source_endpoint(&origin, template)?),
                None | Some(Value::Null) => None,
                _ => return Err(JmapError::InvalidResponse),
            }
        };
        // Validate templates before retaining them; substitute safe placeholders.
        endpoint(
            &origin,
            &download
                .replace("{accountId}", "a")
                .replace("{blobId}", "b")
                .replace("{name}", "n")
                .replace("{type}", "t"),
        )?;
        endpoint(&origin, &upload.replace("{accountId}", "a"))?;
        let gate = account_gate(
            &origin.origin().ascii_serialization(),
            request_limit,
            attachment_limit,
        );
        Ok(Self {
            http,
            origin,
            token,
            primary_account:account.clone(),
            account,
            mail_accounts,
            read_only,
            api,
            download,
            upload,
            event_source,
            submission,
            mail_share,
            gate,
            max_get,
            max_request,
            max_upload,
            session_state,
            vapid_public_key,
            metrics: Mutex::new(JmapSyncMetrics::default()),
            sync_failure: Arc::new(Mutex::new(None)),
        })
    }
    pub fn sync_metrics(&self) -> JmapSyncMetrics {
        *self.metrics.lock().unwrap_or_else(|error| error.into_inner())
    }
    pub fn record_email_changes(&self, created: usize, updated: usize) {
        let mut metrics = self.metrics.lock().unwrap_or_else(|error| error.into_inner());
        metrics.created += created as u64;
        metrics.updated += updated as u64;
    }
    pub fn record_email_reconciliation(&self) {
        self.metrics.lock().unwrap_or_else(|error| error.into_inner()).email_reconciliations += 1;
    }
    pub fn clear_sync_failure(&self) { *self.sync_failure.lock().unwrap_or_else(|e|e.into_inner())=None; }
    pub fn last_sync_failure(&self) -> Option<JmapSyncFailure> { *self.sync_failure.lock().unwrap_or_else(|e|e.into_inner()) }
    pub fn record_store_failure(&self) { *self.sync_failure.lock().unwrap_or_else(|e|e.into_inner())=Some(JmapSyncFailure {operation:"store",class:"store",http_status:None,method_type:None}); }
    fn record_sync_error(&self, method:&str, error:&JmapError) {
        let operation=match method { "Mailbox/query"=>"mailbox_query", "Mailbox/get"=>"mailbox_get", "Mailbox/changes"=>"mailbox_changes", "Email/query"=>"email_query", "Email/get"=>"email_get", "Email/changes"=>"email_changes", _=>return };
        let (class,http_status,method_type)=match error {
            JmapError::Http(status)=>("http",Some(*status),None),
            JmapError::Transport=>("transport",None,None),
            JmapError::InvalidUrl | JmapError::InvalidResponse=>("invalid_response",None,None),
            JmapError::Unsupported=>("unsupported",None,None),
            JmapError::OutcomeUnknown=>("outcome_unknown",None,None),
            JmapError::Method(kind)=>{
                let safe=match kind.as_str() { "cannotCalculateChanges"=>"cannotCalculateChanges", "stateMismatch"=>"stateMismatch", "accountNotFound"=>"accountNotFound", "accountNotSupportedByMethod"=>"accountNotSupportedByMethod", "accountReadOnly"=>"accountReadOnly", "forbidden"=>"forbidden", "invalidArguments"=>"invalidArguments", "serverFail"=>"serverFail", "unknownMethod"=>"unknownMethod", "requestTooLarge"=>"requestTooLarge", _=>"other" };
                ("method",None,Some(safe))
            }
        };
        *self.sync_failure.lock().unwrap_or_else(|e|e.into_inner())=Some(JmapSyncFailure {operation,class,http_status,method_type});
    }
    pub fn account_id(&self) -> &str {
        &self.account
    }
    /// One read-only negative check for the fixed administrative method.
    pub fn probe_admin_query(&self) -> &'static str {
        let payload = match self.call_payload("x:Account/query", json!({"accountId":self.account,"limit":1}), false, false) {
            Ok(payload)=>payload,
            Err(JmapError::Http(401))=>return "http_401",
            Err(JmapError::Http(403))=>return "http_403",
            Err(JmapError::Http(_))=>return "other_http",
            Err(_)=>return "unavailable",
        };
        match parse_method(&payload,"x:Account/query") {
            Ok(_)=>"allowed",
            Err(JmapError::Method(kind)) if kind=="forbidden"=>"forbidden",
            Err(JmapError::Method(kind)) if kind=="accountNotFound"=>"account_not_found",
            Err(JmapError::Method(kind)) if kind=="unknownMethod"=>"unknown_method",
            Err(JmapError::Method(_))=>"other_method_error",
            Err(_)=>"invalid_response",
        }
    }
    pub fn primary_account_id(&self) -> &str { &self.primary_account }
    /// Only accounts advertised for mail by this freshly authenticated session.
    pub fn mail_accounts(&self) -> &[(String, bool)] { &self.mail_accounts }
    /// Bind a client to a discovered account. Shared accounts are read-only locally.
    pub fn for_mail_account(&self, account: &str) -> Result<Self, JmapError> {
        let advertised_read_only = self.mail_accounts.iter().find(|(id,_)| id == account).map(|(_,read_only)| *read_only).ok_or(JmapError::Unsupported)?;
        Ok(Self { http:self.http.clone(), origin:self.origin.clone(), token:self.token.clone(), account:account.into(), primary_account:self.primary_account.clone(), mail_accounts:self.mail_accounts.clone(), read_only:advertised_read_only || account != self.primary_account, api:self.api.clone(), download:self.download.clone(), upload:self.upload.clone(), event_source:self.event_source.clone(), submission:self.submission && account == self.primary_account, mail_share:self.mail_share, gate:self.gate.clone(), max_get:self.max_get, max_request:self.max_request, max_upload:self.max_upload, session_state:self.session_state.clone(), vapid_public_key:self.vapid_public_key.clone(), metrics:Mutex::new(JmapSyncMetrics::default()), sync_failure:self.sync_failure.clone() })
    }
    pub fn origin(&self) -> &str {
        self.origin.as_str()
    }
    pub fn supports_submission(&self) -> bool {
        self.submission
    }
    pub fn max_objects_in_get(&self) -> usize {
        self.max_get
    }
    pub fn session_state(&self) -> &str {
        &self.session_state
    }
    pub fn vapid_public_key(&self) -> Option<&str> {
        self.vapid_public_key.as_deref()
    }

    /// One bounded authenticated stream. A successful connection requests an
    /// immediate delta pass before consuming notifications, closing the gap on
    /// every reconnect. Caller owns lifecycle, backoff and token refresh.
    pub fn watch_mail_events(&self, stop: &AtomicBool, mut on_change: impl FnMut(EventDomains) -> Result<(), ()>) -> Result<(), JmapError> {
        let url = self.event_source.clone().ok_or(JmapError::Unsupported)?;
        let stream_gate = account_gate(&format!("stream|{}|{}", self.origin.origin().ascii_serialization(), self.primary_account), 4, 2);
        let _stream_identity_permit = stream_gate.acquire_stream_identity();
        let _stream_permit = self.gate.acquire_stream().ok_or(JmapError::Unsupported)?;
        let response = self.http.get(url).bearer_auth(&self.token).header(reqwest::header::ACCEPT,"text/event-stream").send().map_err(|_| JmapError::Transport)?;
        if !response.status().is_success() { return Err(JmapError::Http(response.status().as_u16())); }
        let content_type = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
        if !content_type.split(';').next().is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream")) { return Err(JmapError::InvalidResponse); }
        if stop.load(Ordering::Relaxed) { return Ok(()); }
        on_change(EventDomains {mail:true,dav:true}).map_err(|_| JmapError::Transport)?;
        let mut reader = BufReader::new(response);
        consume_mail_event_stream(&mut reader, &self.account, stop, &mut on_change)
    }

    fn call_payload(
        &self,
        method: &str,
        args: Value,
        mutation: bool,
        submission: bool,
    ) -> Result<Value, JmapError> {
        if mutation && self.read_only { return Err(JmapError::Unsupported); }
        let _permit = self.gate.acquire(false);
        {
            let mut metrics = self.metrics.lock().unwrap_or_else(|error| error.into_inner());
            if method == "Email/query" {
                metrics.email_query += 1;
            } else if method == "Email/get" {
                metrics.email_get += 1;
                if args["fetchTextBodyValues"].as_bool() == Some(true) || args["fetchHTMLBodyValues"].as_bool() == Some(true) {
                    metrics.body_fetch += 1;
                }
            } else if method == "Email/changes" {
                metrics.email_changes += 1;
            }
        }
        let mut using = vec![CORE, MAIL];
        if self.mail_share {
            using.push(MAIL_SHARE);
        }
        if submission {
            if !self.submission {
                return Err(JmapError::Unsupported);
            }
            using.push(SUBMISSION);
        }
        let request = json!({"using":using,"methodCalls":[[method,args,"c1"]]});
        if serde_json::to_vec(&request)
            .map_err(|_| JmapError::InvalidResponse)?
            .len()
            > self.max_request
        {
            return Err(JmapError::InvalidResponse);
        }
        let response = self
            .http
            .post(self.api.clone())
            .bearer_auth(&self.token)
            .json(&request)
            .send()
            .map_err(|_| {
                if mutation {
                    JmapError::OutcomeUnknown
                } else {
                    JmapError::Transport
                }
            })?;
        if !response.status().is_success() {
            return Err(if mutation && response.status().is_server_error() {
                JmapError::OutcomeUnknown
            } else {
                JmapError::Http(response.status().as_u16())
            });
        }
        let raw = body(response, MAX_JSON).map_err(|e| {
            if mutation {
                JmapError::OutcomeUnknown
            } else {
                e
            }
        })?;
        self.metrics.lock().unwrap_or_else(|error| error.into_inner()).response_bytes += raw.len() as u64;
        serde_json::from_slice(&raw).map_err(|_| {
            if mutation {
                JmapError::OutcomeUnknown
            } else {
                JmapError::InvalidResponse
            }
        })
    }
    fn call(
        &self,
        method: &str,
        args: Value,
        mutation: bool,
        submission: bool,
    ) -> Result<Value, JmapError> {
        // If a syntactically valid method response fails later engine validation,
        // retain its fixed operation as invalid_response. A successful sync clears it.
        if !mutation { self.record_sync_error(method,&JmapError::InvalidResponse); }
        let result = self.call_checked(method,args,mutation,submission);
        if !mutation { if let Err(error)=&result { self.record_sync_error(method,error); } }
        result
    }
    fn call_checked(
        &self,
        method: &str,
        args: Value,
        mutation: bool,
        submission: bool,
    ) -> Result<Value, JmapError> {
        let payload = self.call_payload(method, args, mutation, submission)?;
        let result = parse_method(&payload, method).map_err(|e| {
            if mutation
                && (e == JmapError::InvalidResponse
                    || e == JmapError::Method("serverPartialFail".into()))
            {
                JmapError::OutcomeUnknown
            } else {
                e
            }
        })?;
        if result.get("accountId").and_then(Value::as_str) != Some(self.account.as_str()) {
            return Err(if mutation {
                JmapError::OutcomeUnknown
            } else {
                JmapError::InvalidResponse
            });
        }
        Ok(result)
    }

    /// `kind` is Email, Mailbox, Identity or EmailSubmission; IDs and raw objects
    /// remain native so the caller can retain mailbox membership as many-to-many.
    pub fn get(
        &self,
        kind: &str,
        ids: Option<&[String]>,
        properties: Option<&[String]>,
    ) -> Result<Value, JmapError> {
        if !matches!(kind, "Email" | "Mailbox" | "Identity" | "EmailSubmission") {
            return Err(JmapError::Unsupported);
        }
        if ids.is_some_and(|values| values.len() > self.max_get) {
            return Err(JmapError::InvalidResponse);
        }
        self.call(
            &format!("{kind}/get"),
            json!({"accountId":self.account,"ids":ids,"properties":properties}),
            false,
            matches!(kind, "Identity" | "EmailSubmission"),
        )
    }
    pub fn query_emails(
        &self,
        position: usize,
        limit: usize,
        filter: Option<Value>,
    ) -> Result<Value, JmapError> {
        if limit == 0 || limit > 1000 {
            return Err(JmapError::InvalidResponse);
        }
        let mut args = json!({"accountId":self.account,"position":position,"limit":limit,"calculateTotal":true});
        if let Some(filter) = filter {
            args["filter"] = filter;
        }
        self.call("Email/query", args, false, false)
    }
    pub fn query_mailboxes(&self, position: usize, limit: usize) -> Result<Value, JmapError> {
        if limit == 0 || limit > 1000 {
            return Err(JmapError::InvalidResponse);
        }
        self.call("Mailbox/query", json!({"accountId":self.account,"position":position,"limit":limit,"calculateTotal":true}), false, false)
    }
    pub fn changes(&self, kind: &str, since: &str, max: usize) -> Result<Value, JmapError> {
        if !matches!(kind, "Email" | "Mailbox" | "EmailSubmission") || since.is_empty() || max == 0 || max > 1000 {
            return Err(JmapError::InvalidResponse);
        }
        self.call(
            &format!("{kind}/changes"),
            json!({"accountId":self.account,"sinceState":since,"maxChanges":max}),
            false,
            kind == "EmailSubmission",
        )
    }
    pub fn query_changes(
        &self,
        since: &str,
        filter: Option<Value>,
        max: usize,
    ) -> Result<Value, JmapError> {
        if since.is_empty() || max == 0 || max > 1000 {
            return Err(JmapError::InvalidResponse);
        }
        let mut args = json!({"accountId":self.account,"sinceQueryState":since,"maxChanges":max});
        if let Some(filter) = filter {
            args["filter"] = filter;
        }
        self.call("Email/queryChanges", args, false, false)
    }
    /// Fetch only requested messages, with the server's text/html body values.
    /// Truncated values are reported by JMAP in the returned native bodyValues.
    pub fn get_email_bodies(
        &self,
        ids: &[String],
        max_body_value_bytes: usize,
    ) -> Result<Value, JmapError> {
        if ids.is_empty()
            || ids.len() > self.max_get
            || max_body_value_bytes == 0
            || max_body_value_bytes > 16 * 1024 * 1024
        {
            return Err(JmapError::InvalidResponse);
        }
        self.call("Email/get", json!({"accountId":self.account,"ids":ids,"properties":["id","blobId","threadId","mailboxIds","keywords","size","receivedAt","messageId","inReplyTo","references","sender","from","to","cc","bcc","replyTo","subject","sentAt","hasAttachment","preview","textBody","htmlBody","bodyStructure","attachments","bodyValues","header:Importance:asText","header:Disposition-Notification-To:asAddresses"],"fetchTextBodyValues":true,"fetchHTMLBodyValues":true,"maxBodyValueBytes":max_body_value_bytes}), false, false)
    }
    pub fn query_submissions(
        &self,
        position: usize,
        limit: usize,
        email_id: &str,
    ) -> Result<Value, JmapError> {
        if email_id.is_empty() || limit == 0 || limit > 1000 {
            return Err(JmapError::InvalidResponse);
        }
        self.call("EmailSubmission/query", json!({"accountId":self.account,"position":position,"limit":limit,"filter":{"emailIds":[email_id]}}), false, true)
    }
    pub fn create_draft(&self, creation_id: &str, email: Value) -> Result<Value, JmapError> {
        if creation_id.is_empty() || !email.is_object() {
            return Err(JmapError::InvalidResponse);
        }
        self.call(
            "Email/set",
            json!({"accountId":self.account,"create":{creation_id:email}}),
            true,
            false,
        )
    }
    pub fn import_email(
        &self,
        creation_id: &str,
        blob_id: &str,
        mailbox_ids: Value,
    ) -> Result<Value, JmapError> {
        if creation_id.is_empty() || blob_id.is_empty() || !mailbox_ids.is_object() {
            return Err(JmapError::InvalidResponse);
        }
        self.call("Email/import", json!({"accountId":self.account,"emails":{creation_id:{"blobId":blob_id,"mailboxIds":mailbox_ids}}}), true, false)
    }
    /// The caller records the intent before dispatch and reconciles unknown outcomes.
    pub fn create_mailbox(&self, creation_id: &str, name: &str, if_in_state: &str) -> Result<MailboxCreateResult, JmapError> {
        if creation_id.is_empty() || name.is_empty() || if_in_state.is_empty() { return Err(JmapError::InvalidResponse); }
        let response = self.call("Mailbox/set", json!({"accountId":self.account,"ifInState":if_in_state,"create":{creation_id:{"name":name}}}), true, false)?;
        let created = response.get("created").and_then(|v| v.get(creation_id));
        let rejected = response.get("notCreated").and_then(|v| v.get(creation_id));
        match (created, rejected) {
            (Some(value), None) => Ok(MailboxCreateResult::Created { id: nonempty(value.get("id")).map_err(|_| JmapError::OutcomeUnknown)?.to_owned() }),
            (None, Some(value)) => {
                let kind = nonempty(value.get("type")).map_err(|_| JmapError::OutcomeUnknown)?;
                if kind.len() > 80 || !kind.bytes().all(|c| c.is_ascii_alphanumeric()) { return Err(JmapError::OutcomeUnknown); }
                Ok(MailboxCreateResult::Rejected { kind: kind.to_owned() })
            }
            _ => Err(JmapError::OutcomeUnknown),
        }
    }
    /// Change only mailbox memberships and keywords against an observed Email state.
    /// The caller owns the durable intent, authorization and unknown reconciliation.
    pub fn update_email(&self, email_id: &str, if_in_state: &str, patch: &Value) -> Result<EmailUpdateResult, JmapError> {
        let entries = patch.as_object().filter(|v| !v.is_empty() && v.len() <= 128).ok_or(JmapError::InvalidResponse)?;
        if email_id.is_empty() || if_in_state.is_empty() || entries.iter().any(|(key, value)| {
            let part = key.strip_prefix("keywords/").or_else(|| key.strip_prefix("mailboxIds/"));
            let valid_path = part.is_some_and(|s| !s.is_empty() && !s.contains('/') && valid_pointer_token(s));
            !valid_path || !(value.is_null() || value.as_bool() == Some(true))
        }) { return Err(JmapError::InvalidResponse); }
        let result = self.call("Email/set", json!({"accountId":self.account,"ifInState":if_in_state,"update":{email_id:patch}}), true, false)?;
        let updated = result.get("updated").and_then(|v| v.get(email_id));
        let rejected = result.get("notUpdated").and_then(|v| v.get(email_id));
        Ok(match (updated, rejected) {
            (Some(value), None) if value.is_null() || value.is_object() => EmailUpdateResult::Applied,
            (None, Some(value)) if value.is_object() => EmailUpdateResult::Rejected,
            _ => EmailUpdateResult::Unknown,
        })
    }
    /// Caller must persist the exact approved intent before invoking this once.
    /// Unknown outcomes require native reconciliation, never automatic retry.
    pub fn submit(
        &self,
        creation_id: &str,
        email_id: &str,
        identity_id: &str,
        if_in_state: &str,
        mail_from: &str,
        recipients: &[String],
        draft_mailbox_id: &str,
        sent_mailbox_id: &str,
    ) -> Result<SubmissionResult, JmapError> {
        if [
            creation_id,
            email_id,
            identity_id,
            if_in_state,
            mail_from,
            draft_mailbox_id,
            sent_mailbox_id,
        ]
        .iter()
        .any(|s| s.is_empty())
            || draft_mailbox_id == sent_mailbox_id
            || recipients.is_empty()
            || recipients.len() > 1000
            || recipients
                .iter()
                .any(|s| s.is_empty() || s.len() > 320 || s.contains('\r') || s.contains('\n'))
            || mail_from.len() > 320
            || mail_from.contains('\r')
            || mail_from.contains('\n')
        {
            return Err(JmapError::InvalidResponse);
        }
        let rcpt: Vec<Value> = recipients
            .iter()
            .map(|address| json!({"email":address}))
            .collect();
        let mut patch = serde_json::Map::new();
        patch.insert(
            format!("mailboxIds/{}", pointer_escape(draft_mailbox_id)),
            Value::Null,
        );
        patch.insert(
            format!("mailboxIds/{}", pointer_escape(sent_mailbox_id)),
            json!(true),
        );
        patch.insert("keywords/$draft".into(), Value::Null);
        let reference = format!("#{creation_id}");
        let payload = self.call_payload("EmailSubmission/set", json!({"accountId":self.account,"ifInState":if_in_state,"create":{creation_id:{"emailId":email_id,"identityId":identity_id,"envelope":{"mailFrom":{"email":mail_from},"rcptTo":rcpt}}},"onSuccessUpdateEmail":{reference:patch}}), true, true)?;
        parse_submission(&payload, &self.account, creation_id, email_id)
    }
    pub fn get_submissions(&self, ids: Option<&[String]>) -> Result<Value, JmapError> {
        self.get("EmailSubmission", ids, None)
    }

    /// Returns the native session-scoped response; it can contain private push
    /// URLs and verification codes and must never be logged.
    pub fn push_get(&self, ids: Option<&[String]>) -> Result<Value, JmapError> {
        if ids.is_some_and(|v| v.len() > self.max_get) {
            return Err(JmapError::InvalidResponse);
        }
        let payload =
            self.call_payload("PushSubscription/get", json!({"ids":ids}), false, false)?;
        parse_method(&payload, "PushSubscription/get")?;
        Ok(payload)
    }
    /// The caller persists an intent and reconciles a lost response before any retry.
    pub fn push_create(
        &self,
        creation_id: &str,
        device_client_id: &str,
        endpoint_url: &str,
        p256dh: &str,
        auth: &str,
        types: Option<&[String]>,
        expires: Option<&str>,
    ) -> Result<PushMutation, JmapError> {
        let endpoint_url_parsed = Url::parse(endpoint_url).map_err(|_| JmapError::InvalidUrl)?;
        if endpoint_url_parsed.scheme() != "https"
            || endpoint_url_parsed.host_str().is_none()
            || !endpoint_url_parsed.username().is_empty()
            || endpoint_url_parsed.password().is_some()
            || endpoint_url_parsed.fragment().is_some()
            || [creation_id, device_client_id, p256dh, auth]
                .iter()
                .any(|v| v.is_empty())
        {
            return Err(JmapError::InvalidResponse);
        }
        let mut object = json!({"deviceClientId":device_client_id,"url":endpoint_url,"keys":{"p256dh":p256dh,"auth":auth},"types":types});
        if let Some(expires) = expires {
            object["expires"] = json!(expires);
        }
        self.push_set(
            json!({"create":{creation_id:object}}),
            "created",
            "notCreated",
            creation_id,
            None,
        )
    }
    pub fn push_verify(&self, id: &str, code: &str) -> Result<PushMutation, JmapError> {
        if id.is_empty() || code.is_empty() {
            return Err(JmapError::InvalidResponse);
        }
        self.push_set(
            json!({"update":{id:{"verificationCode":code}}}),
            "updated",
            "notUpdated",
            id,
            Some(id),
        )
    }
    pub fn push_renew(&self, id: &str, expires: &str) -> Result<PushMutation, JmapError> {
        if id.is_empty() || expires.is_empty() {
            return Err(JmapError::InvalidResponse);
        }
        self.push_set(
            json!({"update":{id:{"expires":expires}}}),
            "updated",
            "notUpdated",
            id,
            Some(id),
        )
    }
    pub fn push_destroy(&self, id: &str) -> Result<PushMutation, JmapError> {
        if id.is_empty() {
            return Err(JmapError::InvalidResponse);
        }
        self.push_set(
            json!({"destroy":[id]}),
            "destroyed",
            "notDestroyed",
            id,
            Some(id),
        )
    }
    fn push_set(
        &self,
        args: Value,
        success: &str,
        failure: &str,
        key: &str,
        existing_id: Option<&str>,
    ) -> Result<PushMutation, JmapError> {
        let payload = self.call_payload("PushSubscription/set", args, true, false)?;
        parse_push_mutation(&payload, success, failure, key, existing_id)
    }

    pub fn download(
        &self,
        blob_id: &str,
        name: &str,
        media_type: &str,
    ) -> Result<Vec<u8>, JmapError> {
        let _permit = self.gate.acquire(true);
        self.metrics.lock().unwrap_or_else(|error| error.into_inner()).blob_download += 1;
        let template = self
            .download
            .replace("{accountId}", &encode(&self.account))
            .replace("{blobId}", &encode(blob_id))
            .replace("{name}", &encode(name))
            .replace("{type}", &encode(media_type));
        let response = self
            .http
            .get(endpoint(&self.origin, &template)?)
            .bearer_auth(&self.token)
            .send()
            .map_err(|_| JmapError::Transport)?;
        if !response.status().is_success() {
            return Err(JmapError::Http(response.status().as_u16()));
        }
        let bytes = body(response, MAX_BLOB)?;
        self.metrics.lock().unwrap_or_else(|error| error.into_inner()).blob_bytes += bytes.len() as u64;
        Ok(bytes)
    }
    pub fn upload(&self, bytes: Vec<u8>, media_type: &str) -> Result<Value, JmapError> {
        if self.read_only { return Err(JmapError::Unsupported); }
        if bytes.len() as u64 > self.max_upload {
            return Err(JmapError::InvalidResponse);
        }
        let _permit = self.gate.acquire(true);
        let template = self.upload.replace("{accountId}", &encode(&self.account));
        let response = self
            .http
            .post(endpoint(&self.origin, &template)?)
            .bearer_auth(&self.token)
            .header(reqwest::header::CONTENT_TYPE, media_type)
            .body(bytes)
            .send()
            .map_err(|_| JmapError::OutcomeUnknown)?;
        if !response.status().is_success() {
            return Err(if response.status().is_server_error() {
                JmapError::OutcomeUnknown
            } else {
                JmapError::Http(response.status().as_u16())
            });
        }
        let raw = body(response, MAX_JSON).map_err(|_| JmapError::OutcomeUnknown)?;
        serde_json::from_slice(&raw).map_err(|_| JmapError::OutcomeUnknown)
    }
}
fn encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
fn pointer_escape(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}
fn valid_pointer_token(value: &str) -> bool {
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'~' && !matches!(bytes.next(), Some(b'0' | b'1')) { return false; }
    }
    true
}
fn parse_push_mutation(
    payload: &Value,
    success: &str,
    failure: &str,
    key: &str,
    existing_id: Option<&str>,
) -> Result<PushMutation, JmapError> {
    let session_state = nonempty(payload.get("sessionState"))
        .map_err(|_| JmapError::OutcomeUnknown)?
        .to_owned();
    let result =
        parse_method(payload, "PushSubscription/set").map_err(|_| JmapError::OutcomeUnknown)?;
    let rejected = result.get(failure).and_then(|v| v.get(key));
    let applied = if success == "destroyed" {
        result
            .get(success)
            .and_then(Value::as_array)
            .and_then(|ids| ids.iter().find(|v| v.as_str() == Some(key)))
    } else {
        result.get(success).and_then(|v| v.get(key))
    };
    let outcome = match (applied, rejected) {
        (Some(value), None) => {
            let id = match existing_id {
                Some(id) => id,
                None => nonempty(value.get("id")).map_err(|_| JmapError::OutcomeUnknown)?,
            };
            let expires = value
                .get("expires")
                .and_then(Value::as_str)
                .map(str::to_owned);
            PushOutcome::Applied {
                id: id.to_owned(),
                expires,
            }
        }
        (None, Some(value)) => {
            let kind = nonempty(value.get("type")).map_err(|_| JmapError::OutcomeUnknown)?;
            if kind.len() > 80 || !kind.bytes().all(|c| c.is_ascii_alphanumeric()) {
                return Err(JmapError::OutcomeUnknown);
            }
            PushOutcome::Rejected {
                kind: kind.to_owned(),
            }
        }
        _ => return Err(JmapError::OutcomeUnknown),
    };
    Ok(PushMutation {
        session_state,
        outcome,
    })
}
fn parse_submission(
    payload: &Value,
    account: &str,
    creation_id: &str,
    email_id: &str,
) -> Result<SubmissionResult, JmapError> {
    let responses = payload
        .get("methodResponses")
        .and_then(Value::as_array)
        .ok_or(JmapError::OutcomeUnknown)?;
    let first = responses
        .first()
        .and_then(Value::as_array)
        .filter(|v| v.len() == 3 && v[2].as_str() == Some("c1"))
        .ok_or(JmapError::OutcomeUnknown)?;
    if first[0].as_str() == Some("error") {
        return Err(JmapError::OutcomeUnknown);
    }
    if first[0].as_str() != Some("EmailSubmission/set")
        || first[1].get("accountId").and_then(Value::as_str) != Some(account)
    {
        return Err(JmapError::OutcomeUnknown);
    }
    let value = &first[1];
    let created = value.get("created").and_then(|v| v.get(creation_id));
    let rejected = value.get("notCreated").and_then(|v| v.get(creation_id));
    match (created, rejected) {
        (Some(object), None) => {
            let id = nonempty(object.get("id")).map_err(|_| JmapError::OutcomeUnknown)?;
            let email_update = if responses.len() == 2 {
                match responses[1]
                    .as_array()
                    .filter(|v| v.len() == 3 && v[2].as_str() == Some("c1"))
                {
                    Some(tuple)
                        if tuple[0].as_str() == Some("Email/set")
                            && tuple[1].get("accountId").and_then(Value::as_str)
                                == Some(account) =>
                    {
                        let updated = tuple[1].get("updated").and_then(|v| v.get(email_id));
                        let rejected = tuple[1].get("notUpdated").and_then(|v| v.get(email_id));
                        match (updated, rejected) {
                            (Some(Value::Null), None) => EmailUpdateResult::Applied,
                            (None, Some(v)) if v.is_object() => EmailUpdateResult::Rejected,
                            _ => EmailUpdateResult::Unknown,
                        }
                    }
                    _ => EmailUpdateResult::Unknown,
                }
            } else {
                EmailUpdateResult::Unknown
            };
            Ok(SubmissionResult::Created {
                id: id.to_owned(),
                email_update,
            })
        }
        (None, Some(object)) => {
            let kind = nonempty(object.get("type")).map_err(|_| JmapError::OutcomeUnknown)?;
            if kind.len() > 80 || !kind.bytes().all(|c| c.is_ascii_alphanumeric()) {
                return Err(JmapError::OutcomeUnknown);
            }
            Ok(SubmissionResult::Rejected {
                kind: kind.to_owned(),
            })
        }
        _ => Err(JmapError::OutcomeUnknown),
    }
}
fn parse_method(payload: &Value, method: &str) -> Result<Value, JmapError> {
    let responses = payload
        .get("methodResponses")
        .and_then(Value::as_array)
        .ok_or(JmapError::InvalidResponse)?;
    if responses.len() != 1 {
        return Err(JmapError::InvalidResponse);
    }
    let tuple = responses[0]
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or(JmapError::InvalidResponse)?;
    if tuple[2].as_str() != Some("c1") {
        return Err(JmapError::InvalidResponse);
    }
    if tuple[0].as_str() == Some("error") {
        let kind = nonempty(tuple[1].get("type"))?;
        // Only the server-defined error class is exposed; descriptions can contain private data.
        if kind.len() > 80 || !kind.bytes().all(|c| c.is_ascii_alphanumeric()) {
            return Err(JmapError::InvalidResponse);
        }
        return Err(JmapError::Method(kind.to_owned()));
    }
    if tuple[0].as_str() != Some(method) || !tuple[1].is_object() {
        return Err(JmapError::InvalidResponse);
    }
    Ok(tuple[1].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_follows_307_only_after_same_origin_validation() {
        use std::io::{Read, Write};
        let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address=listener.local_addr().unwrap();
        let server=std::thread::spawn(move || {
            for (path, status, headers, payload) in [
                ("/.well-known/jmap", "307 Temporary Redirect", "Location: /jmap/session\r\n", ""),
                ("/jmap/session", "200 OK", "", "{}"),
                ("/not-discovery", "307 Temporary Redirect", "Location: /jmap/session\r\n", ""),
            ] {
                let (mut socket,_) = listener.accept().unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut request=Vec::new(); let mut chunk=[0u8;1024];
                while !request.windows(4).any(|v| v==b"\r\n\r\n") { let n=socket.read(&mut chunk).unwrap(); assert!(n>0); request.extend_from_slice(&chunk[..n]); }
                let request=String::from_utf8(request).unwrap();
                assert!(request.starts_with(&format!("GET {path} ")));
                if path!="/not-discovery" { assert!(request.to_ascii_lowercase().contains("authorization: bearer fixture-token")); }
                write!(socket,"HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",payload.len()).unwrap();
            }
        });
        let origin=Url::parse(&format!("http://{address}/")).unwrap();
        let http=Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(5)).build().unwrap();
        assert_eq!(fetch_session(&http,&origin,"fixture-token",None).unwrap().status().as_u16(),200);
        assert_eq!(http.get(origin.join("/not-discovery").unwrap()).send().unwrap().status().as_u16(),307);
        server.join().unwrap();
    }

    #[test]
    fn discovery_rejects_foreign_origin_and_redirect_loops() {
        use std::io::{Read, Write};
        let foreign=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        foreign.set_nonblocking(true).unwrap();
        let foreign_location=format!("http://{}/session",foreign.local_addr().unwrap());
        for location in [foreign_location.as_str(), "/.well-known/jmap"] {
            let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address=listener.local_addr().unwrap();
            let location=location.to_owned();
            let server=std::thread::spawn(move || {
                let (mut socket,_) = listener.accept().unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut request=Vec::new(); let mut buf=[0u8;1024];
                while !request.windows(4).any(|v| v==b"\r\n\r\n") { let n=socket.read(&mut buf).unwrap(); assert!(n>0); request.extend_from_slice(&buf[..n]); }
                write!(socket,"HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            });
            let origin=Url::parse(&format!("http://{address}/")).unwrap();
            let http=Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(5)).build().unwrap();
            assert!(matches!(fetch_session(&http,&origin,"fixture-token",None),Err(JmapError::InvalidUrl)));
            server.join().unwrap();
        }
        assert!(matches!(foreign.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock));
    }

    #[test]
    fn discovery_loopback_rebases_only_enrolled_public_origin() {
        use std::io::{Read, Write};
        let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address=listener.local_addr().unwrap();
        let server=std::thread::spawn(move || {
            for step in 0..2 {
                let (mut socket,_) = listener.accept().unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut request=Vec::new(); let mut buf=[0u8;1024];
                while !request.windows(4).any(|v| v==b"\r\n\r\n") { let n=socket.read(&mut buf).unwrap(); assert!(n>0); request.extend_from_slice(&buf[..n]); }
                if step==0 { write!(socket,"HTTP/1.1 307 Temporary Redirect\r\nLocation: https://public.example/jmap/session\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap(); }
                else { assert!(request.starts_with(b"GET /jmap/session ")); write!(socket,"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap(); }
            }
        });
        let origin=Url::parse(&format!("http://{address}/")).unwrap();
        let public=Url::parse("https://public.example/").unwrap();
        let http=Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(5)).build().unwrap();
        assert_eq!(fetch_session(&http,&origin,"fixture-token",Some(&public)).unwrap().status().as_u16(),200);
        server.join().unwrap();
    }

    #[test]
    fn discovery_rejects_more_than_three_redirects() {
        use std::io::{Read, Write};
        let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address=listener.local_addr().unwrap();
        let server=std::thread::spawn(move || {
            for step in 0..4 {
                let (mut socket,_) = listener.accept().unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut request=Vec::new(); let mut buf=[0u8;1024];
                while !request.windows(4).any(|v| v==b"\r\n\r\n") { let n=socket.read(&mut buf).unwrap(); assert!(n>0); request.extend_from_slice(&buf[..n]); }
                write!(socket,"HTTP/1.1 307 Temporary Redirect\r\nLocation: /step{step}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            }
        });
        let origin=Url::parse(&format!("http://{address}/")).unwrap();
        let http=Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(5)).build().unwrap();
        assert!(matches!(fetch_session(&http,&origin,"fixture-token",None),Err(JmapError::InvalidUrl)));
        server.join().unwrap();
    }
    #[test]
    fn event_stream_limits_scope_and_dispatches_only_mail_hints() {
        let origin = Url::parse("https://mail.example/").unwrap();
        let url = event_source_endpoint(&origin,"https://mail.example/jmap/eventsource/?types={types}&closeafter={closeafter}&ping={ping}").unwrap();
        assert_eq!(url.query(),Some("types=Email%2CMailbox%2CCalendar%2CCalendarEvent%2CAddressBook%2CContactCard&closeafter=no&ping=30"));
        assert!(event_source_endpoint(&origin,"https://evil.example/?types={types}&closeafter={closeafter}&ping={ping}").is_err());
        assert!(event_source_endpoint(&origin,"/events?types={types}&ping={ping}").is_err());
        let stream = b"event: ping\ndata: {\"interval\":30}\n\nevent: state\ndata: {\"@type\":\"StateChange\",\"changed\":{\"other\":{\"Email\":\"a\"}}}\n\nevent: state\ndata: {\"@type\":\"StateChange\",\"changed\":{\"account\":{\"Email\":\"b\"}}}\n\n";
        let stop = AtomicBool::new(false); let mut events = Vec::new();
        consume_mail_event_stream(&mut std::io::Cursor::new(stream),"account",&stop,&mut |domains| {events.push(domains); Ok(())}).unwrap();
        assert_eq!(events,vec![EventDomains {mail:false,dav:false},EventDomains {mail:true,dav:false},EventDomains {mail:true,dav:false}]);
        let oversized = vec![b'x';65_538];
        assert_eq!(consume_mail_event_stream(&mut std::io::Cursor::new(oversized),"account",&stop,&mut |_| Ok(())),Err(JmapError::InvalidResponse));
        assert!(state_change_domains("{}","account").is_err());
        assert_eq!(state_change_domains(r#"{"@type":"StateChange","changed":{"shared":{"CalendarEvent":"s"}}}"#,"account").unwrap(),EventDomains {mail:false,dav:true});
    }

    #[test]
    fn event_stream_authenticates_and_reconciles_on_connect_and_hint() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
          for _ in 0..2 {
            let (mut socket,_) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut input = Vec::new(); let mut buf = [0u8;1024];
            while !input.windows(4).any(|part| part == b"\r\n\r\n") { let n=socket.read(&mut buf).unwrap(); assert!(n>0); input.extend_from_slice(&buf[..n]); }
            let request = String::from_utf8(input).unwrap().to_ascii_lowercase();
            assert!(request.contains("authorization: bearer fixture-token"));
            assert!(request.contains("accept: text/event-stream"));
            let body = b"event: state\ndata: {\"@type\":\"StateChange\",\"changed\":{\"account\":{\"Email\":\"s1\"}}}\n\n";
            write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
            socket.write_all(body).unwrap();
          }
        });
        let origin=Url::parse(&format!("http://{address}/")).unwrap();
        let client=JmapClient { http:Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().unwrap(), origin:origin.clone(), token:"fixture-token".into(), account:"account".into(), primary_account:"account".into(), mail_accounts:vec![("account".into(),false),("shared".into(),false)], read_only:false, api:origin.clone(), download:String::new(), upload:String::new(), event_source:Some(origin.join("/events").unwrap()), submission:false, mail_share:false, gate:account_gate("event-test",2,1), max_get:10, max_request:1024, max_upload:1024, session_state:"state".into(), vapid_public_key:None, metrics:Mutex::new(JmapSyncMetrics::default()), sync_failure:Arc::new(Mutex::new(None)) };
        assert!(client.for_mail_account("unadvertised").is_err());
        let shared=client.for_mail_account("shared").unwrap();
        assert!(!shared.supports_submission());
        assert!(matches!(shared.upload(Vec::new(),"application/octet-stream"),Err(JmapError::Unsupported)));
        shared.record_sync_error("Email/get",&JmapError::Method("untrusted-provider-text".into()));
        assert_eq!(client.last_sync_failure(),Some(JmapSyncFailure {operation:"email_get",class:"method",http_status:None,method_type:Some("other")}));
        client.clear_sync_failure();
        shared.record_sync_error("Mailbox/query",&JmapError::Http(403));
        assert_eq!(client.last_sync_failure(),Some(JmapSyncFailure {operation:"mailbox_query",class:"http",http_status:Some(403),method_type:None}));
        client.clear_sync_failure();
        let mut events = Vec::new();
        client.watch_mail_events(&AtomicBool::new(false),|domains| {events.push(domains); Ok(())}).unwrap();
        client.watch_mail_events(&AtomicBool::new(false),|domains| {events.push(domains); Ok(())}).unwrap();
        server.join().unwrap();
        assert_eq!(events,vec![EventDomains {mail:true,dav:true},EventDomains {mail:true,dav:false},EventDomains {mail:true,dav:true},EventDomains {mail:true,dav:false}]);
    }
    #[test]
    fn method_responses_require_matching_id_and_keep_errors_distinct() {
        assert_eq!(
            parse_method(
                &json!({"methodResponses":[["error",{"type":"cannotCalculateChanges"},"c1"]]}),
                "Email/changes"
            ),
            Err(JmapError::Method("cannotCalculateChanges".into()))
        );
        assert_eq!(
            parse_method(
                &json!({"methodResponses":[["Email/get",{},"wrong"]]}),
                "Email/get"
            ),
            Err(JmapError::InvalidResponse)
        );
    }
    #[test]
    fn endpoint_rejects_cross_origin_and_credentials() {
        let origin = Url::parse("https://mail.example/").unwrap();
        assert!(endpoint(&origin, "https://other.example/api").is_err());
        assert!(endpoint(&origin, "https://user@mail.example/api").is_err());
        assert!(endpoint(&origin, "/api").is_ok());
        assert!(allowed_origin(&Url::parse("http://127.0.0.1:8080/").unwrap(), true));
        assert!(allowed_origin(&Url::parse("http://[::1]:8080/").unwrap(), true));
        assert!(!allowed_origin(&Url::parse("http://localhost:8080/").unwrap(), true));
        assert!(!allowed_origin(&Url::parse("http://127.0.0.2:8080/").unwrap(), true));
        assert!(!allowed_origin(&Url::parse("http://127.0.0.1:8080/").unwrap(), false));
        assert!(endpoint(&Url::parse("http://127.0.0.1:8080/").unwrap(), "http://127.0.0.1:8081/api").is_err());
        let local = Url::parse("http://127.0.0.1:8080/").unwrap();
        let public = Url::parse("https://mail.example/").unwrap();
        assert_eq!(rewrite_loopback_endpoint(&local, &public, "https://mail.example/download/{accountId}/{blobId}/{name}?accept={type}").unwrap(), "http://127.0.0.1:8080/download/{accountId}/{blobId}/{name}?accept={type}");
        assert!(rewrite_loopback_endpoint(&local, &public, "https://other.example/api").is_err());
        assert!(rewrite_loopback_endpoint(&local, &public, "https://mail.example:444/api").is_err());
        assert!(rewrite_loopback_endpoint(&local, &public, "https://user@mail.example/api").is_err());
        assert!(rewrite_loopback_endpoint(&local, &public, "http://127.0.0.1:8081/api").is_err());
        assert!(valid_pointer_token("a~1b~0c"));
        assert!(!valid_pointer_token("a~2b"));
    }
    #[test]
    fn account_gates_are_shared() {
        assert!(Arc::ptr_eq(
            &account_gate("a", 4, 2),
            &account_gate("a", 1, 1)
        ));
        assert!(!Arc::ptr_eq(
            &account_gate("a", 4, 2),
            &account_gate("b", 4, 2)
        ));
    }
    #[test]
    fn submission_requires_a_single_native_outcome() {
        let submission =
            json!(["EmailSubmission/set",{"accountId":"acc","created":{"a":{"id":"s1"}}},"c1"]);
        let applied = json!(["Email/set",{"accountId":"acc","updated":{"e1":null}},"c1"]);
        let rejected =
            json!(["Email/set",{"accountId":"acc","notUpdated":{"e1":{"type":"forbidden"}}},"c1"]);
        assert_eq!(
            parse_submission(
                &json!({"methodResponses":[submission.clone(), applied]}),
                "acc",
                "a",
                "e1"
            ),
            Ok(SubmissionResult::Created {
                id: "s1".into(),
                email_update: EmailUpdateResult::Applied
            })
        );
        assert_eq!(
            parse_submission(
                &json!({"methodResponses":[submission.clone(), rejected]}),
                "acc",
                "a",
                "e1"
            ),
            Ok(SubmissionResult::Created {
                id: "s1".into(),
                email_update: EmailUpdateResult::Rejected
            })
        );
        assert_eq!(
            parse_submission(&json!({"methodResponses":[submission]}), "acc", "a", "e1"),
            Ok(SubmissionResult::Created {
                id: "s1".into(),
                email_update: EmailUpdateResult::Unknown
            })
        );
        assert_eq!(
            parse_submission(
                &json!({"methodResponses":[["EmailSubmission/set",{"accountId":"acc","notCreated":{"a":{"type":"forbidden"}}},"c1"]]}),
                "acc",
                "a",
                "e1"
            ),
            Ok(SubmissionResult::Rejected {
                kind: "forbidden".into()
            })
        );
        assert_eq!(
            parse_submission(
                &json!({"methodResponses":[["EmailSubmission/set",{"accountId":"acc","created":{},"notCreated":{}},"c1"]]}),
                "acc",
                "a",
                "e1"
            ),
            Err(JmapError::OutcomeUnknown)
        );
        assert_eq!(pointer_escape("a~/b"), "a~0~1b");
    }
    #[test]
    fn push_mutations_require_native_outcome_and_session_state() {
        let created = json!({"sessionState":"s1","methodResponses":[["PushSubscription/set",{"created":{"c":{"id":"p1","expires":"2030-01-01T00:00:00Z"}}},"c1"]]});
        let mutation = parse_push_mutation(&created, "created", "notCreated", "c", None).unwrap();
        assert_eq!(mutation.session_state, "s1");
        assert!(
            matches!(mutation.outcome, PushOutcome::Applied { id, expires: Some(_) } if id == "p1")
        );
        let verified = json!({"sessionState":"s1","methodResponses":[["PushSubscription/set",{"updated":{"p1":null}},"c1"]]});
        assert!(matches!(
            parse_push_mutation(&verified, "updated", "notUpdated", "p1", Some("p1"))
                .unwrap()
                .outcome,
            PushOutcome::Applied { .. }
        ));
        let missing =
            json!({"sessionState":"s1","methodResponses":[["PushSubscription/set",{},"c1"]]});
        assert!(matches!(
            parse_push_mutation(&missing, "created", "notCreated", "c", None),
            Err(JmapError::OutcomeUnknown)
        ));
    }
}
