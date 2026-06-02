//! Delta-sync orchestration over an abstract [`Transport`] (plan §5, §6).
//!
//! [`run_delta`] walks a Microsoft Graph delta query to completion: it follows
//! `@odata.nextLink` pages, collects items, persists nothing itself (the caller
//! stores the returned [`DeltaCursor`]), retries transient failures up to a cap,
//! and on `410 Gone` restarts from the base URL with an empty token
//! (`resyncChanges`).
//!
//! The HTTP layer is abstracted behind [`Transport`] so the orchestration is
//! unit-tested deterministically with a mock; the real reqwest-based transport
//! (and the inter-request pacing via [`crate::Pacer`]) wraps this.

use crate::error::{classify, GraphAction};
use crate::DeltaCursor;
use std::time::Duration;

/// A minimal HTTP response as far as the delta loop cares.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub retry_after: Option<Duration>,
    pub body: Option<serde_json::Value>,
}

impl Response {
    pub fn ok(body: serde_json::Value) -> Self {
        Response {
            status: 200,
            retry_after: None,
            body: Some(body),
        }
    }
    pub fn status(status: u16) -> Self {
        Response {
            status,
            retry_after: None,
            body: None,
        }
    }
}

/// Abstract HTTP GET. The real implementation adds auth headers, TLS, etc.
pub trait Transport {
    fn get(&mut self, url: &str) -> Response;

    /// Wait out a retry backoff before re-issuing a throttled request. The
    /// default is a **no-op** so unit-test mocks don't actually sleep; the real
    /// HTTP transport overrides it to sleep `delay`. Honoring this is what lets a
    /// `429`/`5xx` survive (plan §5: full speed → on 429 back off → resume).
    fn backoff(&self, _delay: Duration) {}
}

/// The result of a completed delta walk.
#[derive(Debug, Clone, PartialEq)]
pub struct DeltaOutcome {
    /// Raw item objects gathered across all pages (in arrival order).
    pub items: Vec<serde_json::Value>,
    /// The new opaque cursor to persist for the next incremental run.
    pub cursor: DeltaCursor,
    /// True if a `410 Gone` forced a full resync during this walk.
    pub resynced: bool,
}

/// Errors that abort a delta walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeltaError {
    /// A 2xx page had no body to parse.
    MissingBody,
    /// A 2xx page had neither `@odata.nextLink` nor `@odata.deltaLink`.
    NoCursor,
    /// Retry budget exhausted.
    TooManyRetries,
    /// 401 — token needs refreshing before retrying.
    AuthExpired,
    /// Non-retryable HTTP status.
    Fatal(u16),
}

