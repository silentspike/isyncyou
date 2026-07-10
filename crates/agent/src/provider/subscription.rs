//! Claude OAuth-compatible provider runtime.
//!
//! Product auth is app OAuth plus encrypted credential storage. The #627 experimental
//! build may additionally use local `claude` CLI credentials for private drift/capture
//! work, but that fallback is outside the product boundary.
//!
//! The mimicry recipe (endpoint + the Claude-Code identity headers + the billing system
//! block) is verified against the real client and lives here as an in-repo default.

use super::{anthropic, AssistantBlock, LlmProvider, StreamEvent, Usage};
use crate::turn::Message;
use crate::AgentError;
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use serde_json::{json, Value};

/// Default Claude-Code mimicry recipe (verified from the real client, 2026-06-27).
const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages?beta=true";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const ANTHROPIC_BETA: &str = "claude-code-20250219,oauth-2025-04-20";
const DEFAULT_CLI_VERSION: &str = "2.1.195";

/// The subscription wire configuration. Defaults to the verified Claude-Code recipe; the
/// operator may override the CLI version or supply account identity for `metadata.user_id`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SubscriptionConfig {
    /// Messages endpoint.
    pub messages_url: String,
    /// The `claude --version` string mimicked in the user-agent and billing block.
    pub cli_version: String,
    /// Optional account UUID for `metadata.user_id` (from the OAuth account / claude.json).
    pub account_uuid: String,
    /// Optional device id for `metadata.user_id`.
    pub device_id: String,
}

impl Default for SubscriptionConfig {
    fn default() -> Self {
        Self {
            messages_url: MESSAGES_URL.to_string(),
            cli_version: DEFAULT_CLI_VERSION.to_string(),
            account_uuid: String::new(),
            device_id: String::new(),
        }
    }
}

impl SubscriptionConfig {
    /// Load operator overrides from a local JSON file (optional; defaults are the recipe).
    pub fn load(path: &std::path::Path) -> Result<Self, AgentError> {
        let s = std::fs::read_to_string(path)
            .map_err(|e| AgentError::Provider(format!("subscription config: {e}")))?;
        serde_json::from_str(&s)
            .map_err(|e| AgentError::Provider(format!("subscription config: {e}")))
    }
}

/// A random UUIDv4 string for `x-claude-code-session-id` (the real client uses `uuid4()`).
fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    let _ = SystemRandom::new().fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = |r: &[u8]| r.iter().map(|x| format!("{x:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        h(&b[0..4]),
        h(&b[4..6]),
        h(&b[6..8]),
        h(&b[8..10]),
        h(&b[10..16])
    )
}

