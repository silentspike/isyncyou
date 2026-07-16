//! Live HTTP [`Transport`] backed by reqwest + rustls (feature `http`).
//!
//! Adds the bearer token, parses `Retry-After`, and maps transport errors to a
//! retryable status so the [`crate::run_delta`] orchestration handles them
//! uniformly. The pure orchestration is tested with a mock transport; this is the
//! thin real-network adapter, exercised by the env-gated live test below.

use crate::client::{Response, Transport};
use crate::upload::{UploadSession, CHUNK_ALIGN};
use std::time::Duration;

const GRAPH: &str = "https://graph.microsoft.com/v1.0";

#[derive(Debug, Clone)]
pub struct ServerTimeSample {
    pub server_unix_ms: u64,
    pub received_at_monotonic: std::time::Instant,
}

/// One binary data part for a OneNote multipart page-create request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneNotePagePart {
    pub name: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// Parsed Microsoft Graph permission data needed for crash-safe share recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrivePermission {
    pub id: String,
    pub roles: Vec<String>,
    pub link_web_url: Option<String>,
    pub link_type: Option<String>,
    pub link_scope: Option<String>,
    pub granted_emails: Vec<String>,
    pub invitation_emails: Vec<String>,
    pub inherited: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictBehavior {
    Fail,
    Replace,
    Rename,
}

/// Result of a streamed OneDrive content mutation. A create name collision and a stale
/// replace ETag are policy outcomes, not transport failures.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamUploadOutcome {
    Applied(serde_json::Value),
    Conflict,
}

impl ConflictBehavior {
    fn as_graph(self) -> &'static str {
        match self {
            Self::Fail => "fail",
            Self::Replace => "replace",
            Self::Rename => "rename",
        }
    }
}

impl DrivePermission {
    pub fn from_graph_value(value: &serde_json::Value) -> Option<Self> {
        let id = value.get("id")?.as_str()?.to_string();
        let roles = string_array(value.get("roles"));
        let link = value.get("link");
        let link_web_url = link
            .and_then(|l| l.get("webUrl"))
            .and_then(|u| u.as_str())
            .map(String::from);
        let link_type = link
            .and_then(|l| l.get("type"))
            .and_then(|t| t.as_str())
            .map(String::from);
        let link_scope = link
            .and_then(|l| l.get("scope"))
            .and_then(|s| s.as_str())
            .map(String::from);
        let mut granted_emails = Vec::new();
        for key in ["grantedToV2", "grantedTo"] {
            if let Some(identity) = value.get(key) {
                collect_identity_emails(identity, &mut granted_emails);
            }
        }
        for key in ["grantedToIdentitiesV2", "grantedToIdentities"] {
            if let Some(identities) = value.get(key).and_then(|v| v.as_array()) {
                for identity in identities {
                    collect_identity_emails(identity, &mut granted_emails);
                }
            }
        }
        let mut invitation_emails = Vec::new();
        if let Some(email) = value
            .get("invitation")
            .and_then(|i| i.get("email"))
            .and_then(|e| e.as_str())
        {
            push_unique_nonempty(&mut invitation_emails, email);
        }
        let inherited = value
            .get("inheritedFrom")
            .is_some_and(|inherited| !inherited.is_null());
        Some(Self {
            id,
            roles,
            link_web_url,
            link_type,
            link_scope,
            granted_emails,
            invitation_emails,
            inherited,
        })
    }
}

/// Result from Microsoft Graph invite that preserves partial-success semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InviteOutcome {
    Applied {
        permission_ids: Vec<String>,
    },
    Partial {
        successful_permission_ids: Vec<String>,
        failed_recipient_count: usize,
        redacted_reason: String,
    },
}

fn string_array(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn collect_identity_emails(value: &serde_json::Value, out: &mut Vec<String>) {
    for key in ["email", "mail", "userPrincipalName", "loginName"] {
        if let Some(candidate) = value.get(key).and_then(|v| v.as_str()) {
            push_email_candidate(out, candidate);
        }
    }
    for key in ["user", "siteUser", "emailAddress"] {
        if let Some(child) = value.get(key) {
            collect_identity_emails(child, out);
        }
    }
}

fn push_email_candidate(out: &mut Vec<String>, candidate: &str) {
    if candidate.contains('@') {
        push_unique_nonempty(out, candidate);
    }
}

fn push_unique_nonempty(out: &mut Vec<String>, value: &str) {
    let trimmed = value.trim();
    if !trimmed.is_empty() && !out.iter().any(|existing| existing == trimmed) {
        out.push(trimmed.to_string());
    }
}

/// Errors from the live upload/delete path.
#[derive(Debug)]
pub enum UploadError {
    Http {
        status: u16,
        body: String,
    },
    Transport(String),
    Timeout(String),
    Parse(String),
    /// The response exceeded the caller's explicit body limit.
    TooLarge,
    /// The session ended without a completion response.
    Incomplete,
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UploadError::Http { status, body } => write!(f, "HTTP {status}: {body}"),
            UploadError::Transport(e) => write!(f, "transport error: {e}"),
            UploadError::Timeout(e) => write!(f, "timeout: {e}"),
            UploadError::Parse(e) => write!(f, "parse error: {e}"),
            UploadError::TooLarge => write!(f, "response body exceeded limit"),
            UploadError::Incomplete => write!(f, "upload ended without completion"),
        }
    }
}
impl std::error::Error for UploadError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphTransientFailure {
    Network,
    Timeout,
    Http(u16),
}

impl UploadError {
    fn from_reqwest(error: reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::Timeout(error.to_string())
        } else {
            Self::Transport(error.to_string())
        }
    }

    pub fn transient_failure(&self) -> Option<GraphTransientFailure> {
        match self {
            Self::Timeout(_) => Some(GraphTransientFailure::Timeout),
            Self::Transport(_) => Some(GraphTransientFailure::Network),
            Self::Http { status, .. } if matches!(*status, 408 | 425 | 429 | 500..=599) => {
                Some(GraphTransientFailure::Http(*status))
            }
            Self::Http { .. } | Self::Parse(_) | Self::TooLarge | Self::Incomplete => None,
        }
    }
}

/// A Microsoft Graph HTTP client carrying a bearer access token.
#[derive(Clone)]
pub struct GraphClient {
    client: reqwest::blocking::Client,
    token: String,
    /// API base (default: the public Graph v1.0 endpoint). Overridable via
    /// [`Self::with_base_url`] for tests and non-public (sovereign) endpoints.
    base: String,
    /// When set, GETs send `Prefer: IdType="ImmutableId", outlook.timezone="UTC"`
    /// (the Outlook immutable-ID policy, plan §6).
    prefer_immutable_id: bool,
}

/// The `Prefer` header value for the Outlook immutable-ID policy (plan §6).
const PREFER_IMMUTABLE_ID: &str = r#"IdType="ImmutableId", outlook.timezone="UTC""#;

/// The default HTTP client for Graph calls (#0.4): a request timeout so a hung
/// connection can never wedge a sync/read pass, plus a connect timeout. Falls back to
/// a plain client if the builder ever fails (it doesn't in practice).
fn default_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new())
}

impl GraphClient {
    pub fn new(access_token: impl Into<String>) -> Self {
        GraphClient {
            client: default_client(),
            token: access_token.into(),
            base: GRAPH.into(),
            prefer_immutable_id: false,
        }
    }

    /// Build with a custom reqwest client (timeouts, proxy, …).
    pub fn with_client(client: reqwest::blocking::Client, access_token: impl Into<String>) -> Self {
        GraphClient {
            client,
            token: access_token.into(),
            base: GRAPH.into(),
            prefer_immutable_id: false,
        }
    }

    /// Override the API base URL (no trailing slash). For deterministic tests
    /// against a local endpoint and for non-public (sovereign-cloud) Graph
    /// endpoints; the default is the public `v1.0` base.
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    /// Absolute URL for `url`: pass absolute URLs through, prefix paths with the
    /// configured API base.
    fn abs(&self, url: &str) -> String {
        if url.starts_with("http") {
            url.to_string()
        } else {
            format!("{}{url}", self.base)
        }
    }
}

fn parse_retry_after(resp: &reqwest::blocking::Response) -> Option<Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Central retry policy (#0.4): 429 (throttled) and 5xx (transient server) are safe to
/// retry for an *idempotent* request; everything else is returned as-is. Non-idempotent
/// writes (POST/PATCH/PUT/DELETE) never go through the retry path — their recovery is
/// ledger-driven (#0D), never a blind re-send that could double-apply.
fn is_retryable_status(status: u16) -> bool {
    status == 429 || (500..=599).contains(&status)
}

/// Exponential backoff for retry attempt `n` (1-based), capped, used when the server
/// gave no `Retry-After`.
fn backoff_delay(attempt: u32) -> Duration {
    let ms = 200u64.saturating_mul(1u64 << attempt.min(5));
    Duration::from_millis(ms).min(MAX_RETRY_WAIT)
}

/// Bounded attempts for an idempotent GET and the cap on any single backoff/Retry-After
/// wait (so a hostile `Retry-After` can't wedge the pass).
const MAX_GET_ATTEMPTS: u32 = 4;
const MAX_RETRY_WAIT: Duration = Duration::from_secs(20);

impl Transport for GraphClient {
    fn get(&mut self, url: &str) -> Response {
        let mut req = self.client.get(url).bearer_auth(&self.token);
        if self.prefer_immutable_id {
            // `Prefer` is not in reqwest's well-known header set, so name it directly.
            req = req.header("Prefer", PREFER_IMMUTABLE_ID);
        }
        match req.send() {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let retry_after = parse_retry_after(&resp);
                let body = resp.json::<serde_json::Value>().ok();
                Response {
                    status,
                    retry_after,
                    body,
                }
            }
            // Network/transport failure: surface as a retryable 503 so the
            // delta loop's retry budget applies.
            Err(e) => Response {
                status: e.status().map(|s| s.as_u16()).unwrap_or(503),
                retry_after: None,
                body: None,
            },
        }
    }

    /// Real transport sleeps out the backoff between retries (the trait default
    /// is a no-op for unit-test mocks).
    fn backoff(&self, delay: std::time::Duration) {
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
    }

    fn set_prefer_immutable_id(&mut self, on: bool) {
        self.prefer_immutable_id = on;
    }
}

impl GraphClient {
    /// Single-PUT upload for small files (the content endpoint).
    pub fn simple_upload(
        &self,
        dest_path: &str,
        data: &[u8],
    ) -> Result<serde_json::Value, UploadError> {
        let url = format!("{}/me/drive/root:/{}:/content", self.base, enc(dest_path));
        let resp = self
            .client
            .put(&url)
            .bearer_auth(&self.token)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(data.to_vec())
            .send()
            .map_err(UploadError::from_reqwest)?;
        json_or_err(resp)
    }

    pub fn upload_content_with_conflict_behavior(
        &self,
        dest_path: &str,
        data: &[u8],
        behavior: ConflictBehavior,
    ) -> Result<Option<serde_json::Value>, UploadError> {
        let url = format!(
            "{}/me/drive/root:/{}:/content?@microsoft.graph.conflictBehavior={}",
            self.base,
            enc_path(dest_path),
            behavior.as_graph()
        );
        let resp = self
            .client
            .put(&url)
            .bearer_auth(&self.token)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(data.to_vec())
            .send()
            .map_err(UploadError::from_reqwest)?;
        match resp.status().as_u16() {
            200 | 201 => Ok(Some(
                resp.json::<serde_json::Value>()
                    .map_err(|e| UploadError::Parse(e.to_string()))?,
            )),
            409 => Ok(None),
            s => Err(UploadError::Http {
                status: s,
                body: resp.text().unwrap_or_default().chars().take(300).collect(),
            }),
        }
    }

    pub fn get_drive_item_by_path(
        &self,
        path: &str,
        select: &[&str],
    ) -> Result<Option<serde_json::Value>, UploadError> {
        let mut url = format!("{}/me/drive/root:/{}:", self.base, enc_path(path));
        if !select.is_empty() {
            url.push_str("?$select=");
            url.push_str(&select.join(","));
        }
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .map_err(UploadError::from_reqwest)?;
        match resp.status().as_u16() {
            200 => Ok(Some(
                resp.json::<serde_json::Value>()
                    .map_err(|e| UploadError::Parse(e.to_string()))?,
            )),
            404 => Ok(None),
            s => Err(UploadError::Http {
                status: s,
                body: resp.text().unwrap_or_default().chars().take(300).collect(),
            }),
        }
    }

