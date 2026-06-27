//! EXPERIMENTAL Claude subscription provider — UNSUPPORTED, personal-build only
//! (S-AG.12 / #627). Behind the default-off `agent-subscription-experimental` feature;
//! never built into release artifacts, CI, or referenced in the README (see the risk
//! register, R8).
//!
//! Auth model (deliberate, operator-decided): the operator first performs the **official
//! OAuth device-code login** to their own account, then this harness uses the
//! legitimately-obtained access token — the legitimate login happens first, the harness
//! takes over afterwards (not an auth bypass). The operator accepts responsibility for
//! compliance with the provider's terms.
//!
//! **No reproducible recipe lives in the repo.** The endpoint, the identity/billing
//! headers, and any system-prompt preamble all come from a local, *uncommitted*
//! [`SubscriptionConfig`] file. This module only carries the *structure*: it reads the
//! token plus the operator-supplied headers, then makes a Messages-shaped request
//! (reusing the official Anthropic wire format from [`super::anthropic`]).

use super::{anthropic, AssistantBlock, LlmProvider, StreamEvent, Usage};
use crate::turn::Message;
use crate::AgentError;
use serde::Deserialize;
use std::path::Path;

/// Local, **uncommitted** configuration supplying the subscription wire details. The
/// operator fills this from their own private notes; nothing here is hardcoded in-tree.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SubscriptionConfig {
    /// The Messages endpoint URL (operator-supplied).
    pub messages_url: String,
    /// Extra request headers (identity/billing) the operator supplies locally.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Optional system-prompt preamble the operator supplies locally.
    #[serde(default)]
    pub system_prefix: Option<String>,
}

impl SubscriptionConfig {
    /// Load the config from a local JSON file (never committed).
    pub fn load(path: &Path) -> Result<Self, AgentError> {
        let s = std::fs::read_to_string(path)
            .map_err(|e| AgentError::Provider(format!("subscription config: {e}")))?;
        serde_json::from_str(&s)
            .map_err(|e| AgentError::Provider(format!("subscription config: {e}")))
    }
}

/// The experimental subscription provider. Holds the OAuth token obtained by the
/// operator's official device-code login + the operator's local wire config.
pub struct SubscriptionProvider {
    http: crate::http::HttpTransport,
    access_token: String,
    model: String,
    system: String,
    cfg: SubscriptionConfig,
    pub last_usage: Usage,
}

impl SubscriptionProvider {
    pub fn new(
        access_token: impl Into<String>,
        model: impl Into<String>,
        system: impl Into<String>,
        cfg: SubscriptionConfig,
    ) -> Result<Self, AgentError> {
        Ok(Self {
            http: crate::http::HttpTransport::new()?,
            access_token: access_token.into(),
            model: model.into(),
            system: system.into(),
            cfg,
            last_usage: Usage::default(),
        })
    }

    /// Bearer (the legitimately-obtained OAuth token) + the operator's local headers.
    fn request_headers(&self) -> Vec<(String, String)> {
        let mut h = vec![
            (
                "authorization".to_string(),
                format!("Bearer {}", self.access_token),
            ),
            ("content-type".to_string(), "application/json".to_string()),
        ];
        h.extend(self.cfg.headers.iter().cloned());
        h
    }

    fn effective_system(&self) -> String {
        match &self.cfg.system_prefix {
            Some(p) => format!("{p}\n\n{}", self.system),
            None => self.system.clone(),
        }
    }
}

impl LlmProvider for SubscriptionProvider {
    fn name(&self) -> &str {
        "subscription"
    }

    fn next(
        &mut self,
        history: &[Message],
        emit: &mut dyn FnMut(StreamEvent),
    ) -> Result<Vec<AssistantBlock>, AgentError> {
        // Reuse the official Messages wire format; only auth/headers/system differ.
        let body = anthropic::build_request(&self.model, &self.effective_system(), history);
        let (status, text) =
            self.http
                .post_json(&self.cfg.messages_url, &self.request_headers(), &body)?;
        if status == 401 || status == 403 {
            return Err(AgentError::Provider(
                "subscription: unauthorized — run the official device-code login first".into(),
            ));
        }
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| AgentError::Provider(format!("subscription: invalid JSON: {e}")))?;
        let (blocks, usage) = anthropic::parse_response(&v)?;
        self.last_usage = usage;
        for b in &blocks {
            if let AssistantBlock::Text(t) = b {
                emit(StreamEvent::Token(t.clone()));
            }
        }
        Ok(blocks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SubscriptionConfig {
        SubscriptionConfig {
            messages_url: "https://example.invalid/v1/messages".into(),
            headers: vec![("x-operator-supplied".into(), "value".into())],
            system_prefix: Some("preamble".into()),
        }
    }

    #[test]
    fn headers_carry_bearer_and_operator_supplied_values() {
        let p = SubscriptionProvider::new("tok123", "m", "base system", cfg()).unwrap();
        let h = p.request_headers();
        assert!(h
            .iter()
            .any(|(k, v)| k == "authorization" && v == "Bearer tok123"));
        assert!(h
            .iter()
            .any(|(k, v)| k == "x-operator-supplied" && v == "value"));
    }

    #[test]
    fn system_prefix_is_prepended_when_configured() {
        let p = SubscriptionProvider::new("t", "m", "base", cfg()).unwrap();
        assert!(p.effective_system().starts_with("preamble"));
        assert!(p.effective_system().contains("base"));
    }

    #[test]
    fn no_endpoint_or_recipe_is_hardcoded() {
        // The endpoint comes only from config; the source must not embed a real one.
        // The needle is assembled at runtime so this assertion does not match itself.
        let src = include_str!("subscription.rs");
        let needle = format!("api.{}.com", "anthropic");
        assert!(
            !src.contains(&needle),
            "no hardcoded subscription endpoint may live in the repo"
        );
    }
}