/// The Claude OAuth-compatible provider. Holds the app-obtained OAuth token and the
/// recipe config; shapes each request to the Claude Code-compatible wire format.
pub struct SubscriptionProvider {
    http: crate::http::HttpTransport,
    access_token: String,
    model: String,
    system: String,
    session_id: String,
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
            session_id: uuid_v4(),
            cfg,
            last_usage: Usage::default(),
        })
    }

    /// The Claude-Code identity headers + Bearer (the legitimately-obtained OAuth token).
    fn request_headers(&self) -> Vec<(String, String)> {
        vec![
            (
                "authorization".to_string(),
                format!("Bearer {}", self.access_token),
            ),
            (
                "anthropic-version".to_string(),
                ANTHROPIC_VERSION.to_string(),
            ),
            ("anthropic-beta".to_string(), ANTHROPIC_BETA.to_string()),
            (
                "anthropic-dangerous-direct-browser-access".to_string(),
                "true".to_string(),
            ),
            (
                "user-agent".to_string(),
                format!("claude-cli/{} (external, sdk-cli)", self.cfg.cli_version),
            ),
            ("x-app".to_string(), "cli".to_string()),
            (
                "x-claude-code-session-id".to_string(),
                self.session_id.clone(),
            ),
            ("content-type".to_string(), "application/json".to_string()),
            ("accept".to_string(), "application/json".to_string()),
        ]
    }

    /// `system` as the two-block array: the billing header block (which identifies the
    /// request as Claude Code so the subscription serves Opus/Sonnet) then the real prompt.
    fn system_blocks(&self) -> Value {
        json!([
            {"type": "text", "text": format!(
                "x-anthropic-billing-header: cc_version={}.cab; cc_entrypoint=sdk-cli; cch=00000;",
                self.cfg.cli_version
            )},
            {"type": "text", "text": self.system, "cache_control": {"type": "ephemeral"}},
        ])
    }

    fn metadata(&self) -> Value {
        json!({
            "user_id": json!({
                "device_id": self.cfg.device_id,
                "account_uuid": self.cfg.account_uuid,
                "session_id": self.session_id,
            }).to_string()
        })
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
        // Reuse the official Messages shaping; only system blocks + headers + metadata differ.
        let mut body = anthropic::build_request_blocks(&self.model, self.system_blocks(), history);
        body["metadata"] = self.metadata();
        let (status, text) =
            self.http
                .post_json(&self.cfg.messages_url, &self.request_headers(), &body)?;
        if status == 401 || status == 403 {
            return Err(AgentError::Provider(
                "subscription: unauthorized — run the official login first".into(),
            ));
        }
        let v: Value = serde_json::from_str(&text)
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

    #[test]
    fn default_config_is_the_claude_code_recipe() {
        let c = SubscriptionConfig::default();
        assert_eq!(
            c.messages_url,
            "https://api.anthropic.com/v1/messages?beta=true"
        );
        assert_eq!(c.cli_version, "2.1.195");
    }

    #[test]
    fn headers_mimic_the_claude_code_client() {
        let p = SubscriptionProvider::new(
            "tok123",
            "claude-x",
            "base system",
            SubscriptionConfig::default(),
        )
        .unwrap();
        let h = p.request_headers();
        let get = |k: &str| h.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("authorization").unwrap(), "Bearer tok123");
        assert_eq!(get("anthropic-version").unwrap(), "2023-06-01");
        assert_eq!(
            get("anthropic-beta").unwrap(),
            "claude-code-20250219,oauth-2025-04-20"
        );
        assert_eq!(get("x-app").unwrap(), "cli");
        assert_eq!(
            get("user-agent").unwrap(),
            "claude-cli/2.1.195 (external, sdk-cli)"
        );
        assert!(get("x-claude-code-session-id").is_some());
    }

    #[test]
    fn first_system_block_is_the_billing_header() {
        let p = SubscriptionProvider::new(
            "t",
            "m",
            "the real system prompt",
            SubscriptionConfig::default(),
        )
        .unwrap();
        let s = p.system_blocks();
        let first = s[0]["text"].as_str().unwrap();
        assert!(first.starts_with("x-anthropic-billing-header: cc_version=2.1.195.cab;"));
        assert!(first.contains("cc_entrypoint=sdk-cli"));
        assert!(first.contains("cch=00000"));
        // second block is the real prompt, cached
        assert_eq!(s[1]["text"], "the real system prompt");
        assert_eq!(s[1]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn metadata_user_id_is_a_json_string_with_session() {
        let cfg = SubscriptionConfig {
            account_uuid: "acc-1".into(),
            device_id: "dev-1".into(),
            ..Default::default()
        };
        let p = SubscriptionProvider::new("t", "m", "s", cfg).unwrap();
        let meta = p.metadata();
        let uid: Value = serde_json::from_str(meta["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(uid["account_uuid"], "acc-1");
        assert_eq!(uid["device_id"], "dev-1");
        assert!(uid["session_id"].as_str().unwrap().contains('-'));
    }

    #[test]
    fn uuid_v4_has_version_and_variant_bits() {
        let u = uuid_v4();
        let parts: Vec<&str> = u.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert!(parts[2].starts_with('4')); // version 4
        assert!(matches!(
            parts[3].chars().next().unwrap(),
            '8' | '9' | 'a' | 'b'
        ));
    }
}