    /// Read trusted server time from a successful authenticated Graph response. Callers use the
    /// process-local monotonic receipt instant only to reject stale samples; it is never persisted.
    pub fn server_time_sample(&self) -> Result<ServerTimeSample, UploadError> {
        let url = format!("{}/me/drive/root?$select=id", self.base);
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .map_err(UploadError::from_reqwest)?;
        if !response.status().is_success() {
            return Err(UploadError::Http {
                status: response.status().as_u16(),
                body: String::new(),
            });
        }
        let value = response
            .headers()
            .get(reqwest::header::DATE)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| UploadError::Parse("missing server time".into()))?;
        let time = httpdate::parse_http_date(value)
            .map_err(|_| UploadError::Parse("invalid server time".into()))?;
        let server_unix_ms = time
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| UploadError::Parse("invalid server time".into()))?
            .as_millis()
            .try_into()
            .map_err(|_| UploadError::Parse("invalid server time".into()))?;
        Ok(ServerTimeSample {
            server_unix_ms,
            received_at_monotonic: std::time::Instant::now(),
        })
    }

    /// Open a resumable upload session for `dest_path` (`total` = file size).
    pub fn create_upload_session(
        &self,
        dest_path: &str,
        total: u64,
    ) -> Result<UploadSession, UploadError> {
        let url = format!(
            "{}/me/drive/root:/{}:/createUploadSession",
            self.base,
            enc(dest_path)
        );
        let body = serde_json::json!({"item": {"@microsoft.graph.conflictBehavior": "replace"}});
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .map_err(UploadError::from_reqwest)?;
        let v = json_or_err(resp)?;
        let upload_url = v.get("uploadUrl").and_then(|u| u.as_str()).ok_or_else(|| {
            UploadError::Parse("createUploadSession response had no uploadUrl".into())
        })?;
        Ok(UploadSession::new(upload_url, total))
    }

    fn create_target_upload_session(
        &self,
        url: String,
        total: u64,
        behavior: ConflictBehavior,
        if_match: Option<&str>,
    ) -> Result<Option<UploadSession>, UploadError> {
        let mut request = self
            .client
            .post(url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "item": {
                    "@microsoft.graph.conflictBehavior": behavior.as_graph(),
                    "fileSize": total,
                }
            }));
        if let Some(etag) = if_match {
            request = request.header(reqwest::header::IF_MATCH, etag);
        }
        let response = request.send().map_err(UploadError::from_reqwest)?;
        match response.status().as_u16() {
            200 | 201 => {
                let value = response
                    .json::<serde_json::Value>()
                    .map_err(|error| UploadError::Parse(error.to_string()))?;
                let upload_url = value
                    .get("uploadUrl")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| {
                        UploadError::Parse("createUploadSession response had no uploadUrl".into())
                    })?;
                Ok(Some(UploadSession::new(upload_url, total)))
            }
            409 | 412 => Ok(None),
            status => Err(UploadError::Http {
                status,
                body: String::new(),
            }),
        }
    }

    fn upload_session_from_ranges<F>(
        &self,
        mut session: UploadSession,
        mut read_range: F,
    ) -> Result<serde_json::Value, UploadError>
    where
        F: FnMut(u64, usize) -> Result<Vec<u8>, UploadError>,
    {
        while let Some(plan) = session.next_chunk(CHUNK_ALIGN) {
            let expected = usize::try_from(plan.len)
                .map_err(|_| UploadError::Parse("upload range too large".into()))?;
            let bytes = read_range(plan.start, expected)?;
            if bytes.len() != expected {
                return Err(UploadError::Parse(
                    "upload source returned a short range".into(),
                ));
            }
            let response = self
                .client
                .put(&session.upload_url)
                .header(reqwest::header::CONTENT_RANGE, &plan.content_range)
                .body(bytes)
                .send()
                .map_err(UploadError::from_reqwest)?;
            match response.status().as_u16() {
                202 => {
                    let value = response.json::<serde_json::Value>().ok();
                    let ranges = value
                        .as_ref()
                        .and_then(|value| value.get("nextExpectedRanges"))
                        .and_then(|value| value.as_array())
                        .map(|values| {
                            values
                                .iter()
                                .filter_map(|value| value.as_str().map(String::from))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    if ranges.is_empty() {
                        session.advance(plan.len);
                    } else {
                        session.apply_next_expected(&ranges);
                    }
                }
                200 | 201 => {
                    return response
                        .json::<serde_json::Value>()
                        .map_err(|error| UploadError::Parse(error.to_string()));
                }
                status => {
                    return Err(UploadError::Http {
                        status,
                        body: String::new(),
                    });
                }
            }
        }
        Err(UploadError::Incomplete)
    }

    /// Upload a new child from a bounded random-access source without materializing the whole
    /// body. `conflictBehavior=fail` preserves the ledger's probe-and-adopt recovery rule.
    pub fn upload_to_parent_streaming<F>(
        &self,
        parent_id: &str,
        name: &str,
        total: u64,
        read_range: F,
    ) -> Result<StreamUploadOutcome, UploadError>
    where
        F: FnMut(u64, usize) -> Result<Vec<u8>, UploadError>,
    {
        let url = if parent_id.is_empty() {
            format!(
                "{}/me/drive/root:/{}:/createUploadSession",
                self.base,
                enc(name)
            )
        } else {
            format!(
                "{}/me/drive/items/{}:/{}:/createUploadSession",
                self.base,
                parent_id,
                enc(name)
            )
        };
        let Some(session) =
            self.create_target_upload_session(url, total, ConflictBehavior::Fail, None)?
        else {
            return Ok(StreamUploadOutcome::Conflict);
        };
        match self.upload_session_from_ranges(session, read_range) {
            Ok(value) => Ok(StreamUploadOutcome::Applied(value)),
            Err(UploadError::Http { status: 409, .. }) => Ok(StreamUploadOutcome::Conflict),
            Err(error) => Err(error),
        }
    }

    /// Replace one item from a bounded random-access source. The ETag is checked before an
    /// upload session is issued, so stale content never starts transferring.
    pub fn replace_content_streaming_if_match<F>(
        &self,
        item_id: &str,
        etag: &str,
        total: u64,
        read_range: F,
    ) -> Result<StreamUploadOutcome, UploadError>
    where
        F: FnMut(u64, usize) -> Result<Vec<u8>, UploadError>,
    {
        let url = format!("{}/me/drive/items/{item_id}/createUploadSession", self.base);
        let Some(session) =
            self.create_target_upload_session(url, total, ConflictBehavior::Replace, Some(etag))?
        else {
            return Ok(StreamUploadOutcome::Conflict);
        };
        match self.upload_session_from_ranges(session, read_range) {
            Ok(value) => Ok(StreamUploadOutcome::Applied(value)),
            Err(UploadError::Http { status: 412, .. }) => Ok(StreamUploadOutcome::Conflict),
            Err(error) => Err(error),
        }
    }

    /// Upload `data` to `dest_path`. Small files go via a single PUT; larger ones
    /// use a resumable session with `max_chunk`-sized fragments and honor
    /// `nextExpectedRanges` for resume. Returns the created drive item.
    pub fn upload_file(
        &self,
        dest_path: &str,
        data: &[u8],
        max_chunk: u64,
    ) -> Result<serde_json::Value, UploadError> {
        self.upload_file_resumable(dest_path, data, max_chunk, &crate::NoopResume)
    }

    /// Like [`upload_file`](Self::upload_file) but persists the resumable session
    /// via `resume` (plan §6/§9), so a process kill mid-upload resumes from the
    /// server's `nextExpectedRanges` instead of restarting. On start it reuses a
    /// persisted session for this exact file (validated via [`Self::upload_status`]); on
    /// each accepted fragment it records the offset; on completion it clears it.
    pub fn upload_file_resumable(
        &self,
        dest_path: &str,
        data: &[u8],
        max_chunk: u64,
        resume: &dyn crate::UploadResumeStore,
    ) -> Result<serde_json::Value, UploadError> {
        if (data.len() as u64) <= CHUNK_ALIGN {
            return self.simple_upload(dest_path, data); // small file: no session
        }
        let total = data.len() as u64;
        let mut session = match resume.load(dest_path) {
            // a persisted session for the *same* file: validate + resume from the
            // server's offset (handles expiry → fall through to a fresh session).
            Some((url, persisted_total)) if persisted_total == total => {
                match self.upload_status(&url) {
                    Ok(offset) => UploadSession::resume(url, total, offset),
                    Err(_) => {
                        resume.clear(dest_path);
                        self.start_session(dest_path, total, resume)?
                    }
                }
            }
            other => {
                if other.is_some() {
                    resume.clear(dest_path); // stale (file size changed) → drop it
                }
                self.start_session(dest_path, total, resume)?
            }
        };
        while let Some(plan) = session.next_chunk(max_chunk) {
            let slice = &data[plan.start as usize..=plan.end as usize];
            let resp = self
                .client
                .put(&session.upload_url) // pre-authorized URL: no bearer header
                .header(reqwest::header::CONTENT_RANGE, &plan.content_range)
                .body(slice.to_vec())
                .send()
                .map_err(UploadError::from_reqwest)?;
            match resp.status().as_u16() {
                202 => {
                    let v = resp.json::<serde_json::Value>().ok();
                    let ranges: Vec<String> = v
                        .as_ref()
                        .and_then(|v| v.get("nextExpectedRanges"))
                        .and_then(|r| r.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    if ranges.is_empty() {
                        session.advance(plan.len);
                    } else {
                        session.apply_next_expected(&ranges);
                    }
                    // persist progress so a kill here resumes, not restarts
                    resume.save(dest_path, &session.upload_url, total, session.next_offset());
                }
                200 | 201 => {
                    resume.clear(dest_path); // done → drop the persisted session
                    return resp
                        .json::<serde_json::Value>()
                        .map_err(|e| UploadError::Parse(e.to_string()));
                }
                s => {
                    let body = resp.text().unwrap_or_default();
                    return Err(UploadError::Http {
                        status: s,
                        body: body.chars().take(300).collect(),
                    });
                }
            }
        }
        Err(UploadError::Incomplete)
    }

    /// Create a fresh upload session and persist it at offset 0.
    fn start_session(
        &self,
        dest_path: &str,
        total: u64,
        resume: &dyn crate::UploadResumeStore,
    ) -> Result<UploadSession, UploadError> {
        let s = self.create_upload_session(dest_path, total)?;
        resume.save(dest_path, &s.upload_url, total, 0);
        Ok(s)
    }

    /// Query a resumable upload session via its (pre-authorized) `uploadUrl` and
    /// return the next byte offset the server expects (`nextExpectedRanges`).
    pub fn upload_status(&self, upload_url: &str) -> Result<u64, UploadError> {
        let resp = self
            .client
            .get(upload_url) // pre-authorized URL: no bearer header
            .send()
            .map_err(UploadError::from_reqwest)?;
        match resp.status().as_u16() {
            200 => {
                let v = resp.json::<serde_json::Value>().ok();
                let offset = v
                    .as_ref()
                    .and_then(|v| v.get("nextExpectedRanges"))
                    .and_then(|r| r.as_array())
                    .and_then(|a| a.first())
                    .and_then(|x| x.as_str())
                    .and_then(|s| s.split('-').next())
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .unwrap_or(0);
                Ok(offset)
            }
            s => Err(UploadError::Http {
                status: s,
                body: String::new(),
            }),
        }
    }

    /// Replace an item's content **only if** its `etag` still matches, so a
    /// concurrent cloud change is never silently overwritten (plan A3). Returns
    /// the updated drive item on success, or `None` on `412 Precondition Failed`
    /// (the cloud changed since we last saw it — a conflict to resolve, not clobber).
    pub fn replace_content_if_match(
        &self,
        item_id: &str,
        data: &[u8],
        etag: &str,
    ) -> Result<Option<serde_json::Value>, UploadError> {
        let url = format!("{}/me/drive/items/{item_id}/content", self.base);
        let resp = self
            .client
            .put(&url)
            .bearer_auth(&self.token)
            .header(reqwest::header::IF_MATCH, etag)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(data.to_vec())
            .send()
            .map_err(UploadError::from_reqwest)?;
        match resp.status().as_u16() {
            200 | 201 => Ok(Some(
                resp.json::<serde_json::Value>()
                    .map_err(|e| UploadError::Parse(e.to_string()))?,
            )),
            412 => Ok(None),
            s => Err(UploadError::Http {
                status: s,
                body: resp.text().unwrap_or_default().chars().take(300).collect(),
            }),
        }
    }

    /// Upload `data` as a NEW child named `name` under folder `parent_id`, addressing the
    /// target by **parent id + name** (not a root-relative path). This is the create-upload
    /// primitive the ledger-backed writeback uses (#655 / S-OM.9) and that #657 needs for a
    /// WebUI upload into an Online folder, which has no local store row to derive a path from.
    ///
    /// Uses `@microsoft.graph.conflictBehavior=fail`, so an existing child of the same name is
    /// **never** clobbered or auto-renamed: a `409` returns `Ok(None)` — a conflict the ledger
    /// resolves by probe-and-adopt on recovery — mirroring [`replace_content_if_match`]'s
    /// `412 → None`. `Ok(Some(item))` on success.
    ///
    /// [`replace_content_if_match`]: Self::replace_content_if_match
    pub fn upload_to_parent(
        &self,
        parent_id: &str,
        name: &str,
        data: &[u8],
    ) -> Result<Option<serde_json::Value>, UploadError> {
        // An empty parent id addresses the drive root (a WebUI upload into an Online root, #657):
        // `/items/:/…:/content` would be malformed and Graph rejects it ("Entity only allows writes
        // with a JSON Content-Type header"), so target `/root:/…:/content` there. A real parent id
        // keeps the path-relative-to-item form the offline writeback already uses.
        let url = if parent_id.is_empty() {
            format!(
                "{}/me/drive/root:/{}:/content?@microsoft.graph.conflictBehavior=fail",
                self.base,
                enc(name),
            )
        } else {
            format!(
                "{}/me/drive/items/{}:/{}:/content?@microsoft.graph.conflictBehavior=fail",
                self.base,
                parent_id,
                enc(name),
            )
        };
        let resp = self
            .client
            .put(&url)
            .bearer_auth(&self.token)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(data.to_vec())
            .send()
            .map_err(UploadError::from_reqwest)?;
        match resp.status().as_u16() {
            200 | 201 => Ok(Some(
                resp.json::<serde_json::Value>()
                    .map_err(|e| UploadError::Parse(e.to_string()))?,
            )),
            409 => Ok(None), // name already exists → conflict (recovery probe-adopts)
            s => Err(UploadError::Http {
                status: s,
                body: resp.text().unwrap_or_default().chars().take(300).collect(),
            }),
        }
    }

    /// Replace a drive item's content unconditionally (no `If-Match`). Used by the
    /// FUSE write-back, where the mounted filesystem owns the file for its session.
    pub fn put_content(
        &self,
        item_id: &str,
        data: &[u8],
    ) -> Result<serde_json::Value, UploadError> {
        let url = format!("{}/me/drive/items/{item_id}/content", self.base);
        let resp = self
            .client
            .put(&url)
            .bearer_auth(&self.token)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(data.to_vec())
            .send()
            .map_err(UploadError::from_reqwest)?;
        match resp.status().as_u16() {
            200 | 201 => resp
                .json::<serde_json::Value>()
                .map_err(|e| UploadError::Parse(e.to_string())),
            s => Err(UploadError::Http {
                status: s,
                body: resp.text().unwrap_or_default().chars().take(300).collect(),
            }),
        }
    }

    /// Delete a mail message by id. The id is percent-encoded for the URL path —
    /// Outlook message ids are base64-ish (`+ / =`), which Graph 404s if left raw.
    pub fn delete_message(&self, message_id: &str) -> Result<(), UploadError> {
        self.delete_url(&format!("/me/messages/{}", encode_id(message_id)))
    }

    /// Delete a drive item by id (used for test cleanup on the throwaway account).
    pub fn delete_item(&self, item_id: &str) -> Result<(), UploadError> {
        let url = format!("{}/me/drive/items/{}", self.base, encode_id(item_id));
        let resp = self
            .client
            .delete(&url)
            .bearer_auth(&self.token)
            .send()
            .map_err(UploadError::from_reqwest)?;
        match resp.status().as_u16() {
            200 | 204 => Ok(()),
            s => Err(UploadError::Http {
                status: s,
                body: resp.text().unwrap_or_default().chars().take(200).collect(),
            }),
        }
    }

    pub fn delete_item_if_match(&self, item_id: &str, etag: &str) -> Result<bool, UploadError> {
        let url = format!("{}/me/drive/items/{}", self.base, encode_id(item_id));
        let resp = self
            .client
            .delete(&url)
            .bearer_auth(&self.token)
            .header(reqwest::header::IF_MATCH, etag)
            .send()
            .map_err(UploadError::from_reqwest)?;
        match resp.status().as_u16() {
            200 | 204 => Ok(true),
            412 => Ok(false),
            s => Err(UploadError::Http {
                status: s,
                body: resp.text().unwrap_or_default().chars().take(200).collect(),
            }),
        }
    }

    /// Create a child folder named `name` under the drive folder `parent_id`
    /// (an empty id = the drive root). `conflictBehavior: fail` so a duplicate
    /// name returns a 409 rather than silently auto-renaming (the FUSE layer has
    /// already refused a duplicate, so a 409 here means a concurrent change).
    /// Returns the created folder item (its `id` is the new remote id).
    pub fn create_folder(
        &self,
        parent_id: &str,
        name: &str,
    ) -> Result<serde_json::Value, UploadError> {
        let path = if parent_id.is_empty() {
            "/me/drive/root/children".to_string()
        } else {
            format!("/me/drive/items/{}/children", encode_id(parent_id))
        };
        let body = serde_json::json!({
            "name": name,
            "folder": {},
            "@microsoft.graph.conflictBehavior": "fail",
        });
        self.post_json(&path, &body)
    }

    /// Rename and/or move a drive item. `new_parent_id` is `Some` only when the
    /// item changes parent (an empty id = the drive root, addressed by path);
    /// `None` keeps the current parent and only renames. Returns the updated item.
    pub fn move_item(
        &self,
        item_id: &str,
        new_parent_id: Option<&str>,
        new_name: &str,
    ) -> Result<serde_json::Value, UploadError> {
        let mut body = serde_json::json!({ "name": new_name });
        if let Some(pid) = new_parent_id {
            body["parentReference"] = if pid.is_empty() {
                serde_json::json!({ "path": "/drive/root:" })
            } else {
                serde_json::json!({ "id": pid })
            };
        }
        self.patch_json(&format!("/me/drive/items/{}", encode_id(item_id)), &body)
    }

    // --- Outbound sharing (#494): share a drive item via a link, an email invite,
    // or by listing/revoking its permissions. `Files.ReadWrite` covers all of these
    // (no extra consent). The FUSE-mount-relative path equals the cloud path, so the
    // CLI resolves a selected path to an id with `item_id_for_path`, then operates
    // by id (id-addressing is universal; path-addressing here is only the resolve).

    /// Resolve a drive item's id from its drive-relative path (the FUSE-mount path
    /// equals the cloud path). Per-segment percent-encoded so arbitrary names
    /// (`:`, spaces, umlauts) resolve. Returns the item `id`.
    pub fn item_id_for_path(&self, rel_path: &str) -> Result<String, UploadError> {
        let url = format!("{}/me/drive/root:/{}", self.base, enc_path(rel_path));
        let v = self.get_json(&url)?;
        v.get("id")
            .and_then(|i| i.as_str())
            .map(String::from)
            .ok_or_else(|| UploadError::Parse("drive item response had no id".into()))
    }

    /// The drive's storage quota (`total`/`used`/`remaining`/`state`) from
    /// `GET /me/drive` (#564). Returns the `quota` object as-is.
    pub fn drive_quota(&self) -> Result<serde_json::Value, UploadError> {
        let url = format!("{}/me/drive", self.base);
        let v = self.get_json(&url)?;
        v.get("quota")
            .cloned()
            .ok_or_else(|| UploadError::Parse("drive response had no quota".into()))
    }

    /// Create (or, idempotently per `(link_type, scope)`, return the existing)
    /// sharing link for an item. `link_type` = `view`/`edit`/`embed`, `scope` =
    /// `anonymous`/`organization`/`users`. `password`/`expiry` are
    /// account/Premium-dependent on personal accounts. Returns the link's `webUrl`.
    pub fn create_link(
        &self,
        item_id: &str,
        link_type: &str,
        scope: &str,
        password: Option<&str>,
        expiry: Option<&str>,
        retain_inherited: Option<bool>,
    ) -> Result<String, UploadError> {
        let v = self.post_create_link_json(
            item_id,
            link_type,
            scope,
            password,
            expiry,
            retain_inherited,
        )?;
        v.pointer("/link/webUrl")
            .and_then(|u| u.as_str())
            .map(String::from)
            .ok_or_else(|| UploadError::Parse("createLink response had no link.webUrl".into()))
    }

    /// Create (or return an existing) sharing link and preserve the returned
    /// permission id plus link metadata for share-ledger recovery.
    pub fn create_link_detailed(
        &self,
        item_id: &str,
        link_type: &str,
        scope: &str,
        password: Option<&str>,
        expiry: Option<&str>,
        retain_inherited: Option<bool>,
    ) -> Result<DrivePermission, UploadError> {
        let v = self.post_create_link_json(
            item_id,
            link_type,
            scope,
            password,
            expiry,
            retain_inherited,
        )?;
        DrivePermission::from_graph_value(&v)
            .ok_or_else(|| UploadError::Parse("createLink response was not a permission".into()))
    }

    fn post_create_link_json(
        &self,
        item_id: &str,
        link_type: &str,
        scope: &str,
        password: Option<&str>,
        expiry: Option<&str>,
        retain_inherited: Option<bool>,
    ) -> Result<serde_json::Value, UploadError> {
        let url = format!("/me/drive/items/{}/createLink", encode_id(item_id));
        let mut body = serde_json::json!({ "type": link_type, "scope": scope });
        if let Some(p) = password {
            body["password"] = serde_json::Value::String(p.to_string());
        }
        if let Some(e) = expiry {
            body["expirationDateTime"] = serde_json::Value::String(e.to_string());
        }
        if let Some(r) = retain_inherited {
            body["retainInheritedPermissions"] = serde_json::Value::Bool(r);
        }
        self.post_json(&url, &body)
    }

    /// Invite people to an item by email. `roles` is e.g. `["read"]` or
    /// `["write"]`. Returns the created permission ids (`value[].id`).
    #[allow(clippy::too_many_arguments)]
    pub fn invite(
        &self,
        item_id: &str,
        emails: &[String],
        roles: &[&str],
        require_sign_in: bool,
        send_invitation: bool,
        message: &str,
        expiry: Option<&str>,
        password: Option<&str>,
    ) -> Result<Vec<String>, UploadError> {
        match self.invite_detailed(
            item_id,
            emails,
            roles,
            require_sign_in,
            send_invitation,
            message,
            expiry,
            password,
        )? {
            InviteOutcome::Applied { permission_ids } => Ok(permission_ids),
            InviteOutcome::Partial {
                redacted_reason, ..
            } => Err(UploadError::Http {
                status: 207,
                body: redacted_reason,
            }),
        }
    }

    /// Invite people to an item while preserving `207 Multi-Status` as a
    /// distinct partial outcome for crash-safe invite recovery.
    #[allow(clippy::too_many_arguments)]
    pub fn invite_detailed(
        &self,
        item_id: &str,
        emails: &[String],
        roles: &[&str],
        require_sign_in: bool,
        send_invitation: bool,
        message: &str,
        expiry: Option<&str>,
        password: Option<&str>,
    ) -> Result<InviteOutcome, UploadError> {
        let url = format!("/me/drive/items/{}/invite", encode_id(item_id));
        let recipients: Vec<serde_json::Value> = emails
            .iter()
            .map(|e| serde_json::json!({ "email": e }))
            .collect();
        let mut body = serde_json::json!({
            "recipients": recipients,
            "roles": roles,
            "requireSignIn": require_sign_in,
            "sendInvitation": send_invitation,
            "message": message,
        });
        if let Some(e) = expiry {
            body["expirationDateTime"] = serde_json::Value::String(e.to_string());
        }
        if let Some(p) = password {
            body["password"] = serde_json::Value::String(p.to_string());
        }
        let (status, v) = self.post_json_with_status(&url, &body)?;
        let permission_ids = permission_ids_from_invite_value(&v);
        if status == 207 {
            let value_len = v
                .get("value")
                .and_then(|a| a.as_array())
                .map_or(0, Vec::len);
            let failed_recipient_count = value_len.saturating_sub(permission_ids.len());
            return Ok(InviteOutcome::Partial {
                successful_permission_ids: permission_ids,
                failed_recipient_count,
                redacted_reason: "partial_success".to_string(),
            });
        }
        Ok(InviteOutcome::Applied { permission_ids })
    }

    /// List an item's permissions as `(permission id, roles, link webUrl, grantee
    /// display name)` tuples.
    #[allow(clippy::type_complexity)]
    pub fn list_permissions(
        &self,
        item_id: &str,
    ) -> Result<Vec<(String, Vec<String>, Option<String>, Option<String>)>, UploadError> {
        Ok(self
            .list_permissions_detailed(item_id)?
            .into_iter()
            .map(|permission| {
                let grantee = permission
                    .granted_emails
                    .first()
                    .cloned()
                    .or_else(|| permission.invitation_emails.first().cloned());
                (
                    permission.id,
                    permission.roles,
                    permission.link_web_url,
                    grantee,
                )
            })
            .collect())
    }

    /// List an item's permissions with the fields needed for share/invite
    /// recovery. Inherited permissions are preserved so callers can ignore them
    /// deliberately.
    pub fn list_permissions_detailed(
        &self,
        item_id: &str,
    ) -> Result<Vec<DrivePermission>, UploadError> {
        let url = format!("/me/drive/items/{}/permissions", encode_id(item_id));
        let v = self.get_json(&url)?;
        Ok(v.get("value")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(DrivePermission::from_graph_value)
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Revoke a permission (un-share) by its id.
    pub fn delete_permission(&self, item_id: &str, perm_id: &str) -> Result<(), UploadError> {
        self.delete_url(&format!(
            "/me/drive/items/{}/permissions/{}",
            encode_id(item_id),
            encode_id(perm_id)
        ))
    }

    /// GET an arbitrary Graph URL and return the raw response body bytes
    /// (follows redirects to pre-signed download URLs). Used for binary/content
    /// endpoints like a drive item's `/content` or a message's `/$value` (MIME).
    /// `url` may be absolute or a `/me/...` path; a relative path is prefixed with
    /// the API base (like [`get_json`](Self::get_json)/[`post_json`](Self::post_json))
    /// — without this, a relative path (e.g. the OneNote page-content URL built by
    /// the archive driver) has no host and reqwest fails with a builder error.
    /// Send an **idempotent** GET through the central retry policy (#0.4): honor
    /// `Retry-After`, else exponential backoff (both capped), bounded attempts; log the
    /// Graph `request-id` for correlation (#0E). Rebuilds the request each attempt (a
    /// `RequestBuilder` is single-use). `url` must already be absolute.
    fn get_with_retry(&self, url: &str) -> Result<reqwest::blocking::Response, UploadError> {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let mut req = self.client.get(url).bearer_auth(&self.token);
            if self.prefer_immutable_id {
                req = req.header("Prefer", PREFER_IMMUTABLE_ID);
            }
            let resp = req.send().map_err(UploadError::from_reqwest)?;
            let status = resp.status().as_u16();
            if let Some(rid) = resp
                .headers()
                .get("request-id")
                .and_then(|v| v.to_str().ok())
            {
                // request-ids are correlation tokens, not secrets — safe to log (#0E).
                eprintln!("isyncyou-graph: GET status={status} request-id={rid} attempt={attempt}");
            }
            if is_retryable_status(status) && attempt < MAX_GET_ATTEMPTS {
                let wait = parse_retry_after(&resp)
                    .unwrap_or_else(|| backoff_delay(attempt))
                    .min(MAX_RETRY_WAIT);
                std::thread::sleep(wait);
                continue;
            }
            return Ok(resp);
        }
    }

    pub fn get_bytes(&self, url: &str) -> Result<Vec<u8>, UploadError> {
        let url = self.abs(url);
        let resp = self.get_with_retry(&url)?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(UploadError::Http {
                status,
                body: resp.text().unwrap_or_default().chars().take(300).collect(),
            });
        }
        resp.bytes()
            .map(|b| b.to_vec())
            .map_err(|e| UploadError::Transport(e.to_string()))
    }

    /// Reads a response body up to an explicit caller-owned limit. The limit is
    /// checked against both Content-Length and streamed bytes.
    pub fn get_bytes_bounded(&self, url: &str, max_bytes: usize) -> Result<Vec<u8>, UploadError> {
        use std::io::Read;

        let url = self.abs(url);
        let mut resp = self.get_with_retry(&url)?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(UploadError::Http {
                status,
                body: resp.text().unwrap_or_default().chars().take(300).collect(),
            });
        }
        if resp
            .content_length()
            .is_some_and(|length| length > max_bytes as u64)
        {
            return Err(UploadError::TooLarge);
        }
        let initial_capacity = resp
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or(0)
            .min(max_bytes);
        let mut out = Vec::with_capacity(initial_capacity);
        let mut chunk = [0u8; 16 * 1024];
        loop {
            let read = resp
                .read(&mut chunk)
                .map_err(|error| UploadError::Transport(error.to_string()))?;
            if read == 0 {
                break;
            }
            if out.len().saturating_add(read) > max_bytes {
                return Err(UploadError::TooLarge);
            }
            out.extend_from_slice(&chunk[..read]);
        }
        Ok(out)
    }

    /// Like [`get_bytes`](Self::get_bytes), but **streams** the body and reports the cumulative
    /// bytes read through `on_progress` as they arrive (#656 F-C: a moving materialize progress
    /// bar instead of 0%→100%). Redirect + retry handling is identical (`get_with_retry`); only
    /// the body read differs — chunked via `Read` instead of buffered by `bytes()`.
    pub fn get_bytes_with_progress(
        &self,
        url: &str,
        on_progress: &mut dyn FnMut(u64),
    ) -> Result<Vec<u8>, UploadError> {
        use std::io::Read;
        let url = self.abs(url);
        let mut resp = self.get_with_retry(&url)?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(UploadError::Http {
                status,
                body: resp.text().unwrap_or_default().chars().take(300).collect(),
            });
        }
        let mut out: Vec<u8> = Vec::with_capacity(resp.content_length().unwrap_or(0) as usize);
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = resp
                .read(&mut buf)
                .map_err(|e| UploadError::Transport(e.to_string()))?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            on_progress(out.len() as u64);
        }
        Ok(out)
    }

    /// GET a Graph resource as JSON (by-ref, unlike the `&mut self` [`Transport`]
    /// poll loop). `url` may be absolute or a `/me/...` path. Used to fetch a
    /// single item's canonical JSON for the content archive.
    pub fn get_json(&self, url: &str) -> Result<serde_json::Value, UploadError> {
        // The immutable-ID `Prefer` header (#565) is applied inside `get_with_retry`,
        // which also carries the central retry/Retry-After/backoff policy (#0.4).
        let url = self.abs(url);
        let resp = self.get_with_retry(&url)?;
        json_or_err(resp)
    }

    /// GET a Graph listing, following `@odata.nextLink` to completion and returning the
    /// concatenated `value` arrays (#0.4 paging — `get_json` returns only one page).
    /// Each page goes through the central retry policy. `nextLink` is absolute per Graph.
    pub fn get_json_paged(&self, url: &str) -> Result<Vec<serde_json::Value>, UploadError> {
        let mut items = Vec::new();
        let mut next = Some(self.abs(url));
        while let Some(u) = next {
            let page = json_or_err(self.get_with_retry(&u)?)?;
            if let Some(arr) = page.get("value").and_then(|v| v.as_array()) {
                items.extend(arr.iter().cloned());
            }
            next = page
                .get("@odata.nextLink")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
        }
        Ok(items)
    }

    /// Download a drive item's content by id (follows the redirect to the
    /// pre-signed download URL).
    pub fn download_content(&self, item_id: &str) -> Result<Vec<u8>, UploadError> {
        self.get_bytes(&format!("{}/me/drive/items/{item_id}/content", self.base))
    }

    /// [`download_content`](Self::download_content) with a cumulative-bytes progress callback,
    /// so the offline materialize can report a moving download bar (#656 F-C).
    pub fn download_content_with_progress(
        &self,
        item_id: &str,
        on_progress: &mut dyn FnMut(u64),
    ) -> Result<Vec<u8>, UploadError> {
        self.get_bytes_with_progress(
            &format!("{}/me/drive/items/{item_id}/content", self.base),
            on_progress,
        )
    }

    /// List a drive folder's children **live** from Graph, following
    /// `@odata.nextLink` to completion over the central retry policy (#0.4) — the
    /// Mode-1 (online) browse primitive. An empty `parent_id` = the drive root.
    /// `$select`s only the fields the browser UI needs (id/name/size/folder/file/
    /// lastModifiedDateTime); no store write happens here (this crate has no store
    /// dep). Returns every child across all pages.
    pub fn list_children(&self, parent_id: &str) -> Result<Vec<serde_json::Value>, UploadError> {
        // The `$select` query is appended as a literal — it must NOT go through
        // `encode_id`/`encode_seg`, which would percent-escape `$`, `=`, `,`.
        const SELECT: &str = "$select=id,name,size,folder,file,lastModifiedDateTime,eTag";
        let path = if parent_id.is_empty() {
            format!("/me/drive/root/children?{SELECT}")
        } else {
            format!("/me/drive/items/{}/children?{SELECT}", encode_id(parent_id))
        };
        self.get_json_paged(&path)
    }

    /// Download a mail message's full MIME (`.eml`) by id.
    pub fn download_message_mime(&self, message_id: &str) -> Result<Vec<u8>, UploadError> {
        self.get_bytes(&format!("{}/me/messages/{message_id}/$value", self.base))
    }

    /// POST a JSON body to a Graph collection and return the created resource
    /// (used by restore: re-create events/tasks/contacts). `url` may be absolute
    /// or a `/me/...` path (prefixed with the Graph base).
    pub fn post_json(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, UploadError> {
        self.post_json_with_status(url, body)
            .map(|(_, value)| value)
    }

    fn post_json_with_status(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<(u16, serde_json::Value), UploadError> {
        let url = self.abs(url);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .map_err(UploadError::from_reqwest)?;
        json_with_status_or_err(resp)
    }

    /// PATCH a JSON body onto a Graph resource and return the updated resource.
    /// `url` may be absolute or a `/me/...` path. Used to update a drive item's
    /// `fileSystemInfo` (preserve the local mtime on upload).
    pub fn patch_json(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, UploadError> {
        let url = self.abs(url);
        let resp = self
            .client
            .patch(&url)
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .map_err(UploadError::from_reqwest)?;
        json_or_err(resp)
    }

    /// POST a raw body with an explicit `Content-Type` and return the created
    /// resource. Used for endpoints that don't take JSON — e.g. creating a mail
    /// message from MIME (`text/plain` + base64 body).
    pub fn post_raw(
        &self,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<serde_json::Value, UploadError> {
        let url = self.abs(url);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(body)
            .send()
            .map_err(UploadError::from_reqwest)?;
        json_or_err(resp)
    }

    /// Create a mail message from its full MIME (`.eml`). Graph expects the MIME
    /// **base64-encoded** with `Content-Type: text/plain`; the message is created
    /// (in Drafts) and the created resource JSON returned.
    pub fn create_message_from_mime(&self, mime: &[u8]) -> Result<serde_json::Value, UploadError> {
        self.post_raw(
            "/me/messages",
            "text/plain",
            base64_encode(mime).into_bytes(),
        )
    }

    /// Create a OneNote page from its archived HTML (`POST /me/onenote/pages`,
    /// `Content-Type: text/html`), in the default section; returns the created
    /// page JSON. OneNote pages can't be re-created by a plain JSON POST like other
    /// items, so restore re-posts the page HTML (plan §6).
    pub fn create_onenote_page(&self, html: &[u8]) -> Result<serde_json::Value, UploadError> {
        self.post_raw("/me/onenote/pages", "text/html", html.to_vec())
    }

    /// Create a OneNote page from a `Presentation` HTML part plus binary resource
    /// parts. The HTML must reference each part as `name:<part-name>` in `img src`
    /// or `object data`, per Microsoft Graph's OneNote page-create contract.
    pub fn create_onenote_page_multipart(
        &self,
        html: &[u8],
        parts: &[OneNotePagePart],
    ) -> Result<serde_json::Value, UploadError> {
        let (content_type, body) = onenote_multipart_body(html, parts)?;
        self.post_raw("/me/onenote/pages", &content_type, body)
    }

    /// Delete a OneNote page by id (`DELETE /me/onenote/pages/{id}`). OneNote is
    /// eventually consistent, so a freshly-created page may 404 until it propagates;
    /// callers retry. Used for test cleanup on the throwaway account.
    pub fn delete_onenote_page(&self, page_id: &str) -> Result<(), UploadError> {
        let url = format!("{}/me/onenote/pages/{page_id}", self.base);
        let resp = self
            .client
            .delete(&url)
            .bearer_auth(&self.token)
            .send()
            .map_err(UploadError::from_reqwest)?;
        match resp.status().as_u16() {
            200 | 202 | 204 => Ok(()),
            s => Err(UploadError::Http {
                status: s,
                body: resp.text().unwrap_or_default().chars().take(200).collect(),
            }),
        }
    }

    /// Create a OneNote page **in a specific section** (`POST /me/onenote/sections/
    /// {section}/pages`, `text/html`) — the live-write / restore-to-original-section
    /// path (#568). Returns the created page JSON. OneNote ids are URL-path-safe and
    /// used raw, like `delete_onenote_page`.
    pub fn create_onenote_page_in_section(
        &self,
        section_id: &str,
        html: &[u8],
    ) -> Result<serde_json::Value, UploadError> {
        self.post_raw(
            &format!("/me/onenote/sections/{section_id}/pages"),
            "text/html",
            html.to_vec(),
        )
    }

    /// Create a OneNote page in a specific section from an HTML part plus binary
    /// resource parts (multipart; #568).
    pub fn create_onenote_page_in_section_multipart(
        &self,
        section_id: &str,
        html: &[u8],
        parts: &[OneNotePagePart],
    ) -> Result<serde_json::Value, UploadError> {
        let (content_type, body) = onenote_multipart_body(html, parts)?;
        self.post_raw(
            &format!("/me/onenote/sections/{section_id}/pages"),
            &content_type,
            body,
        )
    }

    /// Append/replace content on an existing OneNote page (`PATCH /me/onenote/pages/
    /// {id}/content`, #568). `commands` is the Graph OneNote update-command array
    /// (`[{ "target", "action", "content" }]`); Graph returns 204 on success.
    /// OneNote's content write is command-based (no full rewrite) — best-effort.
    pub fn append_onenote_page_content(
        &self,
        page_id: &str,
        commands: &serde_json::Value,
    ) -> Result<(), UploadError> {
        let url = format!("{}/me/onenote/pages/{page_id}/content", self.base);
        let body =
            serde_json::to_vec(commands).map_err(|e| UploadError::Transport(e.to_string()))?;
        let resp = self
            .client
            .patch(&url)
            .bearer_auth(&self.token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .map_err(UploadError::from_reqwest)?;
        match resp.status().as_u16() {
            200 | 202 | 204 => Ok(()),
            s => Err(UploadError::Http {
                status: s,
                body: resp.text().unwrap_or_default().chars().take(200).collect(),
            }),
        }
    }

    /// DELETE an arbitrary Graph resource (used for restore-test cleanup on the
    /// throwaway account). `url` may be absolute or a `/me/...` path.
    pub fn delete_url(&self, url: &str) -> Result<(), UploadError> {
        let url = self.abs(url);
        let resp = self
            .client
            .delete(&url)
            .bearer_auth(&self.token)
            .send()
            .map_err(UploadError::from_reqwest)?;
        match resp.status().as_u16() {
            200 | 202 | 204 => Ok(()),
            s => Err(UploadError::Http {
                status: s,
                body: resp.text().unwrap_or_default().chars().take(200).collect(),
            }),
        }
    }

    // ---- mail write layer (#561): the live client's verbs ---------------------
    //
    // Thin wrappers; every request body is built by a pure `*_body`/`send_envelope`
    // helper below so its exact shape is unit-testable without a network. Graph
    // *action* endpoints (sendMail/reply/replyAll/forward/send) answer 202 with no
    // body, so they go through `post_action`; the rest return the affected resource.

    /// POST a JSON body to a Graph **action** that returns no content (202/204 with
    /// an empty body — e.g. `sendMail`/`reply`). Unlike [`Self::post_json`] this
    /// never tries to parse a body.
    pub fn post_action(&self, url: &str, body: &serde_json::Value) -> Result<(), UploadError> {
        let url = self.abs(url);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .map_err(UploadError::from_reqwest)?;
        match resp.status().as_u16() {
            200 | 201 | 202 | 204 => Ok(()),
            s => Err(UploadError::Http {
                status: s,
                body: resp.text().unwrap_or_default().chars().take(300).collect(),
            }),
        }
    }

    /// POST with no body to a Graph action (the `send` draft action takes none).
    pub fn post_empty(&self, url: &str) -> Result<(), UploadError> {
        let url = self.abs(url);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            // An explicit empty body forces `Content-Length: 0`; without it some
            // Graph endpoints (e.g. `/messages/{id}/send`) reject the POST with
            // HTTP 411 Length Required.
            .body(Vec::<u8>::new())
            .send()
            .map_err(UploadError::from_reqwest)?;
        match resp.status().as_u16() {
            200 | 201 | 202 | 204 => Ok(()),
            s => Err(UploadError::Http {
                status: s,
                body: resp.text().unwrap_or_default().chars().take(300).collect(),
            }),
        }
    }

    /// Send a mail message (`POST /me/sendMail`). `message` is the Graph `message`
    /// resource the engine built; `save_to_sent` adds it to Sent Items.
    pub fn send_mail(
        &self,
        message: &serde_json::Value,
        save_to_sent: bool,
    ) -> Result<(), UploadError> {
        self.post_action("/me/sendMail", &send_envelope(message, save_to_sent))
    }

    /// Reply to the sender (`POST /me/messages/{id}/reply`).
    pub fn reply(&self, message_id: &str, comment: &str) -> Result<(), UploadError> {
        self.post_action(
            &format!("/me/messages/{}/reply", encode_id(message_id)),
            &comment_body(comment),
        )
    }

    /// Reply to all recipients (`POST /me/messages/{id}/replyAll`).
    pub fn reply_all(&self, message_id: &str, comment: &str) -> Result<(), UploadError> {
        self.post_action(
            &format!("/me/messages/{}/replyAll", encode_id(message_id)),
            &comment_body(comment),
        )
    }

    /// Forward a message to new recipients (`POST /me/messages/{id}/forward`).
    pub fn forward(&self, message_id: &str, comment: &str, to: &[&str]) -> Result<(), UploadError> {
        self.post_action(
            &format!("/me/messages/{}/forward", encode_id(message_id)),
            &forward_body(comment, to),
        )
    }

    /// Move a message to another folder (`POST /me/messages/{id}/move`); returns
    /// the moved message (it gets a new id in the destination folder).
    pub fn move_message(
        &self,
        message_id: &str,
        destination_id: &str,
    ) -> Result<serde_json::Value, UploadError> {
        self.post_json(
            &format!("/me/messages/{}/move", encode_id(message_id)),
            &move_body(destination_id),
        )
    }

    /// Mark a message read/unread (`PATCH /me/messages/{id}`).
    pub fn set_read(
        &self,
        message_id: &str,
        is_read: bool,
    ) -> Result<serde_json::Value, UploadError> {
        self.patch_json(
            &format!("/me/messages/{}", encode_id(message_id)),
            &read_body(is_read),
        )
    }

    /// Set/clear a follow-up flag (`PATCH /me/messages/{id}`); `status` is one of
    /// `notFlagged` / `flagged` / `complete`.
    pub fn set_flag(
        &self,
        message_id: &str,
        flag_status: &str,
        due: Option<&str>,
        tz: &str,
    ) -> Result<serde_json::Value, UploadError> {
        self.patch_json(
            &format!("/me/messages/{}", encode_id(message_id)),
            &flag_body(flag_status, due, tz),
        )
    }

    /// Replace a message's categories (`PATCH /me/messages/{id}`).
    pub fn set_categories(
        &self,
        message_id: &str,
        categories: &[String],
    ) -> Result<serde_json::Value, UploadError> {
        self.patch_json(
            &format!("/me/messages/{}", encode_id(message_id)),
            &categories_body(categories),
        )
    }

    /// Set a message's importance (`PATCH /me/messages/{id}`): `low`/`normal`/`high`.
    pub fn set_importance(
        &self,
        message_id: &str,
        importance: &str,
    ) -> Result<serde_json::Value, UploadError> {
        self.patch_json(
            &format!("/me/messages/{}", encode_id(message_id)),
            &importance_body(importance),
        )
    }

    /// Create a draft message (`POST /me/messages`); returns the created draft.
    pub fn create_draft(
        &self,
        message: &serde_json::Value,
    ) -> Result<serde_json::Value, UploadError> {
        self.post_json("/me/messages", message)
    }

    /// Update a draft message (`PATCH /me/messages/{id}`); returns the updated draft.
    pub fn update_draft(
        &self,
        message_id: &str,
        patch: &serde_json::Value,
    ) -> Result<serde_json::Value, UploadError> {
        self.patch_json(&format!("/me/messages/{}", encode_id(message_id)), patch)
    }

    /// Send an existing draft (`POST /me/messages/{id}/send`); no request body.
    pub fn send_draft(&self, message_id: &str) -> Result<(), UploadError> {
        self.post_empty(&format!("/me/messages/{}/send", encode_id(message_id)))
    }

    /// Create a reply **draft** to the sender (`POST /me/messages/{id}/createReply`);
    /// returns the new draft message (in the same conversation, pre-quoted). Used by
    /// the inline rich reply: PATCH the draft body to the user's full HTML, then send.
    pub fn create_reply(&self, message_id: &str) -> Result<serde_json::Value, UploadError> {
        self.post_json(
            &format!("/me/messages/{}/createReply", encode_id(message_id)),
            &serde_json::json!({}),
        )
    }

    /// Create a reply-all **draft** (`POST /me/messages/{id}/createReplyAll`).
    pub fn create_reply_all(&self, message_id: &str) -> Result<serde_json::Value, UploadError> {
        self.post_json(
            &format!("/me/messages/{}/createReplyAll", encode_id(message_id)),
            &serde_json::json!({}),
        )
    }

    /// Create a forward **draft** to new recipients (`POST /me/messages/{id}/createForward`);
    /// returns the new draft. PATCH the body, then send.
    pub fn create_forward(
        &self,
        message_id: &str,
        to: &[&str],
    ) -> Result<serde_json::Value, UploadError> {
        let recipients: Vec<_> = to
            .iter()
            .map(|a| serde_json::json!({ "emailAddress": { "address": a } }))
            .collect();
        self.post_json(
            &format!("/me/messages/{}/createForward", encode_id(message_id)),
            &serde_json::json!({ "toRecipients": recipients }),
        )
    }

    /// Create a calendar event (`POST /me/events`); returns the created event
    /// (#565). The body should already be a sanitized, writable event resource.
    pub fn create_event(
        &self,
        event: &serde_json::Value,
    ) -> Result<serde_json::Value, UploadError> {
        self.post_json("/me/events", event)
    }

    /// Update a calendar event (`PATCH /me/events/{id}`); returns the updated event.
    pub fn update_event(
        &self,
        event_id: &str,
        patch: &serde_json::Value,
    ) -> Result<serde_json::Value, UploadError> {
        self.patch_json(&format!("/me/events/{}", encode_id(event_id)), patch)
    }

    /// Delete a calendar event (`DELETE /me/events/{id}`).
    pub fn delete_event(&self, event_id: &str) -> Result<(), UploadError> {
        self.delete_url(&format!("/me/events/{}", encode_id(event_id)))
    }

    /// Respond to an event invitation (#565): `response` is `accept` / `decline`
    /// / `tentative`, mapped to the Graph action `POST /me/events/{id}/{action}`
    /// with an optional comment; the response email is always sent.
    pub fn respond_event(
        &self,
        event_id: &str,
        response: &str,
        comment: &str,
    ) -> Result<(), UploadError> {
        let action = match response {
            "decline" | "declined" => "decline",
            "tentative" | "tentativelyAccepted" => "tentativelyAccept",
            _ => "accept",
        };
        let body = if comment.is_empty() {
            serde_json::json!({ "sendResponse": true })
        } else {
            serde_json::json!({ "comment": comment, "sendResponse": true })
        };
        self.post_action(
            &format!("/me/events/{}/{}", encode_id(event_id), action),
            &body,
        )
    }

    /// Create a personal contact (`POST /me/contacts`); returns the created
    /// contact (#566). The body should already be a sanitized, writable resource.
    pub fn create_contact(
        &self,
        contact: &serde_json::Value,
    ) -> Result<serde_json::Value, UploadError> {
        self.post_json("/me/contacts", contact)
    }

    /// Update a personal contact (`PATCH /me/contacts/{id}`); returns the updated
    /// contact.
    pub fn update_contact(
        &self,
        contact_id: &str,
        patch: &serde_json::Value,
    ) -> Result<serde_json::Value, UploadError> {
        self.patch_json(&format!("/me/contacts/{}", encode_id(contact_id)), patch)
    }

    /// Delete a personal contact (`DELETE /me/contacts/{id}`).
    pub fn delete_contact(&self, contact_id: &str) -> Result<(), UploadError> {
        self.delete_url(&format!("/me/contacts/{}", encode_id(contact_id)))
    }

    // ---- Microsoft To Do write verbs (#567 B5) — ids percent-encoded ----

    /// Create a task in a list (`POST /me/todo/lists/{list}/tasks`); returns it.
    pub fn create_task(
        &self,
        list_id: &str,
        task: &serde_json::Value,
    ) -> Result<serde_json::Value, UploadError> {
        self.post_json(
            &format!("/me/todo/lists/{}/tasks", encode_id(list_id)),
            task,
        )
    }

    /// Update a task's writable fields (`PATCH /me/todo/lists/{list}/tasks/{id}`).
    pub fn update_task(
        &self,
        list_id: &str,
        task_id: &str,
        patch: &serde_json::Value,
    ) -> Result<serde_json::Value, UploadError> {
        self.patch_json(
            &format!(
                "/me/todo/lists/{}/tasks/{}",
                encode_id(list_id),
                encode_id(task_id)
            ),
            patch,
        )
    }

    /// Delete a task (`DELETE /me/todo/lists/{list}/tasks/{id}`).
    pub fn delete_task(&self, list_id: &str, task_id: &str) -> Result<(), UploadError> {
        self.delete_url(&format!(
            "/me/todo/lists/{}/tasks/{}",
            encode_id(list_id),
            encode_id(task_id)
        ))
    }

    /// Add a checklist item to a task (`POST .../tasks/{id}/checklistItems`).
    pub fn create_checklist_item(
        &self,
        list_id: &str,
        task_id: &str,
        item: &serde_json::Value,
    ) -> Result<serde_json::Value, UploadError> {
        self.post_json(
            &format!(
                "/me/todo/lists/{}/tasks/{}/checklistItems",
                encode_id(list_id),
                encode_id(task_id)
            ),
            item,
        )
    }

    /// Update a checklist item (`PATCH .../checklistItems/{cid}`).
    pub fn update_checklist_item(
        &self,
        list_id: &str,
        task_id: &str,
        item_id: &str,
        patch: &serde_json::Value,
    ) -> Result<serde_json::Value, UploadError> {
        self.patch_json(
            &format!(
                "/me/todo/lists/{}/tasks/{}/checklistItems/{}",
                encode_id(list_id),
                encode_id(task_id),
                encode_id(item_id)
            ),
            patch,
        )
    }

    /// Delete a checklist item (`DELETE .../checklistItems/{cid}`).
    pub fn delete_checklist_item(
        &self,
        list_id: &str,
        task_id: &str,
        item_id: &str,
    ) -> Result<(), UploadError> {
        self.delete_url(&format!(
            "/me/todo/lists/{}/tasks/{}/checklistItems/{}",
            encode_id(list_id),
            encode_id(task_id),
            encode_id(item_id)
        ))
    }

    /// Create a task list (`POST /me/todo/lists`); returns the created list.
    pub fn create_todo_list(
        &self,
        list: &serde_json::Value,
    ) -> Result<serde_json::Value, UploadError> {
        self.post_json("/me/todo/lists", list)
    }

    /// Delete a task list (`DELETE /me/todo/lists/{id}`).
    pub fn delete_todo_list(&self, list_id: &str) -> Result<(), UploadError> {
        self.delete_url(&format!("/me/todo/lists/{}", encode_id(list_id)))
    }
}

// ---- mail-write request-body builders (pure; unit-tested for exact shape) ----

/// `{ "emailAddress": { "address": addr } }` — a Graph recipient.
fn mail_recipient(addr: &str) -> serde_json::Value {
    serde_json::json!({ "emailAddress": { "address": addr } })
}
/// `sendMail` envelope: `{ "message": <message>, "saveToSentItems": <bool> }`.
fn send_envelope(message: &serde_json::Value, save_to_sent: bool) -> serde_json::Value {
    serde_json::json!({ "message": message, "saveToSentItems": save_to_sent })
}
/// reply/replyAll body: `{ "comment": <text> }`.
fn comment_body(comment: &str) -> serde_json::Value {
    serde_json::json!({ "comment": comment })
}
/// forward body: `{ "comment": <text>, "toRecipients": [ … ] }`.
fn forward_body(comment: &str, to: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "comment": comment,
        "toRecipients": to.iter().map(|a| mail_recipient(a)).collect::<Vec<_>>(),
    })
}
/// move body: `{ "destinationId": <folder-id> }`.
fn move_body(destination_id: &str) -> serde_json::Value {
    serde_json::json!({ "destinationId": destination_id })
}
/// read PATCH body: `{ "isRead": <bool> }`.
fn read_body(is_read: bool) -> serde_json::Value {
    serde_json::json!({ "isRead": is_read })
}
/// flag PATCH body: `{ "flag": { "flagStatus": <status> } }`.
fn flag_body(flag_status: &str, due: Option<&str>, tz: &str) -> serde_json::Value {
    let mut flag = serde_json::json!({ "flagStatus": flag_status });
    if let Some(d) = due {
        // Graph rejects dueDateTime without startDateTime, so set both.
        let dt = serde_json::json!({ "dateTime": d, "timeZone": tz });
        flag["startDateTime"] = dt.clone();
        flag["dueDateTime"] = dt;
    }
    serde_json::json!({ "flag": flag })
}
/// categories PATCH body: `{ "categories": [ … ] }`.
fn categories_body(categories: &[String]) -> serde_json::Value {
    serde_json::json!({ "categories": categories })
}
/// importance PATCH body: `{ "importance": <level> }`.
fn importance_body(importance: &str) -> serde_json::Value {
    serde_json::json!({ "importance": importance })
}

/// Build the raw multipart/form-data body Graph expects for OneNote page create
/// with binary resources. Pure and unit-testable; callers still own rewriting the
/// archived page HTML to `name:<part-name>` references.
pub fn onenote_multipart_body(
    html: &[u8],
    parts: &[OneNotePagePart],
) -> Result<(String, Vec<u8>), UploadError> {
    for part in parts {
        validate_multipart_token(&part.name, "part name")?;
        validate_content_type(&part.content_type)?;
    }
    let boundary = multipart_boundary(html, parts);
    let mut body = Vec::new();
    write_part(
        &mut body,
        &boundary,
        "Presentation",
        "text/html; charset=utf-8",
        html,
    );
    for part in parts {
        write_part(
            &mut body,
            &boundary,
            &part.name,
            &part.content_type,
            &part.bytes,
        );
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Ok((format!("multipart/form-data; boundary={boundary}"), body))
}

fn write_part(body: &mut Vec<u8>, boundary: &str, name: &str, content_type: &str, bytes: &[u8]) {
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"{name}\"\r\n").as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n");
}

fn validate_multipart_token(value: &str, label: &str) -> Result<(), UploadError> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(UploadError::Parse(format!(
            "invalid OneNote multipart {label}: {value:?}"
        )));
    }
    Ok(())
}

