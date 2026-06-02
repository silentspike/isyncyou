//! `isyncyou-graph` — Microsoft Graph transport core.
//!
//! This module holds the **pure, network-independent** building blocks of the
//! Graph client so they can be unit-tested deterministically:
//!
//! - [`error`] — classify an HTTP status into a [`GraphAction`] (retry / refresh
//!   auth / resync / precondition-failed / …).
//! - [`throttle`] — an adaptive [`Pacer`] implementing the project rule: no
//!   artificial limit (full speed) until a `429`, then honor `Retry-After`,
//!   back off, probe, and decay back to full speed.
//! - [`upload`] — [`UploadSession`] chunk planning for large OneDrive uploads
//!   (320 KiB-aligned chunks, `< 60 MiB`, `nextExpectedRanges` resume).
//!
//! OAuth (Auth-Code+PKCE / device-code) and the live HTTP client are layered on
//! top of these and exercised against the test account separately.

pub mod auth;
pub mod client;
pub mod error;
#[cfg(feature = "http")]
pub mod http;
pub mod throttle;
pub mod upload;

#[cfg(feature = "http")]
pub use http::GraphClient;

pub use auth::{TokenCache, TokenResponse};
pub use client::{run_delta, DeltaError, DeltaOutcome, Response, Transport};
pub use error::{classify, GraphAction};
pub use throttle::{Outcome, Pacer};
pub use upload::{ChunkPlan, UploadSession};

/// An opaque Microsoft Graph delta token. Never construct or parse the inner
/// value — it is only ever round-tripped through the store and back to Graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaCursor(String);

impl DeltaCursor {
    pub fn new(token: impl Into<String>) -> Self {
        DeltaCursor(token.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn into_inner(self) -> String {
        self.0
    }
}