/// Walk a delta query to completion.
///
/// `base_url` is the un-tokened delta endpoint (used for the initial run and for
/// `410` resync); `start_cursor` is the persisted token for an incremental run.
pub fn run_delta<T: Transport>(
    transport: &mut T,
    base_url: &str,
    start_cursor: Option<&DeltaCursor>,
    max_retries: u32,
) -> Result<DeltaOutcome, DeltaError> {
    let mut url = match start_cursor {
        Some(c) => c.as_str().to_string(),
        None => base_url.to_string(),
    };
    let mut items: Vec<serde_json::Value> = Vec::new();
    let mut resynced = false;
    let mut retries = 0u32;
    // Drives the backoff between retries: full speed until a 429/5xx, then honor
    // Retry-After / exponential backoff, decaying back to full speed on success.
    let mut pacer = crate::throttle::Pacer::new();

    loop {
        let resp = transport.get(&url);
        match classify(resp.status, resp.retry_after) {
            GraphAction::Ok => {
                retries = 0;
                pacer.update(crate::throttle::Outcome::Ok); // decay toward full speed
                let body = resp.body.ok_or(DeltaError::MissingBody)?;
                if let Some(arr) = body.get("value").and_then(|v| v.as_array()) {
                    items.extend(arr.iter().cloned());
                }
                if let Some(next) = body.get("@odata.nextLink").and_then(|v| v.as_str()) {
                    url = next.to_string();
                    continue;
                }
                if let Some(delta) = body.get("@odata.deltaLink").and_then(|v| v.as_str()) {
                    return Ok(DeltaOutcome {
                        items,
                        cursor: DeltaCursor::new(delta),
                        resynced,
                    });
                }
                return Err(DeltaError::NoCursor);
            }
            GraphAction::Retry { after } => {
                retries += 1;
                if retries > max_retries {
                    return Err(DeltaError::TooManyRetries);
                }
                // Honor Retry-After (or exponential backoff) BEFORE retrying the
                // same url — otherwise a real 429 burns the whole retry budget in
                // a few milliseconds and the walk fails under load.
                let delay = pacer.update(crate::throttle::Outcome::Retry { after });
                transport.backoff(delay);
                continue;
            }
            GraphAction::Resync => {
                // 410 Gone: discard partial results and restart from base.
                items.clear();
                resynced = true;
                retries = 0;
                url = base_url.to_string();
                continue;
            }
            GraphAction::RefreshAuth => return Err(DeltaError::AuthExpired),
            _ => return Err(DeltaError::Fatal(resp.status)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Returns queued responses in order, ignoring the url.
    struct MockTransport {
        queue: Vec<Response>,
        calls: usize,
    }
    impl MockTransport {
        fn new(queue: Vec<Response>) -> Self {
            MockTransport { queue, calls: 0 }
        }
    }
    impl Transport for MockTransport {
        fn get(&mut self, _url: &str) -> Response {
            let r = self.queue[self.calls].clone();
            self.calls += 1;
            r
        }
    }

    #[test]
    fn follows_pages_then_returns_delta_cursor() {
        let mut t = MockTransport::new(vec![
            Response::ok(json!({ "value": [{"id": "a"}], "@odata.nextLink": "u2" })),
            Response::ok(json!({ "value": [{"id": "b"}], "@odata.deltaLink": "TOKEN" })),
        ]);
        let out = run_delta(&mut t, "base", None, 5).unwrap();
        assert_eq!(out.items.len(), 2);
        assert_eq!(out.cursor.as_str(), "TOKEN");
        assert!(!out.resynced);
        assert_eq!(t.calls, 2);
    }

    #[test]
    fn incremental_run_starts_from_cursor() {
        let mut t = MockTransport::new(vec![Response::ok(
            json!({ "value": [], "@odata.deltaLink": "T2" }),
        )]);
        let cur = DeltaCursor::new("PREV");
        let out = run_delta(&mut t, "base", Some(&cur), 5).unwrap();
        assert!(out.items.is_empty());
        assert_eq!(out.cursor.as_str(), "T2");
    }

    #[test]
    fn retries_then_succeeds() {
        let mut t = MockTransport::new(vec![
            Response::status(429),
            Response::status(503),
            Response::ok(json!({ "value": [{"id": "a"}], "@odata.deltaLink": "T" })),
        ]);
        let out = run_delta(&mut t, "base", None, 5).unwrap();
        assert_eq!(out.items.len(), 1);
    }

    #[test]
    fn retry_budget_is_enforced() {
        let mut t = MockTransport::new(vec![Response::status(429); 10]);
        assert_eq!(
            run_delta(&mut t, "base", None, 3),
            Err(DeltaError::TooManyRetries)
        );
    }

    /// Records the backoff delays without sleeping, so we can assert run_delta
    /// actually waits between retries (the throttle fix) at unit-test speed.
    struct BackoffSpy {
        queue: Vec<Response>,
        calls: usize,
        waited: std::cell::RefCell<Vec<Duration>>,
    }
    impl Transport for BackoffSpy {
        fn get(&mut self, _url: &str) -> Response {
            let r = self.queue[self.calls].clone();
            self.calls += 1;
            r
        }
        fn backoff(&self, delay: Duration) {
            self.waited.borrow_mut().push(delay);
        }
    }

    #[test]
    fn retry_backs_off_exponentially_without_retry_after() {
        let mut t = BackoffSpy {
            queue: vec![
                Response::status(429),
                Response::status(503),
                Response::ok(json!({ "value": [{"id": "a"}], "@odata.deltaLink": "T" })),
            ],
            calls: 0,
            waited: std::cell::RefCell::new(Vec::new()),
        };
        let out = run_delta(&mut t, "base", None, 5).unwrap();
        assert_eq!(out.items.len(), 1);
        let w = t.waited.borrow();
        assert_eq!(w.len(), 2, "one backoff per throttle response");
        assert!(w[0] > Duration::ZERO, "must wait, not retry instantly");
        assert!(w[1] > w[0], "backoff must grow without Retry-After: {w:?}");
    }

    #[test]
    fn retry_honors_server_retry_after() {
        let mut t = BackoffSpy {
            queue: vec![
                Response {
                    status: 429,
                    retry_after: Some(Duration::from_secs(7)),
                    body: None,
                },
                Response::ok(json!({ "value": [], "@odata.deltaLink": "T" })),
            ],
            calls: 0,
            waited: std::cell::RefCell::new(Vec::new()),
        };
        run_delta(&mut t, "base", None, 5).unwrap();
        assert_eq!(t.waited.borrow().as_slice(), &[Duration::from_secs(7)]);
    }

    #[test]
    fn success_decays_backoff_back_toward_full_speed() {
        // throttle once (sets backoff high), then several successful pages: the
        // pacer should decay so a later throttle starts low again, not stay high.
        let mut t = BackoffSpy {
            queue: vec![
                Response::status(429),
                Response::ok(json!({ "value": [{"id": "a"}], "@odata.nextLink": "u2" })),
                Response::ok(json!({ "value": [{"id": "b"}], "@odata.nextLink": "u3" })),
                Response::status(429),
                Response::ok(json!({ "value": [{"id": "c"}], "@odata.deltaLink": "T" })),
            ],
            calls: 0,
            waited: std::cell::RefCell::new(Vec::new()),
        };
        run_delta(&mut t, "base", None, 5).unwrap();
        let w = t.waited.borrow();
        // both throttles start from the same base step (decay reset to full speed
        // between them), so the second isn't larger than the first.
        assert_eq!(w.len(), 2);
        assert!(
            w[1] <= w[0],
            "decay should reset backoff between throttles: {w:?}"
        );
    }

    #[test]
    fn gone_triggers_resync_from_base() {
        let mut t = MockTransport::new(vec![
            // incremental page with a stale cursor returns some items...
            Response::ok(json!({ "value": [{"id": "stale"}], "@odata.nextLink": "u2" })),
            // ...then 410 Gone
            Response::status(410),
            // resync from base: fresh full snapshot
            Response::ok(json!({ "value": [{"id": "fresh"}], "@odata.deltaLink": "T" })),
        ]);
        let out = run_delta(&mut t, "base", Some(&DeltaCursor::new("c")), 5).unwrap();
        assert!(out.resynced);
        // partial pre-410 items were discarded
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.items[0]["id"], "fresh");
    }

    #[test]
    fn auth_and_fatal_errors() {
        let mut t = MockTransport::new(vec![Response::status(401)]);
        assert_eq!(
            run_delta(&mut t, "base", None, 5),
            Err(DeltaError::AuthExpired)
        );
        let mut t = MockTransport::new(vec![Response::status(403)]);
        assert_eq!(
            run_delta(&mut t, "base", None, 5),
            Err(DeltaError::Fatal(403))
        );
    }

    #[test]
    fn page_without_cursor_is_an_error() {
        let mut t = MockTransport::new(vec![Response::ok(json!({ "value": [] }))]);
        assert_eq!(
            run_delta(&mut t, "base", None, 5),
            Err(DeltaError::NoCursor)
        );
    }
}