fn validate_content_type(value: &str) -> Result<(), UploadError> {
    if value.is_empty() || value.bytes().any(|b| matches!(b, b'\r' | b'\n' | b'"')) {
        return Err(UploadError::Parse(format!(
            "invalid OneNote multipart content type: {value:?}"
        )));
    }
    Ok(())
}

fn multipart_boundary(html: &[u8], parts: &[OneNotePagePart]) -> String {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in html {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    for part in parts {
        for b in part.name.as_bytes().iter().chain(part.bytes.iter()) {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("isyncyou-{h:016x}")
}

/// Standard base64 (RFC 4648, with padding). Small + dependency-free; used to
/// encode MIME for `POST /me/messages`.
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn json_or_err(resp: reqwest::blocking::Response) -> Result<serde_json::Value, UploadError> {
    json_with_status_or_err(resp).map(|(_, value)| value)
}

fn json_with_status_or_err(
    resp: reqwest::blocking::Response,
) -> Result<(u16, serde_json::Value), UploadError> {
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        let value = resp
            .json::<serde_json::Value>()
            .map_err(|e| UploadError::Parse(e.to_string()))?;
        Ok((status, value))
    } else {
        let body = resp.text().unwrap_or_default();
        Err(UploadError::Http {
            status,
            body: body.chars().take(300).collect(),
        })
    }
}

fn permission_ids_from_invite_value(value: &serde_json::Value) -> Vec<String> {
    value
        .get("value")
        .and_then(|items| items.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|permission| {
                    permission
                        .get("id")
                        .and_then(|id| id.as_str())
                        .map(String::from)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Minimal path encoding for OneDrive `root:/PATH:` addressing (spaces only;
/// callers use safe names). Full percent-encoding is a later refinement.
fn enc(path: &str) -> String {
    path.trim_start_matches('/').replace(' ', "%20")
}

/// Percent-encode one path segment: every byte outside the RFC 3986 unreserved
/// set is escaped over its UTF-8 bytes (so `:`, `#`, `?`, `%`, spaces and
/// non-ASCII like umlauts are all made safe). The shared core of [`encode_id`]
/// and [`enc_path`].
fn encode_seg(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for b in seg.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Percent-encode an item id for safe inclusion in a URL path segment. Outlook
/// message ids are base64-ish (contain `+ / =`), which Graph 404s if left raw in
/// the path; everything outside RFC 3986 unreserved is escaped. Plain alphanumeric
/// ids (e.g. OneDrive drive-item ids) pass through unchanged.
fn encode_id(id: &str) -> String {
    encode_seg(id)
}

/// Percent-encode a drive-relative path for `root:/{path}:` addressing: split on
/// `/`, encode each segment with [`encode_seg`] (so arbitrary OneDrive names —
/// `:`, spaces, umlauts — resolve), re-join with `/`, strip any leading `/`.
/// Unlike [`enc`] (space-only) this is safe for user-chosen names.
fn enc_path(path: &str) -> String {
    path.trim_matches('/')
        .split('/')
        .map(encode_seg)
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_transient_failure_is_structured_without_message_matching() {
        assert_eq!(
            UploadError::Transport("arbitrary".into()).transient_failure(),
            Some(GraphTransientFailure::Network)
        );
        assert_eq!(
            UploadError::Timeout("arbitrary".into()).transient_failure(),
            Some(GraphTransientFailure::Timeout)
        );
        assert_eq!(
            UploadError::Http {
                status: 425,
                body: "arbitrary".into()
            }
            .transient_failure(),
            Some(GraphTransientFailure::Http(425))
        );
        assert_eq!(
            UploadError::Http {
                status: 403,
                body: "timeout network 503".into()
            }
            .transient_failure(),
            None
        );
    }
    use crate::run_delta;

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648 test vectors.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn flag_body_sets_due_with_required_start() {
        // status-only when no due date is given
        assert_eq!(
            flag_body("flagged", None, "UTC"),
            serde_json::json!({ "flag": { "flagStatus": "flagged" } })
        );
        assert_eq!(
            flag_body("complete", None, "UTC"),
            serde_json::json!({ "flag": { "flagStatus": "complete" } })
        );
        // a due date also sets startDateTime — Graph 400s on dueDateTime alone
        let b = flag_body("flagged", Some("2026-07-01T09:00:00"), "Europe/Vienna");
        let f = &b["flag"];
        assert_eq!(f["flagStatus"], "flagged");
        let dt =
            serde_json::json!({ "dateTime": "2026-07-01T09:00:00", "timeZone": "Europe/Vienna" });
        assert_eq!(f["dueDateTime"], dt);
        assert_eq!(f["startDateTime"], dt);
    }

    #[test]
    fn abs_prefixes_paths_but_not_urls() {
        let c = GraphClient::new("t");
        assert_eq!(c.abs("/me/events"), format!("{GRAPH}/me/events"));
        assert_eq!(c.abs("https://x/y"), "https://x/y");
        let local = GraphClient::new("t").with_base_url("http://127.0.0.1:1");
        assert_eq!(local.abs("/me/events"), "http://127.0.0.1:1/me/events");
    }

    #[test]
    fn onenote_multipart_body_uses_presentation_and_binary_parts() {
        let html = br#"<!DOCTYPE html><html><body><img src="name:imageBlock1" /></body></html>"#;
        let parts = vec![OneNotePagePart {
            name: "imageBlock1".into(),
            content_type: "image/png".into(),
            bytes: b"\x89PNG\r\nbinary".to_vec(),
        }];

        let (content_type, body) = onenote_multipart_body(html, &parts).unwrap();
        assert!(content_type.starts_with("multipart/form-data; boundary=isyncyou-"));
        let boundary = content_type.split("boundary=").nth(1).unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains(&format!("--{boundary}\r\n")));
        assert!(text.contains("Content-Disposition: form-data; name=\"Presentation\"\r\n"));
        assert!(text.contains("Content-Type: text/html; charset=utf-8\r\n\r\n"));
        assert!(text.contains("Content-Disposition: form-data; name=\"imageBlock1\"\r\n"));
        assert!(text.contains("Content-Type: image/png\r\n\r\n"));
        assert!(body.ends_with(format!("--{boundary}--\r\n").as_bytes()));
        assert!(body
            .windows(b"\x89PNG\r\nbinary".len())
            .any(|w| w == b"\x89PNG\r\nbinary"));
    }

    // ---- deterministic transport tests against a local mock HTTP server ------
    //
    // std-only one-shot HTTP/1.1 server: serves a fixed sequence of canned
    // responses (one connection per response) and records each request head so
    // tests can assert on method/path/headers. No live account, no extra deps.

    /// Read one HTTP request (head + `Content-Length` body) from the socket and
    /// return the head text. The body must be consumed, or large uploads would
    /// deadlock on a full TCP buffer.
    fn read_request(sock: &mut std::net::TcpStream) -> String {
        use std::io::Read;
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") {
            if sock.read(&mut byte).unwrap_or(0) == 0 {
                break;
            }
            buf.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&buf).to_string();
        let content_length = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(str::to_owned)
            })
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            sock.read_exact(&mut body).unwrap();
        }
        if body.is_empty() {
            head
        } else {
            format!("{head}\n{}", String::from_utf8_lossy(&body))
        }
    }

    /// Serve `responses` verbatim, one connection each; returns the base URL and
    /// a handle yielding the recorded request heads.
    fn serve(responses: Vec<String>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for resp in responses {
                let (mut sock, _) = listener.accept().unwrap();
                seen.push(read_request(&mut sock));
                use std::io::Write;
                sock.write_all(resp.as_bytes()).unwrap();
            }
            seen
        });
        (format!("http://{addr}"), handle)
    }

    fn http_response(status: u16, reason: &str, extra_headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// In-memory `UploadResumeStore` so the resumable-upload path is drivable
    /// against the mock server (the persisted `uploadUrl` points at it).
    #[derive(Default)]
    struct MemResume(std::sync::Mutex<std::collections::HashMap<String, (String, u64, u64)>>);
    impl crate::UploadResumeStore for MemResume {
        fn load(&self, dest: &str) -> Option<(String, u64)> {
            self.0
                .lock()
                .unwrap()
                .get(dest)
                .map(|(u, t, _)| (u.clone(), *t))
        }
        fn save(&self, dest: &str, upload_url: &str, total: u64, next_offset: u64) {
            self.0
                .lock()
                .unwrap()
                .insert(dest.into(), (upload_url.into(), total, next_offset));
        }
        fn clear(&self, dest: &str) {
            self.0.lock().unwrap().remove(dest);
        }
    }

    #[test]
    fn transport_get_surfaces_429_with_retry_after() {
        let (base, server) = serve(vec![http_response(
            429,
            "Too Many Requests",
            "Retry-After: 7\r\n",
            "{}",
        )]);
        let mut c = GraphClient::new("tok");
        let resp = Transport::get(&mut c, &base);
        assert_eq!(resp.status, 429);
        assert_eq!(resp.retry_after, Some(Duration::from_secs(7)));
        let seen = server.join().unwrap();
        assert!(seen[0].contains("Bearer tok"), "missing bearer auth");
    }

    #[test]
    fn transport_get_sends_prefer_immutable_id_header_when_enabled() {
        let (base, server) = serve(vec![http_response(200, "OK", "", "{\"value\":[]}")]);
        let mut c = GraphClient::new("tok");
        Transport::set_prefer_immutable_id(&mut c, true);
        let resp = Transport::get(&mut c, &base);
        assert_eq!(resp.status, 200);
        assert!(resp.body.is_some());
        let seen = server.join().unwrap();
        assert!(
            seen[0].contains(PREFER_IMMUTABLE_ID),
            "missing Prefer header"
        );
    }

    #[test]
    fn transport_get_maps_network_failure_to_retryable_503() {
        // bind + drop: the port is closed, so the connection is refused.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let mut c = GraphClient::new("tok");
        let resp = Transport::get(&mut c, &format!("http://{addr}/x"));
        assert_eq!(
            resp.status, 503,
            "transport failure must map to retryable 503"
        );
        assert!(resp.body.is_none());
    }

    #[test]
    fn get_json_retries_on_429_then_succeeds() {
        // Central retry policy (#0.4): an idempotent GET honors Retry-After and retries.
        let (base, server) = serve(vec![
            http_response(429, "Too Many Requests", "Retry-After: 0\r\n", "{}"),
            http_response(200, "OK", "", r#"{"ok":true}"#),
        ]);
        let c = GraphClient::new("tok");
        let v = c.get_json(&base).unwrap();
        assert_eq!(v["ok"], true);
        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 2, "must have retried the 429 exactly once");
    }

    #[test]
    fn writes_do_not_blind_retry_on_429() {
        // Non-idempotent writes must be single-shot — a blind re-POST could double-apply;
        // their recovery is ledger-driven (#0D), not a retry here.
        let (base, server) = serve(vec![http_response(
            429,
            "Too Many Requests",
            "Retry-After: 0\r\n",
            "{}",
        )]);
        let c = GraphClient::new("tok");
        let r = c.post_json(&base, &serde_json::json!({"x": 1}));
        assert!(
            matches!(r, Err(UploadError::Http { status: 429, .. })),
            "a throttled write must surface 429, not retry"
        );
        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 1, "a write must not be re-sent");
    }

    #[test]
    fn get_json_paged_follows_odata_nextlink() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let page1 = http_response(
            200,
            "OK",
            "",
            &format!(r#"{{"value":[1,2],"@odata.nextLink":"{base}/p2"}}"#),
        );
        let page2 = http_response(200, "OK", "", r#"{"value":[3]}"#);
        let handle = std::thread::spawn(move || {
            for resp in [page1, page2] {
                let (mut sock, _) = listener.accept().unwrap();
                read_request(&mut sock);
                sock.write_all(resp.as_bytes()).unwrap();
            }
        });
        let c = GraphClient::new("tok");
        let items = c.get_json_paged(&format!("{base}/p1")).unwrap();
        assert_eq!(
            items.len(),
            3,
            "both pages' value arrays must be concatenated"
        );
        handle.join().unwrap();
    }

    #[test]
    fn client_times_out_on_a_hung_connection() {
        // #0.4: a request timeout means a server that accepts but never answers can't
        // wedge the caller. Uses a short-timeout client to prove the mechanism quickly;
        // `GraphClient::new` sets the production timeout via the same builder.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _hang = std::thread::spawn(move || {
            let _s = listener.accept();
            std::thread::sleep(Duration::from_secs(3));
        });
        let short = reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(300))
            .build()
            .unwrap();
        let c = GraphClient::with_client(short, "tok");
        let start = std::time::Instant::now();
        let r = c.get_bytes(&format!("http://{addr}/x"));
        assert!(
            r.is_err(),
            "a hung connection must time out, not block forever"
        );
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "the timeout must fire promptly (was {:?})",
            start.elapsed()
        );
    }

    #[test]
    fn get_json_classifies_4xx_with_truncated_body() {
        let long_body = "e".repeat(900);
        let (base, _server) = serve(vec![http_response(403, "Forbidden", "", &long_body)]);
        let err = GraphClient::new("tok").get_json(&base).unwrap_err();
        match err {
            UploadError::Http { status, body } => {
                assert_eq!(status, 403);
                assert_eq!(body.len(), 300, "error body must be truncated to 300 chars");
            }
            other => panic!("expected Http error, got {other}"),
        }
    }

    #[test]
    fn get_json_malformed_json_is_a_parse_error_not_a_panic() {
        let (base, _server) = serve(vec![http_response(200, "OK", "", "this is not json")]);
        let err = GraphClient::new("tok").get_json(&base).unwrap_err();
        assert!(matches!(err, UploadError::Parse(_)), "got {err}");
    }

    #[test]
    fn get_bytes_returns_content_and_classifies_5xx() {
        let (base, _s1) = serve(vec![http_response(200, "OK", "", "raw-bytes-here")]);
        assert_eq!(
            GraphClient::new("tok").get_bytes(&base).unwrap(),
            b"raw-bytes-here"
        );
        // A 5xx on an idempotent GET is now retried (#0.4); when it persists across the
        // bounded attempts it surfaces as an Http error. Serve one 503 per attempt
        // (Retry-After: 0 keeps the test fast).
        let five_oh_three = || http_response(503, "Unavailable", "Retry-After: 0\r\n", "busy");
        let (base2, _s2) = serve((0..MAX_GET_ATTEMPTS).map(|_| five_oh_three()).collect());
        match GraphClient::new("tok").get_bytes(&base2).unwrap_err() {
            UploadError::Http { status, .. } => assert_eq!(status, 503),
            other => panic!("expected Http error after retries, got {other}"),
        }
    }

    #[test]
    fn get_bytes_with_progress_streams_body_and_reports_cumulative() {
        // #656 F-C: the streamed variant returns the same bytes as `get_bytes`, and reports
        // monotonic cumulative progress ending at the full length.
        let (base, _s) = serve(vec![http_response(200, "OK", "", "raw-bytes-here")]);
        let mut seen: Vec<u64> = Vec::new();
        let out = GraphClient::new("tok")
            .get_bytes_with_progress(&base, &mut |n| seen.push(n))
            .unwrap();
        assert_eq!(out, b"raw-bytes-here");
        assert!(!seen.is_empty(), "progress must be reported");
        assert_eq!(
            *seen.last().unwrap(),
            out.len() as u64,
            "final progress equals the byte count"
        );
        assert!(
            seen.windows(2).all(|w| w[0] <= w[1]),
            "cumulative progress is non-decreasing"
        );
    }

    #[test]
    fn get_bytes_prefixes_a_relative_path_with_the_base() {
        // A relative `/me/...` path (e.g. the OneNote page-content URL the archive
        // driver builds) must be prefixed with the API base; otherwise reqwest has
        // no host and fails with a builder error (regression: OneNote body backup).
        let (base, _s) = serve(vec![http_response(200, "OK", "", "page-html")]);
        let client = GraphClient::new("tok").with_base_url(base);
        assert_eq!(
            client
                .get_bytes("/me/onenote/pages/p1/content")
                .expect("relative path must resolve against the base, not builder-error"),
            b"page-html"
        );
    }

    #[test]
    fn upload_status_parses_next_expected_offset_and_classifies_errors() {
        let (base, _s) = serve(vec![http_response(
            200,
            "OK",
            "",
            "{\"nextExpectedRanges\":[\"327680-983039\"]}",
        )]);
        assert_eq!(
            GraphClient::new("tok").upload_status(&base).unwrap(),
            327_680
        );

        // expired/unknown session → Http error, not a bogus offset
        let (base2, _s2) = serve(vec![http_response(404, "Not Found", "", "")]);
        match GraphClient::new("tok").upload_status(&base2).unwrap_err() {
            UploadError::Http { status, .. } => assert_eq!(status, 404),
            other => panic!("expected Http error, got {other}"),
        }
    }

    #[test]
    fn resumable_upload_resumes_from_server_offset_and_completes() {
        // 3 * CHUNK_ALIGN bytes, max_chunk 2 * CHUNK_ALIGN → chunk1 = 640 KiB,
        // chunk2 = 320 KiB. The persisted session points at the mock server, so
        // the whole resume path (status probe → chunked PUTs → completion) runs
        // deterministically without Graph.
        let total = 3 * CHUNK_ALIGN;
        let data = vec![0xA5u8; total as usize];
        let chunk2_start = 2 * CHUNK_ALIGN;
        let (base, server) = serve(vec![
            // status probe: server expects from 0
            http_response(200, "OK", "", "{\"nextExpectedRanges\":[\"0-\"]}"),
            // chunk 1 accepted, server asks for the rest
            http_response(
                202,
                "Accepted",
                "",
                &format!("{{\"nextExpectedRanges\":[\"{chunk2_start}-\"]}}"),
            ),
            // chunk 2 completes the file
            http_response(201, "Created", "", "{\"id\":\"item-done\",\"size\":983040}"),
        ]);
        let resume = MemResume::default();
        crate::UploadResumeStore::save(&resume, "/big.bin", &base, total, 0);

        let out = GraphClient::new("tok")
            .upload_file_resumable("/big.bin", &data, 2 * CHUNK_ALIGN, &resume)
            .unwrap();
        assert_eq!(out["id"].as_str(), Some("item-done"));
        // completed → persisted session dropped
        assert!(crate::UploadResumeStore::load(&resume, "/big.bin").is_none());

        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(
            seen[0].starts_with("GET"),
            "first request is the status probe"
        );
        let expect_range1 = format!("bytes 0-{}/{}", chunk2_start - 1, total);
        assert!(
            seen[1].contains(&expect_range1),
            "chunk 1 Content-Range wrong: {}",
            seen[1].lines().find(|l| l.contains("range")).unwrap_or("?")
        );
        let expect_range2 = format!("bytes {}-{}/{}", chunk2_start, total - 1, total);
        assert!(
            seen[2].contains(&expect_range2),
            "chunk 2 Content-Range wrong"
        );
    }

    #[test]
    fn resumable_upload_chunk_error_is_classified_and_keeps_the_session() {
        let total = 2 * CHUNK_ALIGN;
        let data = vec![1u8; total as usize];
        let (base, _server) = serve(vec![
            http_response(200, "OK", "", "{\"nextExpectedRanges\":[\"0-\"]}"),
            // storage exhausted mid-upload
            http_response(507, "Insufficient Storage", "", "quota exceeded"),
        ]);
        let resume = MemResume::default();
        crate::UploadResumeStore::save(&resume, "/big.bin", &base, total, 0);

        match GraphClient::new("tok")
            .upload_file_resumable("/big.bin", &data, total, &resume)
            .unwrap_err()
        {
            UploadError::Http { status, body } => {
                assert_eq!(status, 507);
                assert!(body.contains("quota exceeded"));
            }
            other => panic!("expected Http error, got {other}"),
        }
        // the persisted session survives a failed chunk, so a retry can resume
        assert!(crate::UploadResumeStore::load(&resume, "/big.bin").is_some());
    }

    // ---- base-bound methods via with_base_url -------------------------------

    #[test]
    fn small_upload_takes_the_single_put_path() {
        let (base, server) = serve(vec![http_response(
            201,
            "Created",
            "",
            "{\"id\":\"small-1\",\"size\":5}",
        )]);
        let c = GraphClient::new("tok").with_base_url(&base);
        // ≤ CHUNK_ALIGN → upload_file short-circuits to simple_upload: one PUT.
        let out = c
            .upload_file("/a dir/x.txt", b"hello", CHUNK_ALIGN)
            .unwrap();
        assert_eq!(out["id"].as_str(), Some("small-1"));
        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 1);
        // space is encoded; the OneDrive path addressing form is used
        assert!(seen[0].starts_with("PUT /me/drive/root:/a%20dir/x.txt:/content"));
    }

    #[test]
    fn get_bytes_bounded_rejects_declared_and_streamed_overflow() {
        let (base, _server) = serve(vec![http_response(200, "OK", "", "12345")]);
        let error = GraphClient::new("tok")
            .with_base_url(&base)
            .get_bytes_bounded("/bounded", 4)
            .unwrap_err();
        assert!(matches!(error, UploadError::TooLarge));

        let response = "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n12345".to_owned();
        let (base, _server) = serve(vec![response]);
        let error = GraphClient::new("tok")
            .with_base_url(&base)
            .get_bytes_bounded("/bounded", 4)
            .unwrap_err();
        assert!(matches!(error, UploadError::TooLarge));
    }

    #[test]
    fn create_upload_session_parses_upload_url_and_rejects_missing_one() {
        let (base, _s) = serve(vec![http_response(
            200,
            "OK",
            "",
            "{\"uploadUrl\":\"http://session.local/u1\"}",
        )]);
        let s = GraphClient::new("tok")
            .with_base_url(&base)
            .create_upload_session("/big.bin", 999)
            .unwrap();
        assert_eq!(s.upload_url, "http://session.local/u1");

        let (base2, _s2) = serve(vec![http_response(200, "OK", "", "{\"ok\":true}")]);
        let err = GraphClient::new("tok")
            .with_base_url(&base2)
            .create_upload_session("/big.bin", 999)
            .unwrap_err();
        assert!(matches!(err, UploadError::Parse(_)), "got {err}");
    }

    #[test]
    fn replace_content_if_match_returns_none_on_412_conflict() {
        // 412 Precondition Failed = the cloud changed → conflict, never clobber (A3)
        let (base, server) = serve(vec![http_response(412, "Precondition Failed", "", "")]);
        let out = GraphClient::new("tok")
            .with_base_url(&base)
            .replace_content_if_match("item9", b"data", "\"etag-1\"")
            .unwrap();
        assert!(out.is_none(), "412 must surface as None, not an item");
        let seen = server.join().unwrap();
        assert!(seen[0].contains("if-match: \"etag-1\""), "missing If-Match");
    }

    #[test]
    fn replace_content_if_match_returns_item_on_200_and_error_otherwise() {
        let (base, _s) = serve(vec![http_response(200, "OK", "", "{\"id\":\"item9\"}")]);
        let out = GraphClient::new("tok")
            .with_base_url(&base)
            .replace_content_if_match("item9", b"data", "\"e\"")
            .unwrap();
        assert_eq!(out.unwrap()["id"].as_str(), Some("item9"));

        let (base2, _s2) = serve(vec![http_response(423, "Locked", "", "locked by office")]);
        match GraphClient::new("tok")
            .with_base_url(&base2)
            .replace_content_if_match("item9", b"data", "\"e\"")
            .unwrap_err()
        {
            UploadError::Http { status, body } => {
                assert_eq!(status, 423);
                assert!(body.contains("locked"));
            }
            other => panic!("expected Http error, got {other}"),
        }
    }

    #[test]
    fn upload_content_with_conflict_behavior_puts_query_not_json() {
        let (base, server) = serve(vec![http_response(201, "Created", "", "{\"id\":\"t1\"}")]);
        let out = GraphClient::new("tok")
            .with_base_url(&base)
            .upload_content_with_conflict_behavior(
                "/Apps/iSyncYou/agent/sess1/turn one.json",
                b"sealed",
                ConflictBehavior::Fail,
            )
            .unwrap()
            .unwrap();
        assert_eq!(out["id"].as_str(), Some("t1"));
        let seen = server.join().unwrap();
        assert!(
            seen[0].contains(
                "PUT /me/drive/root:/Apps/iSyncYou/agent/sess1/turn%20one.json:/content?@microsoft.graph.conflictBehavior=fail "
            ),
            "unexpected request: {}",
            seen[0]
        );
        assert!(!seen[0].contains("@microsoft.graph.conflictBehavior\":\"fail"));
        assert!(seen[0].contains("sealed"));
    }

    #[test]
    fn upload_content_with_conflict_behavior_returns_none_on_409() {
        let (base, _server) = serve(vec![http_response(409, "Conflict", "", "exists")]);
        let out = GraphClient::new("tok")
            .with_base_url(&base)
            .upload_content_with_conflict_behavior(
                "/Apps/iSyncYou/agent/sess1/turn.json",
                b"sealed",
                ConflictBehavior::Fail,
            )
            .unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn get_drive_item_by_path_encodes_path_and_selects_fields() {
        let (base, server) = serve(vec![http_response(
            200,
            "OK",
            "",
            "{\"id\":\"lease\",\"eTag\":\"\\\"e\\\"\"}",
        )]);
        let out = GraphClient::new("tok")
            .with_base_url(&base)
            .get_drive_item_by_path(
                "/Apps/iSyncYou/agent/sess1/.active_turn_lease.json",
                &["id", "eTag", "name"],
            )
            .unwrap()
            .unwrap();
        assert_eq!(out["id"].as_str(), Some("lease"));
        let seen = server.join().unwrap();
        assert!(
            seen[0].contains(
                "GET /me/drive/root:/Apps/iSyncYou/agent/sess1/.active_turn_lease.json:?$select=id,eTag,name "
            ),
            "unexpected request: {}",
            seen[0]
        );
    }

    #[test]
    fn delete_item_if_match_sends_header_and_returns_false_on_412() {
        let (base, server) = serve(vec![http_response(412, "Precondition Failed", "", "")]);
        let deleted = GraphClient::new("tok")
            .with_base_url(&base)
            .delete_item_if_match("item9", "\"etag-1\"")
            .unwrap();
        assert!(!deleted);
        let seen = server.join().unwrap();
        assert!(seen[0].contains("DELETE /me/drive/items/item9 "));
        assert!(seen[0].contains("if-match: \"etag-1\""));
    }

    #[test]
    fn delete_item_if_match_returns_true_on_204() {
        let (base, _server) = serve(vec![http_response(204, "No Content", "", "")]);
        assert!(GraphClient::new("tok")
            .with_base_url(&base)
            .delete_item_if_match("item9", "\"etag-1\"")
            .unwrap());
    }

    #[test]
    fn put_content_and_delete_item_roundtrip_and_classify() {
        let (base, _s) = serve(vec![http_response(200, "OK", "", "{\"id\":\"w1\"}")]);
        let out = GraphClient::new("tok")
            .with_base_url(&base)
            .put_content("w1", b"new bytes")
            .unwrap();
        assert_eq!(out["id"].as_str(), Some("w1"));

        let (base2, server2) = serve(vec![http_response(204, "No Content", "", "")]);
        GraphClient::new("tok")
            .with_base_url(&base2)
            .delete_item("w1")
            .unwrap();
        assert!(server2.join().unwrap()[0].starts_with("DELETE /me/drive/items/w1"));

        let (base3, _s3) = serve(vec![http_response(404, "Not Found", "", "gone")]);
        match GraphClient::new("tok")
            .with_base_url(&base3)
            .delete_item("w1")
            .unwrap_err()
        {
            UploadError::Http { status, .. } => assert_eq!(status, 404),
            other => panic!("expected Http error, got {other}"),
        }
    }

    #[test]
    fn create_folder_posts_to_children_and_returns_new_id() {
        // top-level (root) folder → POST /me/drive/root/children
        let (base, server) = serve(vec![http_response(201, "Created", "", "{\"id\":\"D1\"}")]);
        let out = GraphClient::new("tok")
            .with_base_url(&base)
            .create_folder("", "New Folder")
            .unwrap();
        assert_eq!(out["id"].as_str(), Some("D1"));
        let seen = server.join().unwrap();
        assert!(seen[0].starts_with("POST /me/drive/root/children"));
        assert!(seen[0].contains("content-type: application/json"));

        // nested folder → POST under the parent item id
        let (base2, server2) = serve(vec![http_response(201, "Created", "", "{\"id\":\"D2\"}")]);
        GraphClient::new("tok")
            .with_base_url(&base2)
            .create_folder("PARENT", "Sub")
            .unwrap();
        assert!(server2.join().unwrap()[0].starts_with("POST /me/drive/items/PARENT/children"));
    }

    #[test]
    fn list_children_targets_children_with_select() {
        // root → GET /me/drive/root/children?$select=...
        let (base, server) = serve(vec![http_response(200, "OK", "", r#"{"value":[]}"#)]);
        GraphClient::new("tok")
            .with_base_url(&base)
            .list_children("")
            .unwrap();
        let seen = server.join().unwrap();
        assert!(
            seen[0].starts_with(
                "GET /me/drive/root/children?$select=id,name,size,folder,file,lastModifiedDateTime,eTag"
            ),
            "root listing must GET /children with the $select, got: {}",
            seen[0]
        );

        // by parent id → GET /me/drive/items/PARENT/children?$select=...
        let (base2, server2) = serve(vec![http_response(200, "OK", "", r#"{"value":[]}"#)]);
        GraphClient::new("tok")
            .with_base_url(&base2)
            .list_children("PARENT")
            .unwrap();
        assert!(server2.join().unwrap()[0].starts_with(
            "GET /me/drive/items/PARENT/children?$select=id,name,size,folder,file,lastModifiedDateTime,eTag"
        ));
    }

    #[test]
    fn upload_to_parent_targets_root_or_item_content() {
        // #657 regression: an empty parent (drive root) must PUT /me/drive/root:/…:/content,
        // NOT the malformed /me/drive/items/:/… that Graph rejects.
        let (base, server) = serve(vec![http_response(201, "Created", "", r#"{"id":"new"}"#)]);
        GraphClient::new("tok")
            .with_base_url(&base)
            .upload_to_parent("", "a b.txt", b"data")
            .unwrap();
        assert!(
            server.join().unwrap()[0].starts_with(
                "PUT /me/drive/root:/a%20b.txt:/content?@microsoft.graph.conflictBehavior=fail"
            ),
            "root upload must PUT /root:/…:/content"
        );

        // by parent id → PUT /me/drive/items/PARENT:/{name}:/content (unchanged path form).
        let (base2, server2) = serve(vec![http_response(201, "Created", "", r#"{"id":"new"}"#)]);
        GraphClient::new("tok")
            .with_base_url(&base2)
            .upload_to_parent("PARENT", "f.bin", b"x")
            .unwrap();
        assert!(
            server2.join().unwrap()[0].starts_with(
                "PUT /me/drive/items/PARENT:/f.bin:/content?@microsoft.graph.conflictBehavior=fail"
            ),
            "by-id upload must PUT /items/ID:/…:/content"
        );
    }

    #[test]
    fn list_children_pages_over_200_items() {
        // AC1 (#647): a folder with >200 children returns the full set across all
        // `@odata.nextLink` pages. 5 pages × 60 = 300 items; pages 1..=4 link on.
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let mut pages: Vec<String> = Vec::new();
        for p in 0..5 {
            let vals: Vec<String> = (0..60)
                .map(|i| format!(r#"{{"id":"it-{p}-{i}"}}"#))
                .collect();
            let body = if p < 4 {
                format!(
                    r#"{{"value":[{}],"@odata.nextLink":"{base}/p{}"}}"#,
                    vals.join(","),
                    p + 1
                )
            } else {
                format!(r#"{{"value":[{}]}}"#, vals.join(","))
            };
            pages.push(http_response(200, "OK", "", &body));
        }
        let handle = std::thread::spawn(move || {
            let mut served = 0;
            for resp in pages {
                let (mut sock, _) = listener.accept().unwrap();
                read_request(&mut sock);
                sock.write_all(resp.as_bytes()).unwrap();
                served += 1;
            }
            served
        });
        let items = GraphClient::new("tok")
            .with_base_url(&base)
            .list_children("BIG")
            .unwrap();
        assert_eq!(items.len(), 300, "all pages' children must be concatenated");
        assert_eq!(
            handle.join().unwrap(),
            5,
            "every page was fetched (paged to completion)"
        );
    }

    #[test]
    fn list_children_retries_via_central_policy_then_pages() {
        // AC2 (#647): every request — including a throttled retry — goes through the
        // central retry policy (`get_with_retry`): the 429 is honored (Retry-After: 0)
        // and retried, then paging completes. Proves list_children never bypasses it.
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let throttled = http_response(429, "Too Many Requests", "Retry-After: 0\r\n", "{}");
        let page1 = http_response(
            200,
            "OK",
            "",
            &format!(r#"{{"value":[{{"id":"a"}}],"@odata.nextLink":"{base}/p2"}}"#),
        );
        let page2 = http_response(200, "OK", "", r#"{"value":[{"id":"b"}]}"#);
        let handle = std::thread::spawn(move || {
            let mut served = 0;
            for resp in [throttled, page1, page2] {
                let (mut sock, _) = listener.accept().unwrap();
                read_request(&mut sock);
                sock.write_all(resp.as_bytes()).unwrap();
                served += 1;
            }
            served
        });
        let items = GraphClient::new("tok")
            .with_base_url(&base)
            .list_children("F")
            .unwrap();
        assert_eq!(items.len(), 2, "children from both pages after the retry");
        assert_eq!(
            handle.join().unwrap(),
            3,
            "429 retried once, then both pages fetched"
        );
    }

    #[test]
    fn move_item_patches_the_item_and_classifies_conflict() {
        // rename in place (no parent change) → PATCH the item id
        let (base, server) = serve(vec![http_response(200, "OK", "", "{\"id\":\"i1\"}")]);
        let out = GraphClient::new("tok")
            .with_base_url(&base)
            .move_item("i1", None, "renamed.txt")
            .unwrap();
        assert_eq!(out["id"].as_str(), Some("i1"));
        let seen = server.join().unwrap();
        assert!(seen[0].starts_with("PATCH /me/drive/items/i1"));
        assert!(seen[0].contains("content-type: application/json"));

        // a name conflict on move surfaces as a classified HTTP error
        let (base2, _s2) = serve(vec![http_response(409, "Conflict", "", "name exists")]);
        match GraphClient::new("tok")
            .with_base_url(&base2)
            .move_item("i1", Some("P2"), "x")
            .unwrap_err()
        {
            UploadError::Http { status, .. } => assert_eq!(status, 409),
            other => panic!("expected Http error, got {other}"),
        }
    }

    #[test]
    fn enc_path_encodes_each_segment_keeps_slash_strips_leading() {
        // space, ':' (the root:/…: delimiter), '#', '%', and an umlaut are all escaped
        assert_eq!(
            // lang-allow: deliberate non-ASCII (umlaut) path-encoding fixture
            enc_path("Docs/Q3 Report/Übersicht: v#2%.pdf"),
            "Docs/Q3%20Report/%C3%9Cbersicht%3A%20v%232%25.pdf"
        );
        // a leading slash is stripped; '/' stays the separator
        assert_eq!(enc_path("/a/b.txt"), "a/b.txt");
        // encode_id (single segment) is unchanged after the encode_seg refactor
        assert_eq!(encode_id("01ABCDEF-_."), "01ABCDEF-_.");
        assert_eq!(encode_id("aB+/9=="), "aB%2B%2F9%3D%3D");
    }

    #[test]
    fn item_id_for_path_resolves_via_root_path_addressing() {
        let (base, server) = serve(vec![http_response(200, "OK", "", "{\"id\":\"X1\"}")]);
        let id = GraphClient::new("tok")
            .with_base_url(&base)
            .item_id_for_path("Docs/Q3 Report.txt")
            .unwrap();
        assert_eq!(id, "X1");
        let seen = server.join().unwrap();
        // path is colon-addressed and each segment encoded (space -> %20)
        assert!(
            seen[0].starts_with("GET /me/drive/root:/Docs/Q3%20Report.txt"),
            "got: {}",
            seen[0].lines().next().unwrap_or("")
        );

        // a missing item (404) surfaces as a classified HTTP error
        let (base2, _s2) = serve(vec![http_response(404, "Not Found", "", "gone")]);
        match GraphClient::new("tok")
            .with_base_url(&base2)
            .item_id_for_path("nope.txt")
            .unwrap_err()
        {
            UploadError::Http { status, .. } => assert_eq!(status, 404),
            other => panic!("expected Http error, got {other}"),
        }
    }

    #[test]
    fn create_link_posts_createlink_and_returns_weburl() {
        let (base, server) = serve(vec![http_response(
            200,
            "OK",
            "",
            "{\"link\":{\"webUrl\":\"https://1drv.ms/x/abc\"}}",
        )]);
        let url = GraphClient::new("tok")
            .with_base_url(&base)
            .create_link("i1", "view", "anonymous", None, None, None)
            .unwrap();
        assert_eq!(url, "https://1drv.ms/x/abc");
        let seen = server.join().unwrap();
        assert!(seen[0].starts_with("POST /me/drive/items/i1/createLink"));
        assert!(seen[0].contains("content-type: application/json"));

        // a 200 without link.webUrl is a parse error, not a silent empty string
        let (base2, _s2) = serve(vec![http_response(200, "OK", "", "{\"link\":{}}")]);
        match GraphClient::new("tok")
            .with_base_url(&base2)
            .create_link("i1", "view", "anonymous", None, None, None)
            .unwrap_err()
        {
            UploadError::Parse(_) => {}
            other => panic!("expected Parse error, got {other}"),
        }

        // 403 (e.g. premium-gated option) is classified
        let (base3, _s3) = serve(vec![http_response(403, "Forbidden", "", "no")]);
        match GraphClient::new("tok")
            .with_base_url(&base3)
            .create_link("i1", "view", "anonymous", Some("pw"), None, None)
            .unwrap_err()
        {
            UploadError::Http { status, .. } => assert_eq!(status, 403),
            other => panic!("expected Http error, got {other}"),
        }
    }

    #[test]
    fn create_link_detailed_accepts_organization_scope_and_returns_permission() {
        let (base, server) = serve(vec![http_response(
            201,
            "Created",
            "",
            r#"{
                "id": "perm-org",
                "roles": ["write"],
                "link": {
                    "type": "edit",
                    "scope": "organization",
                    "webUrl": "https://contoso.sharepoint.com/:w:/r/doc"
                }
            }"#,
        )]);
        let permission = GraphClient::new("tok")
            .with_base_url(&base)
            .create_link_detailed("i1", "edit", "organization", None, None, None)
            .unwrap();
        assert_eq!(permission.id, "perm-org");
        assert_eq!(permission.roles, vec!["write".to_string()]);
        assert_eq!(permission.link_type.as_deref(), Some("edit"));
        assert_eq!(permission.link_scope.as_deref(), Some("organization"));
        assert_eq!(
            permission.link_web_url.as_deref(),
            Some("https://contoso.sharepoint.com/:w:/r/doc")
        );
        let seen = server.join().unwrap();
        assert!(seen[0].starts_with("POST /me/drive/items/i1/createLink"));
        assert!(seen[0].contains(r#""scope":"organization""#));
    }

    #[test]
    fn detailed_permission_parser_extracts_link_identities_and_invitation() {
        let value = serde_json::json!({
            "id": "perm1",
            "roles": ["read"],
            "inheritedFrom": {"id": "parent"},
            "link": {
                "webUrl": "https://1drv.ms/x/abc",
                "type": "view",
                "scope": "anonymous"
            },
            "grantedToV2": {
                "user": {"email": "Ada@Example.com"},
                "siteUser": {"loginName": "ada@example.com"}
            },
            "grantedToIdentitiesV2": [
                {"user": {"userPrincipalName": "bob@example.com"}},
                {"siteUser": {"loginName": "not-an-email"}}
            ],
            "grantedTo": {
                "user": {"mail": "carol@example.com"}
            },
            "invitation": {"email": "invitee@example.com"}
        });
        let permission = DrivePermission::from_graph_value(&value).unwrap();
        assert_eq!(permission.id, "perm1");
        assert_eq!(permission.roles, vec!["read".to_string()]);
        assert_eq!(
            permission.link_web_url.as_deref(),
            Some("https://1drv.ms/x/abc")
        );
        assert_eq!(permission.link_type.as_deref(), Some("view"));
        assert_eq!(permission.link_scope.as_deref(), Some("anonymous"));
        assert!(permission.inherited);
        for email in [
            "Ada@Example.com",
            "ada@example.com",
            "bob@example.com",
            "carol@example.com",
        ] {
            assert!(permission.granted_emails.contains(&email.to_string()));
        }
        assert_eq!(permission.granted_emails.len(), 4);
        assert_eq!(
            permission.invitation_emails,
            vec!["invitee@example.com".to_string()]
        );
    }

    #[test]
    fn list_permissions_detailed_preserves_tuple_compatibility() {
        let (base, server) = serve(vec![http_response(
            200,
            "OK",
            "",
            r#"{
                "value": [
                    {
                        "id": "p1",
                        "roles": ["read"],
                        "link": {
                            "webUrl": "https://1drv.ms/x/abc",
                            "type": "view",
                            "scope": "users"
                        },
                        "grantedToV2": {
                            "user": {"email": "reader@example.com"}
                        }
                    }
                ]
            }"#,
        )]);
        let client = GraphClient::new("tok").with_base_url(&base);
        let detailed = client.list_permissions_detailed("i1").unwrap();
        assert_eq!(detailed.len(), 1);
        assert_eq!(detailed[0].id, "p1");
        assert_eq!(detailed[0].link_type.as_deref(), Some("view"));
        assert_eq!(detailed[0].link_scope.as_deref(), Some("users"));
        assert_eq!(
            detailed[0].granted_emails,
            vec!["reader@example.com".to_string()]
        );
        assert!(server.join().unwrap()[0].starts_with("GET /me/drive/items/i1/permissions"));

        let (base2, _server2) = serve(vec![http_response(
            200,
            "OK",
            "",
            r#"{
                "value": [
                    {
                        "id": "p1",
                        "roles": ["read"],
                        "link": {"webUrl": "u"},
                        "invitation": {"email": "fallback@example.com"}
                    }
                ]
            }"#,
        )]);
        let tuples = GraphClient::new("tok")
            .with_base_url(&base2)
            .list_permissions("i1")
            .unwrap();
        assert_eq!(tuples.len(), 1);
        assert_eq!(tuples[0].0, "p1");
        assert_eq!(tuples[0].1, vec!["read".to_string()]);
        assert_eq!(tuples[0].2.as_deref(), Some("u"));
        assert_eq!(tuples[0].3.as_deref(), Some("fallback@example.com"));
    }

    #[test]
    fn invite_posts_invite_and_returns_permission_ids() {
        let (base, server) = serve(vec![http_response(
            200,
            "OK",
            "",
            "{\"value\":[{\"id\":\"perm1\"},{\"id\":\"perm2\"}]}",
        )]);
        let ids = GraphClient::new("tok")
            .with_base_url(&base)
            .invite(
                "i1",
                &["a@b.com".to_string()],
                &["read"],
                true,
                true,
                "hi",
                None,
                None,
            )
            .unwrap();
        assert_eq!(ids, vec!["perm1".to_string(), "perm2".to_string()]);
        let seen = server.join().unwrap();
        assert!(seen[0].starts_with("POST /me/drive/items/i1/invite"));
        assert!(seen[0].contains("content-type: application/json"));
    }

    #[test]
    fn invite_detailed_parses_207_partial_as_distinct_outcome() {
        let (base, server) = serve(vec![http_response(
            207,
            "Multi-Status",
            "",
            r#"{
                "value": [
                    {"id": "perm-ok", "roles": ["read"]},
                    {"error": {"code": "recipientNotFound", "message": "raw person@example.com"}}
                ]
            }"#,
        )]);
        let outcome = GraphClient::new("tok")
            .with_base_url(&base)
            .invite_detailed(
                "i1",
                &["person@example.com".to_string()],
                &["read"],
                true,
                true,
                "",
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            outcome,
            InviteOutcome::Partial {
                successful_permission_ids: vec!["perm-ok".to_string()],
                failed_recipient_count: 1,
                redacted_reason: "partial_success".to_string(),
            }
        );
        let seen = server.join().unwrap();
        assert!(seen[0].starts_with("POST /me/drive/items/i1/invite"));
        assert!(seen[0].contains(r#""sendInvitation":true"#));

        let (base2, _server2) = serve(vec![http_response(
            207,
            "Multi-Status",
            "",
            r#"{"value":[{"id":"perm-ok"},{"error":{"message":"raw person@example.com"}}]}"#,
        )]);
        match GraphClient::new("tok")
            .with_base_url(&base2)
            .invite(
                "i1",
                &["person@example.com".to_string()],
                &["read"],
                true,
                true,
                "",
                None,
                None,
            )
            .unwrap_err()
        {
            UploadError::Http { status, body } => {
                assert_eq!(status, 207);
                assert_eq!(body, "partial_success");
                assert!(!body.contains("person@example.com"));
            }
            other => panic!("expected redacted 207 Http error, got {other}"),
        }
    }

    #[test]
    fn invite_detailed_handles_empty_value_without_panic() {
        let (base, server) = serve(vec![http_response(200, "OK", "", r#"{"value":[]}"#)]);
        let outcome = GraphClient::new("tok")
            .with_base_url(&base)
            .invite_detailed(
                "i1",
                &["person@example.com".to_string()],
                &["read"],
                true,
                true,
                "",
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            outcome,
            InviteOutcome::Applied {
                permission_ids: Vec::new()
            }
        );
        assert!(server.join().unwrap()[0].starts_with("POST /me/drive/items/i1/invite"));
    }

    #[test]
    fn list_and_delete_permissions_roundtrip() {
        let (base, server) = serve(vec![http_response(
            200,
            "OK",
            "",
            "{\"value\":[{\"id\":\"p1\",\"roles\":[\"read\"],\"link\":{\"webUrl\":\"u\"}}]}",
        )]);
        let perms = GraphClient::new("tok")
            .with_base_url(&base)
            .list_permissions("i1")
            .unwrap();
        assert_eq!(perms.len(), 1);
        assert_eq!(perms[0].0, "p1");
        assert_eq!(perms[0].1, vec!["read".to_string()]);
        assert_eq!(perms[0].2.as_deref(), Some("u"));
        assert!(server.join().unwrap()[0].starts_with("GET /me/drive/items/i1/permissions"));

        let (base2, server2) = serve(vec![http_response(204, "No Content", "", "")]);
        GraphClient::new("tok")
            .with_base_url(&base2)
            .delete_permission("i1", "p1")
            .unwrap();
        assert!(server2.join().unwrap()[0].starts_with("DELETE /me/drive/items/i1/permissions/p1"));
    }

    #[test]
    fn download_content_and_message_mime_fetch_bytes_from_the_base() {
        let (base, server) = serve(vec![
            http_response(200, "OK", "", "file-bytes"),
            http_response(200, "OK", "", "mime-bytes"),
        ]);
        let c = GraphClient::new("tok").with_base_url(&base);
        assert_eq!(c.download_content("i1").unwrap(), b"file-bytes");
        assert_eq!(c.download_message_mime("m1").unwrap(), b"mime-bytes");
        let seen = server.join().unwrap();
        assert!(seen[0].starts_with("GET /me/drive/items/i1/content"));
        assert!(seen[1].starts_with("GET /me/messages/m1/$value"));
    }

    #[test]
    fn post_json_prefixes_paths_with_the_base_and_returns_created() {
        let (base, server) = serve(vec![http_response(201, "Created", "", "{\"id\":\"ev1\"}")]);
        let out = GraphClient::new("tok")
            .with_base_url(&base)
            .post_json("/me/events", &serde_json::json!({"subject": "s"}))
            .unwrap();
        assert_eq!(out["id"].as_str(), Some("ev1"));
        let seen = server.join().unwrap();
        assert!(seen[0].starts_with("POST /me/events"));
        assert!(seen[0].contains("content-type: application/json"));
    }

    #[test]
    fn contact_verbs_hit_the_right_urls_and_encode_ids() {
        let (base, server) = serve(vec![
            http_response(201, "Created", "", "{\"id\":\"c1\"}"),
            http_response(200, "OK", "", "{\"id\":\"c1\"}"),
            http_response(204, "No Content", "", ""),
        ]);
        let c = GraphClient::new("tok").with_base_url(&base);
        let created = c
            .create_contact(&serde_json::json!({ "displayName": "Ada" }))
            .unwrap();
        assert_eq!(created["id"].as_str(), Some("c1"));
        c.update_contact("aB+/9==", &serde_json::json!({ "jobTitle": "Analyst" }))
            .unwrap();
        c.delete_contact("aB+/9==").unwrap();
        let seen = server.join().unwrap();
        assert!(seen[0].starts_with("POST /me/contacts"));
        assert!(seen[0].contains("content-type: application/json"));
        // Outlook ids carry +//= → must be percent-escaped or Graph 404s the path.
        assert!(seen[1].starts_with("PATCH /me/contacts/aB%2B%2F9%3D%3D"));
        assert!(seen[2].starts_with("DELETE /me/contacts/aB%2B%2F9%3D%3D"));
    }

    #[test]
    fn todo_write_verbs_hit_the_right_urls() {
        let (base, server) = serve(vec![
            http_response(201, "Created", "", "{\"id\":\"t1\"}"),
            http_response(200, "OK", "", "{\"id\":\"t1\"}"),
            http_response(201, "Created", "", "{\"id\":\"ci1\"}"),
            http_response(200, "OK", "", "{\"id\":\"ci1\"}"),
            http_response(204, "No Content", "", ""),
            http_response(204, "No Content", "", ""),
            http_response(201, "Created", "", "{\"id\":\"L9\"}"),
            http_response(204, "No Content", "", ""),
        ]);
        let c = GraphClient::new("tok").with_base_url(&base);
        c.create_task("L1", &serde_json::json!({"title":"x"}))
            .unwrap();
        c.update_task("L1", "t1", &serde_json::json!({"status":"completed"}))
            .unwrap();
        c.create_checklist_item("L1", "t1", &serde_json::json!({"displayName":"step"}))
            .unwrap();
        c.update_checklist_item("L1", "t1", "ci1", &serde_json::json!({"isChecked":true}))
            .unwrap();
        c.delete_checklist_item("L1", "t1", "ci1").unwrap();
        c.delete_task("L1", "t1").unwrap();
        c.create_todo_list(&serde_json::json!({"displayName":"New"}))
            .unwrap();
        c.delete_todo_list("L9").unwrap();
        let seen = server.join().unwrap();
        assert!(seen[0].starts_with("POST /me/todo/lists/L1/tasks"));
        assert!(seen[1].starts_with("PATCH /me/todo/lists/L1/tasks/t1"));
        assert!(seen[2].starts_with("POST /me/todo/lists/L1/tasks/t1/checklistItems"));
        assert!(seen[3].starts_with("PATCH /me/todo/lists/L1/tasks/t1/checklistItems/ci1"));
        assert!(seen[4].starts_with("DELETE /me/todo/lists/L1/tasks/t1/checklistItems/ci1"));
        assert!(seen[5].starts_with("DELETE /me/todo/lists/L1/tasks/t1"));
        assert!(seen[6].starts_with("POST /me/todo/lists"));
        assert!(seen[7].starts_with("DELETE /me/todo/lists/L9"));
    }

    #[test]
    fn create_message_from_mime_posts_base64_with_text_plain() {
        let (base, server) = serve(vec![http_response(201, "Created", "", "{\"id\":\"msg1\"}")]);
        let out = GraphClient::new("tok")
            .with_base_url(&base)
            .create_message_from_mime(b"foobar")
            .unwrap();
        assert_eq!(out["id"].as_str(), Some("msg1"));
        let seen = server.join().unwrap();
        assert!(seen[0].starts_with("POST /me/messages"));
        assert!(seen[0].contains("content-type: text/plain"));
        // body itself is after the head; assert the encoding via content-length of "Zm9vYmFy"
        assert!(seen[0].to_ascii_lowercase().contains("content-length: 8"));
    }

    #[test]
    fn onenote_page_create_multipart_and_delete_roundtrip() {
        let (base, server) = serve(vec![
            http_response(201, "Created", "", "{\"id\":\"page1\"}"),
            http_response(204, "No Content", "", ""),
        ]);
        let c = GraphClient::new("tok").with_base_url(&base);
        let html = br#"<html><body><img src="name:img1"/></body></html>"#;
        let parts = vec![OneNotePagePart {
            name: "img1".into(),
            content_type: "image/png".into(),
            bytes: vec![1, 2, 3],
        }];
        let out = c.create_onenote_page_multipart(html, &parts).unwrap();
        assert_eq!(out["id"].as_str(), Some("page1"));
        c.delete_onenote_page("page1").unwrap();
        let seen = server.join().unwrap();
        assert!(seen[0].contains("content-type: multipart/form-data; boundary=isyncyou-"));
        assert!(seen[1].starts_with("DELETE /me/onenote/pages/page1"));
    }

    #[test]
    fn onenote_section_create_and_content_append_hit_the_right_urls() {
        let (base, server) = serve(vec![
            http_response(201, "Created", "", "{\"id\":\"p1\"}"),
            http_response(204, "No Content", "", ""),
        ]);
        let c = GraphClient::new("tok").with_base_url(&base);
        let out = c
            .create_onenote_page_in_section("S1", b"<html><body>hi</body></html>")
            .unwrap();
        assert_eq!(out["id"].as_str(), Some("p1"));
        c.append_onenote_page_content(
            "p1",
            &serde_json::json!([{ "target": "body", "action": "append", "content": "<p>x</p>" }]),
        )
        .unwrap();
        let seen = server.join().unwrap();
        assert!(seen[0].starts_with("POST /me/onenote/sections/S1/pages"));
        assert!(seen[0].contains("content-type: text/html"));
        assert!(seen[1].starts_with("PATCH /me/onenote/pages/p1/content"));
        assert!(seen[1].contains("content-type: application/json"));
    }

    #[test]
    fn delete_url_accepts_2xx_and_classifies_failures() {
        let (base, _s) = serve(vec![http_response(202, "Accepted", "", "")]);
        GraphClient::new("tok")
            .with_base_url(&base)
            .delete_url("/me/contacts/c1")
            .unwrap();

        let (base2, _s2) = serve(vec![http_response(403, "Forbidden", "", "no")]);
        match GraphClient::new("tok")
            .with_base_url(&base2)
            .delete_url("/me/contacts/c1")
            .unwrap_err()
        {
            UploadError::Http { status, .. } => assert_eq!(status, 403),
            other => panic!("expected Http error, got {other}"),
        }
    }

    #[test]
    fn encode_id_escapes_base64_chars_in_outlook_ids() {
        // plain ids pass through (OneDrive drive-item ids)
        assert_eq!(encode_id("01ABCDEF-_."), "01ABCDEF-_.");
        // base64-ish Outlook ids: + / = must be escaped or Graph 404s the path
        assert_eq!(encode_id("aB+/9=="), "aB%2B%2F9%3D%3D");
    }

    #[test]
    fn delete_message_encodes_the_id_in_the_path() {
        let (base, server) = serve(vec![http_response(204, "No Content", "", "")]);
        GraphClient::new("tok")
            .with_base_url(&base)
            .delete_message("AB+/cd=")
            .unwrap();
        let req = &server.join().unwrap()[0];
        assert!(
            req.starts_with("DELETE /me/messages/AB%2B%2Fcd%3D"),
            "id not percent-encoded in path: {}",
            req.lines().next().unwrap_or("")
        );
    }

    #[test]
    fn onenote_multipart_body_rejects_header_injection() {
        let bad_name = vec![OneNotePagePart {
            name: "bad\r\nname".into(),
            content_type: "image/png".into(),
            bytes: vec![1],
        }];
        assert!(onenote_multipart_body(b"<html></html>", &bad_name).is_err());

        let bad_type = vec![OneNotePagePart {
            name: "imageBlock1".into(),
            content_type: "image/png\r\nX-Evil: 1".into(),
            bytes: vec![1],
        }];
        assert!(onenote_multipart_body(b"<html></html>", &bad_type).is_err());
    }

    /// Live OneDrive delta against the test account. Skips unless
    /// `ISYNCYOU_TEST_TOKEN` (a Files.Read bearer token for the throwaway
    /// account) is set, so CI without credentials passes.
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_onedrive_delta() {
        let token = match std::env::var("ISYNCYOU_TEST_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!("skipping live_onedrive_delta: ISYNCYOU_TEST_TOKEN not set");
                return;
            }
        };
        let mut client = GraphClient::new(token);
        let out = run_delta(
            &mut client,
            "https://graph.microsoft.com/v1.0/me/drive/root/delta",
            None,
            5,
        )
        .expect("live delta walk should succeed");
        assert!(!out.cursor.as_str().is_empty(), "expected a delta cursor");
        eprintln!(
            "live delta: {} items, cursor {} chars",
            out.items.len(),
            out.cursor.as_str().len()
        );
    }

    /// Live resumable upload (then cleanup) against the test account. Skips
    /// unless `ISYNCYOU_TEST_WRITE_TOKEN` (a Files.ReadWrite bearer token) is set.
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_onedrive_upload_roundtrip() {
        let token = match std::env::var("ISYNCYOU_TEST_WRITE_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!(
                    "skipping live_onedrive_upload_roundtrip: ISYNCYOU_TEST_WRITE_TOKEN not set"
                );
                return;
            }
        };
        let client = GraphClient::new(token);
        // ~1.1 MiB deterministic payload -> forces a multi-chunk session.
        let data: Vec<u8> = (0..1_100_000u32).map(|i| (i % 251) as u8).collect();
        let path = "/iSyncYou-livetest/upload-roundtrip.bin";

        let item = client
            .upload_file(path, &data, CHUNK_ALIGN * 2)
            .expect("resumable upload should succeed");
        assert_eq!(
            item["size"].as_u64(),
            Some(data.len() as u64),
            "uploaded size mismatch"
        );
        let id = item["id"]
            .as_str()
            .expect("created item should have an id")
            .to_string();
        eprintln!("uploaded {} bytes -> item {id}", data.len());

        // download it back and verify the content round-trips byte-for-byte
        let downloaded = client
            .download_content(&id)
            .expect("download should succeed");
        assert_eq!(downloaded.len(), data.len(), "downloaded length mismatch");
        assert_eq!(downloaded, data, "downloaded content must match the upload");
        eprintln!("downloaded {} bytes, content matches", downloaded.len());

        client
            .delete_item(&id)
            .expect("cleanup delete should succeed");
        eprintln!("cleaned up test item {id}");
    }

    /// Live outbound-sharing round-trip against the throwaway account. Skips
    /// unless `ISYNCYOU_TEST_WRITE_TOKEN` (a Files.ReadWrite bearer token) is set.
    /// This is the only test that exercises the request *bodies* Graph accepts and
    /// the personal-account link constraints (the mock server only returns heads).
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    #[test]
    fn live_sharing_roundtrip() {
        let token = match std::env::var("ISYNCYOU_TEST_WRITE_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!("skipping live_sharing_roundtrip: ISYNCYOU_TEST_WRITE_TOKEN not set");
                return;
            }
        };
        let client = GraphClient::new(token);
        let rel = "iSyncYou-livetest/share-roundtrip.txt";
        let item = client
            .upload_file(&format!("/{rel}"), b"share me", CHUNK_ALIGN)
            .expect("upload should succeed");
        let id = item["id"].as_str().expect("item id").to_string();

        // path -> id resolves to the same item
        let resolved = client.item_id_for_path(rel).expect("resolve path");
        assert_eq!(resolved, id, "item_id_for_path must match the uploaded id");

        // anonymous view link → reachable webUrl
        let url = client
            .create_link(&id, "view", "anonymous", None, None, None)
            .expect("createLink");
        assert!(url.starts_with("https://"), "webUrl: {url}");
        eprintln!("created view link: {url}");

        // list shows the just-created link permission
        let perms = client.list_permissions(&id).expect("list permissions");
        assert!(!perms.is_empty(), "expected at least one permission");
        eprintln!("permissions: {perms:?}");

        // revoke every permission we can, then cleanup the item
        for (pid, _, _, _) in &perms {
            let _ = client.delete_permission(&id, pid);
        }
        client.delete_item(&id).expect("cleanup delete");
        eprintln!("revoked + cleaned up {id}");
    }

    // ---- mail write layer (#561) --------------------------------------------

    #[test]
    fn mail_write_builders_have_exact_shapes() {
        assert_eq!(
            mail_recipient("a@b.com"),
            serde_json::json!({ "emailAddress": { "address": "a@b.com" } })
        );
        let msg = serde_json::json!({ "subject": "Hi" });
        assert_eq!(
            send_envelope(&msg, true),
            serde_json::json!({ "message": { "subject": "Hi" }, "saveToSentItems": true })
        );
        assert_eq!(comment_body("ok"), serde_json::json!({ "comment": "ok" }));
        assert_eq!(
            forward_body("fyi", &["x@y.com", "z@y.com"]),
            serde_json::json!({
                "comment": "fyi",
                "toRecipients": [
                    { "emailAddress": { "address": "x@y.com" } },
                    { "emailAddress": { "address": "z@y.com" } }
                ]
            })
        );
        assert_eq!(
            move_body("AAMk"),
            serde_json::json!({ "destinationId": "AAMk" })
        );
        assert_eq!(read_body(true), serde_json::json!({ "isRead": true }));
        assert_eq!(
            flag_body("flagged", None, "UTC"),
            serde_json::json!({ "flag": { "flagStatus": "flagged" } })
        );
        assert_eq!(
            categories_body(&["Red".to_string(), "Work".to_string()]),
            serde_json::json!({ "categories": ["Red", "Work"] })
        );
        assert_eq!(
            importance_body("high"),
            serde_json::json!({ "importance": "high" })
        );
    }

    #[test]
    fn send_mail_posts_to_send_mail_action() {
        let (base, h) = serve(vec![http_response(202, "Accepted", "", "")]);
        let c = GraphClient::new("tok").with_base_url(&base);
        c.send_mail(&serde_json::json!({ "subject": "Hi" }), true)
            .expect("send");
        let req = &h.join().unwrap()[0];
        assert!(
            req.starts_with("POST /me/sendMail HTTP/1.1"),
            "unexpected request line: {req}"
        );
    }

    #[test]
    fn reply_and_forward_hit_the_right_action_paths() {
        let (base, h) = serve(vec![
            http_response(202, "Accepted", "", ""),
            http_response(202, "Accepted", "", ""),
        ]);
        let c = GraphClient::new("tok").with_base_url(&base);
        c.reply("m1", "thanks").expect("reply");
        c.forward("m1", "fyi", &["x@y.com"]).expect("forward");
        let seen = h.join().unwrap();
        assert!(seen[0].starts_with("POST /me/messages/m1/reply HTTP/1.1"));
        assert!(seen[1].starts_with("POST /me/messages/m1/forward HTTP/1.1"));
    }

    #[test]
    fn move_message_posts_to_move_and_returns_resource() {
        let (base, h) = serve(vec![http_response(201, "Created", "", r#"{"id":"newid"}"#)]);
        let c = GraphClient::new("tok").with_base_url(&base);
        let moved = c.move_message("m1", "AAMkDest").expect("move");
        assert_eq!(moved["id"], "newid");
        assert!(h.join().unwrap()[0].starts_with("POST /me/messages/m1/move HTTP/1.1"));
    }

    #[test]
    fn set_read_patches_the_message() {
        let (base, h) = serve(vec![http_response(
            200,
            "OK",
            "",
            r#"{"id":"m1","isRead":true}"#,
        )]);
        let c = GraphClient::new("tok").with_base_url(&base);
        let updated = c.set_read("m1", true).expect("set_read");
        assert_eq!(updated["isRead"], true);
        assert!(h.join().unwrap()[0].starts_with("PATCH /me/messages/m1 HTTP/1.1"));
    }

    #[test]
    fn send_draft_posts_to_send_with_no_body() {
        let (base, h) = serve(vec![http_response(202, "Accepted", "", "")]);
        let c = GraphClient::new("tok").with_base_url(&base);
        c.send_draft("m1").expect("send draft");
        let req = &h.join().unwrap()[0];
        assert!(req.starts_with("POST /me/messages/m1/send HTTP/1.1"));
        // no body: Content-Length must be absent or zero
        assert!(
            !req.to_ascii_lowercase().contains("content-length: ")
                || req.to_ascii_lowercase().contains("content-length: 0"),
            "send draft must carry no body: {req}"
        );
    }

    /// Live send-to-self against the throwaway account. Needs `ISYNCYOU_TEST_TOKEN`
    /// (carrying `Mail.Send`) + `ISYNCYOU_TEST_EMAIL` (the self address).
    #[test]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    fn live_send_mail_to_self() {
        let token = match std::env::var("ISYNCYOU_TEST_TOKEN") {
            Ok(t) => t,
            Err(_) => {
                eprintln!("skipping live_send_mail_to_self: ISYNCYOU_TEST_TOKEN not set");
                return;
            }
        };
        let to = std::env::var("ISYNCYOU_TEST_EMAIL")
            .expect("ISYNCYOU_TEST_EMAIL (self address) required for the live send test");
        let c = GraphClient::new(token);
        let message = serde_json::json!({
            "subject": "iSyncYou live send-to-self test",
            "body": { "contentType": "Text", "content": "Sent by the #561 live test." },
            "toRecipients": [ mail_recipient(&to) ],
        });
        c.send_mail(&message, true).expect("live send-to-self");
        eprintln!("live send-to-self delivered to {to}");
    }

    /// Live exercise of the metadata-PATCH + move verbs against the throwaway
    /// account: pick a real message, flag/read/categorize it, then move it to the
    /// Archive (and back). Needs `ISYNCYOU_TEST_TOKEN` (Mail.ReadWrite).
    #[test]
    #[ignore = "live: opt-in integration test; needs ISYNCYOU_* credentials, run with --ignored"]
    fn live_flag_read_categories_move() {
        let token = match std::env::var("ISYNCYOU_TEST_TOKEN") {
            Ok(t) => t,
            Err(_) => {
                eprintln!("skipping live_flag_read_categories_move: ISYNCYOU_TEST_TOKEN not set");
                return;
            }
        };
        let c = GraphClient::new(token);
        // a real message to act on (newest in the mailbox)
        let msgs = c
            .get_json("/me/messages?$top=1&$select=id,parentFolderId")
            .expect("list messages");
        let msg = msgs["value"]
            .get(0)
            .expect("the test account must have at least one message");
        let id = msg["id"].as_str().expect("message id").to_string();
        let origin = msg["parentFolderId"].as_str().unwrap_or("").to_string();

        // metadata PATCHes (each returns the updated message)
        c.set_flag(&id, "flagged", None, "UTC").expect("set_flag");
        c.set_read(&id, true).expect("set_read");
        c.set_categories(&id, &["iSyncYou Live Test".to_string()])
            .expect("set_categories");
        c.set_importance(&id, "high").expect("set_importance");
        eprintln!("live flag/read/categories/importance applied to {id}");

        // move to the well-known Archive folder, then back to where it came from
        let archive = c
            .get_json("/me/mailFolders/archive?$select=id")
            .expect("resolve archive folder");
        let archive_id = archive["id"].as_str().expect("archive id");
        let moved = c.move_message(&id, archive_id).expect("move to archive");
        let new_id = moved["id"].as_str().expect("moved message id").to_string();
        eprintln!("live moved {id} -> archive as {new_id}");
        if !origin.is_empty() {
            // restore: move it back so the mailbox is left as we found it
            c.move_message(&new_id, &origin).expect("move back");
            eprintln!("live moved back to origin folder");
        }
    }
}
