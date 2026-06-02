//! Shared connector helpers.

use crate::onedrive::SyncError;
use isyncyou_graph::Transport;
use serde_json::Value;

/// Page through a plain (non-delta) Graph collection, following `@odata.nextLink`
/// and collecting every `value[]` entry. Used for resource *lists* (mail folders,
/// calendars, contact folders) which terminate on the absence of a next link
/// rather than an `@odata.deltaLink` (so [`isyncyou_graph::run_delta`] can't be
/// used — it requires a delta token to finish).
pub(crate) fn fetch_pages<T: Transport>(
    transport: &mut T,
    start_url: &str,
) -> Result<Vec<Value>, SyncError> {
    let mut url = start_url.to_string();
    let mut out = Vec::new();
    loop {
        let resp = transport.get(&url);
        if !(200..300).contains(&resp.status) {
            return Err(SyncError::Remote(format!(
                "HTTP {} listing {url}",
                resp.status
            )));
        }
        let body = resp
            .body
            .ok_or_else(|| SyncError::Malformed("empty list page".into()))?;
        if let Some(arr) = body.get("value").and_then(Value::as_array) {
            out.extend(arr.iter().cloned());
        }
        match body.get("@odata.nextLink").and_then(Value::as_str) {
            Some(next) => url = next.to_string(),
            None => break,
        }
    }
    Ok(out)
}
