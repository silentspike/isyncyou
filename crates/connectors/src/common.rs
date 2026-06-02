//! Shared connector helpers.

use crate::onedrive::SyncError;
use isyncyou_graph::Transport;
use serde_json::Value;
use std::path::{Path, PathBuf};

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

/// Deterministic 2-level sharded archive path for a remote id, e.g.
/// `<root>/<service>/ab/cd/<hash>.<ext>`. Hashing the id keeps directories
/// balanced and the filename filesystem-safe — Graph ids contain `/`, `=` and
/// other characters that are illegal or awkward in path components.
pub(crate) fn shard_path(root: &Path, service: &str, id: &str, ext: &str) -> PathBuf {
    let hex = format!("{:016x}", fnv1a64(id));
    root.join(service)
        .join(&hex[0..2])
        .join(&hex[2..4])
        .join(format!("{hex}.{ext}"))
}

/// FNV-1a 64-bit — a tiny, dependency-free hash; used only to derive balanced
/// shard directories and a safe filename, never for security.
fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}
