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
            prefer_immutable_id: false,
        }
    }

    /// Build with a custom reqwest client (timeouts, proxy, …).
    pub fn with_client(client: reqwest::blocking::Client, access_token: impl Into<String>) -> Self {
        GraphClient {
            client,
            token: access_token.into(),
            prefer_immutable_id: false,
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
        let url = format!("{GRAPH}/me/drive/root:/{}:/content", enc(dest_path));
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
            "{GRAPH}/me/drive/root:/{}:/createUploadSession",
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
    /// persisted session for this exact file (validated via [`upload_status`]); on
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
        let url = format!("{GRAPH}/me/drive/items/{item_id}/content");
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
        let url = format!("{GRAPH}/me/drive/items/{item_id}/content");
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

    /// Delete a drive item by id (used for test cleanup on the throwaway account).
    pub fn delete_item(&self, item_id: &str) -> Result<(), UploadError> {
        let url = format!("{GRAPH}/me/drive/items/{item_id}");
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
    pub fn get_bytes(&self, url: &str) -> Result<Vec<u8>, UploadError> {
        let resp = self
            .client
            .get(url)
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
        let url = abs(url);
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
        self.get_bytes(&format!("{GRAPH}/me/drive/items/{item_id}/content"))
    }

    /// Download a mail message's full MIME (`.eml`) by id.
    pub fn download_message_mime(&self, message_id: &str) -> Result<Vec<u8>, UploadError> {
        self.get_bytes(&format!("{GRAPH}/me/messages/{message_id}/$value"))
    }

    /// POST a JSON body to a Graph collection and return the created resource
    /// (used by restore: re-create events/tasks/contacts). `url` may be absolute
    /// or a `/me/...` path (prefixed with the Graph base).
    pub fn post_json(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, UploadError> {
        let url = abs(url);
        let resp = self
            .client
            .post(&url)
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
        let url = abs(url);
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

    /// Delete a OneNote page by id (`DELETE /me/onenote/pages/{id}`). OneNote is
    /// eventually consistent, so a freshly-created page may 404 until it propagates;
    /// callers retry. Used for test cleanup on the throwaway account.
    pub fn delete_onenote_page(&self, page_id: &str) -> Result<(), UploadError> {
        let url = format!("{GRAPH}/me/onenote/pages/{page_id}");
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
        let url = abs(url);
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

/// Prefix a bare `/me/...` path with the Graph base; pass absolute URLs through.
fn abs(url: &str) -> String {
    if url.starts_with("http") {
        url.to_string()
    } else {
        format!("{GRAPH}{url}")
    }
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
        assert_eq!(abs("/me/events"), format!("{GRAPH}/me/events"));
        assert_eq!(abs("https://x/y"), "https://x/y");
    }

    /// Live OneDrive delta against the test account. Skips unless
    /// `ISYNCYOU_TEST_TOKEN` (a Files.Read bearer token for the throwaway
    /// account) is set, so CI without credentials passes.
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
