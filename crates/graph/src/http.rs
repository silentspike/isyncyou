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

/// One binary data part for a OneNote multipart page-create request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneNotePagePart {
    pub name: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// Errors from the live upload/delete path.
#[derive(Debug)]
pub enum UploadError {
    Http {
        status: u16,
        body: String,
    },
    Transport(String),
    Parse(String),
    /// The session ended without a completion response.
    Incomplete,
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UploadError::Http { status, body } => write!(f, "HTTP {status}: {body}"),
            UploadError::Transport(e) => write!(f, "transport error: {e}"),
            UploadError::Parse(e) => write!(f, "parse error: {e}"),
            UploadError::Incomplete => write!(f, "upload ended without completion"),
        }
    }
}
impl std::error::Error for UploadError {}

/// A Microsoft Graph HTTP client carrying a bearer access token.
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

impl GraphClient {
    pub fn new(access_token: impl Into<String>) -> Self {
        GraphClient {
            client: reqwest::blocking::Client::new(),
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
            .map_err(|e| UploadError::Transport(e.to_string()))?;
        json_or_err(resp)
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
            .map_err(|e| UploadError::Transport(e.to_string()))?;
        let v = json_or_err(resp)?;
        let upload_url = v.get("uploadUrl").and_then(|u| u.as_str()).ok_or_else(|| {
            UploadError::Parse("createUploadSession response had no uploadUrl".into())
        })?;
        Ok(UploadSession::new(upload_url, total))
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
                .map_err(|e| UploadError::Transport(e.to_string()))?;
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
            .map_err(|e| UploadError::Transport(e.to_string()))?;
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
            .map_err(|e| UploadError::Transport(e.to_string()))?;
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
            .map_err(|e| UploadError::Transport(e.to_string()))?;
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
            .map_err(|e| UploadError::Transport(e.to_string()))?;
        match resp.status().as_u16() {
            200 | 204 => Ok(()),
            s => Err(UploadError::Http {
                status: s,
                body: resp.text().unwrap_or_default().chars().take(200).collect(),
            }),
        }
    }

    /// GET an arbitrary Graph URL and return the raw response body bytes
    /// (follows redirects to pre-signed download URLs). Used for binary/content
    /// endpoints like a drive item's `/content` or a message's `/$value` (MIME).
    /// `url` may be absolute or a `/me/...` path; a relative path is prefixed with
    /// the API base (like [`get_json`](Self::get_json)/[`post_json`](Self::post_json))
    /// — without this, a relative path (e.g. the OneNote page-content URL built by
    /// the archive driver) has no host and reqwest fails with a builder error.
    pub fn get_bytes(&self, url: &str) -> Result<Vec<u8>, UploadError> {
        let url = self.abs(url);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .map_err(|e| UploadError::Transport(e.to_string()))?;
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

    /// GET a Graph resource as JSON (by-ref, unlike the `&mut self` [`Transport`]
    /// poll loop). `url` may be absolute or a `/me/...` path. Used to fetch a
    /// single item's canonical JSON for the content archive.
    pub fn get_json(&self, url: &str) -> Result<serde_json::Value, UploadError> {
        let url = self.abs(url);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .map_err(|e| UploadError::Transport(e.to_string()))?;
        json_or_err(resp)
    }

    /// Download a drive item's content by id (follows the redirect to the
    /// pre-signed download URL).
    pub fn download_content(&self, item_id: &str) -> Result<Vec<u8>, UploadError> {
        self.get_bytes(&format!("{}/me/drive/items/{item_id}/content", self.base))
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
        let url = self.abs(url);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .map_err(|e| UploadError::Transport(e.to_string()))?;
        json_or_err(resp)
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
            .map_err(|e| UploadError::Transport(e.to_string()))?;
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
            .map_err(|e| UploadError::Transport(e.to_string()))?;
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
            .map_err(|e| UploadError::Transport(e.to_string()))?;
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
            .map_err(|e| UploadError::Transport(e.to_string()))?;
        match resp.status().as_u16() {
            200 | 202 | 204 => Ok(()),
            s => Err(UploadError::Http {
                status: s,
                body: resp.text().unwrap_or_default().chars().take(200).collect(),
            }),
        }
    }
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
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        resp.json::<serde_json::Value>()
            .map_err(|e| UploadError::Parse(e.to_string()))
    } else {
        let body = resp.text().unwrap_or_default();
        Err(UploadError::Http {
            status,
            body: body.chars().take(300).collect(),
        })
    }
}

/// Minimal path encoding for OneDrive `root:/PATH:` addressing (spaces only;
/// callers use safe names). Full percent-encoding is a later refinement.
fn enc(path: &str) -> String {
    path.trim_start_matches('/').replace(' ', "%20")
}

/// Percent-encode an item id for safe inclusion in a URL path segment. Outlook
/// message ids are base64-ish (contain `+ / =`), which Graph 404s if left raw in
/// the path; everything outside RFC 3986 unreserved is escaped. Plain alphanumeric
/// ids (e.g. OneDrive drive-item ids) pass through unchanged.
fn encode_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for b in id.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
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
        head
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
        let (base2, _s2) = serve(vec![http_response(503, "Unavailable", "", "busy")]);
        match GraphClient::new("tok").get_bytes(&base2).unwrap_err() {
            UploadError::Http { status, .. } => assert_eq!(status, 503),
            other => panic!("expected Http error, got {other}"),
        }
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
}
