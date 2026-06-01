//! Classification of Microsoft Graph HTTP responses into a recovery action.

use std::time::Duration;

/// What the engine should do in response to a Graph HTTP status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphAction {
    /// 2xx — proceed.
    Ok,
    /// Transient — retry after an optional server-specified delay (429/5xx/408).
    Retry { after: Option<Duration> },
    /// 401 — access token expired/invalid; refresh and retry.
    RefreshAuth,
    /// 410 — delta token gone; resync from scratch.
    Resync,
    /// 412 — `If-Match`/optimistic-concurrency precondition failed; re-fetch + reconcile.
    PreconditionFailed,
    /// 404 — resource not found; treat as deleted, or restart an upload session.
    NotFound,
    /// 416 — requested range not satisfiable; query `nextExpectedRanges` and resume.
    RangeNotSatisfiable,
    /// 507 — quota exceeded; pause this transfer.
    InsufficientStorage,
    /// Non-retryable client error (e.g. 400, 403, 405).
    Fatal,
}

impl GraphAction {
    /// Whether the request should be retried (possibly after a delay).
    pub fn is_retryable(&self) -> bool {
        matches!(self, GraphAction::Retry { .. })
    }
}

/// Map an HTTP status (+ a parsed `Retry-After`, if present) to a [`GraphAction`].
pub fn classify(status: u16, retry_after: Option<Duration>) -> GraphAction {
    match status {
        200..=299 => GraphAction::Ok,
        401 => GraphAction::RefreshAuth,
        404 => GraphAction::NotFound,
        408 => GraphAction::Retry { after: retry_after },
        410 => GraphAction::Resync,
        412 => GraphAction::PreconditionFailed,
        416 => GraphAction::RangeNotSatisfiable,
        // 429 and the retryable server errors honor Retry-After when present.
        429 | 500 | 502 | 503 | 504 | 509 => GraphAction::Retry { after: retry_after },
        507 => GraphAction::InsufficientStorage,
        _ => GraphAction::Fatal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_is_ok() {
        assert_eq!(classify(200, None), GraphAction::Ok);
        assert_eq!(classify(204, None), GraphAction::Ok);
    }

    #[test]
    fn rate_limited_carries_retry_after() {
        let d = Some(Duration::from_secs(14));
        assert_eq!(classify(429, d), GraphAction::Retry { after: d });
        assert!(classify(429, None).is_retryable());
    }

    #[test]
    fn server_errors_are_retryable() {
        for s in [500, 502, 503, 504, 509, 408] {
            assert!(classify(s, None).is_retryable(), "status {s} should retry");
        }
    }

    #[test]
    fn specific_statuses_map_to_specific_actions() {
        assert_eq!(classify(401, None), GraphAction::RefreshAuth);
        assert_eq!(classify(404, None), GraphAction::NotFound);
        assert_eq!(classify(410, None), GraphAction::Resync);
        assert_eq!(classify(412, None), GraphAction::PreconditionFailed);
        assert_eq!(classify(416, None), GraphAction::RangeNotSatisfiable);
        assert_eq!(classify(507, None), GraphAction::InsufficientStorage);
    }

    #[test]
    fn other_client_errors_are_fatal() {
        for s in [400, 403, 405, 422] {
            assert_eq!(classify(s, None), GraphAction::Fatal, "status {s}");
            assert!(!classify(s, None).is_retryable());
        }
    }
}
