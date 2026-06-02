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
}

impl GraphClient {
    pub fn new(access_token: impl Into<String>) -> Self {
        GraphClient {
            client: reqwest::blocking::Client::new(),
            token: access_token.into(),
        }
    }

    /// Build with a custom reqwest client (timeouts, proxy, …).
    pub fn with_client(client: reqwest::blocking::Client, access_token: impl Into<String>) -> Self {
        GraphClient {
            client,
            token: access_token.into(),
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
        match self.client.get(url).bearer_auth(&self.token).send() {
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
        if (data.len() as u64) <= CHUNK_ALIGN {
            return self.simple_upload(dest_path, data);
        }
        let mut session = self.create_upload_session(dest_path, data.len() as u64)?;
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
                }
                200 | 201 => {
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

    /// Download a drive item's content by id (follows the redirect to the
    /// pre-signed download URL).
    pub fn download_content(&self, item_id: &str) -> Result<Vec<u8>, UploadError> {
        self.get_bytes(&format!("{GRAPH}/me/drive/items/{item_id}/content"))
    }

    /// Download a mail message's full MIME (`.eml`) by id.
    pub fn download_message_mime(&self, message_id: &str) -> Result<Vec<u8>, UploadError> {
        self.get_bytes(&format!("{GRAPH}/me/messages/{message_id}/$value"))
    }
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
