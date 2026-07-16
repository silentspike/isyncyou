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
use std::collections::BTreeMap;

/// Default Claude-Code mimicry recipe (verified from the real client, 2026-06-27).
pub(crate) const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages?beta=true";
pub(super) const ANTHROPIC_VERSION: &str = "2023-06-01";
pub(super) const ANTHROPIC_BETA: &str = "claude-code-20250219,oauth-2025-04-20";
pub(crate) const DEFAULT_CLI_VERSION: &str = "2.1.207";

/// #639 (F4): the exact retained billing-envelope block text the product harness must carry
/// (bound EXACTLY by the runtime attestation, not merely prefix-checked).
pub(crate) fn expected_product_billing_block() -> String {
    format!(
        "x-anthropic-billing-header: cc_version={}.cab; cc_entrypoint=sdk-cli; cch=00000;",
        DEFAULT_CLI_VERSION
    )
}

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
fn uuid_v4() -> Result<String, AgentError> {
    let mut b = [0u8; 16];
    SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| AgentError::Provider("provider session RNG unavailable".into()))?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = |r: &[u8]| r.iter().map(|x| format!("{x:02x}")).collect::<String>();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        h(&b[0..4]),
        h(&b[4..6]),
        h(&b[6..8]),
        h(&b[8..10]),
        h(&b[10..16])
    ))
}

/// The Claude OAuth-compatible provider. Holds the app-obtained OAuth token and the
/// recipe config; shapes each request to the Claude Code-compatible wire format.
pub struct SubscriptionProvider {
    http: crate::http::ProductHttpTransport,
    access_token: String,
    model: String,
    system: String,
    pub(super) session_id: String,
    pub(super) cfg: SubscriptionConfig,
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
            http: crate::http::ProductHttpTransport::shared()?,
            access_token: access_token.into(),
            model: model.into(),
            system: system.into(),
            session_id: uuid_v4()?,
            cfg,
            last_usage: Usage::default(),
        })
    }

    /// The Claude-Code identity headers + Bearer (the legitimately-obtained OAuth token).
    pub(crate) fn request_headers(&self) -> Vec<(String, String)> {
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
            ("accept".to_string(), "text/event-stream".to_string()),
        ]
    }

    /// `system` as the two-block array: the billing header block (which identifies the
    /// request as Claude Code so the subscription serves Opus/Sonnet) then the real prompt.
    pub(crate) fn system_blocks(&self) -> Value {
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

    pub(crate) fn request_body(&self, history: &[Message]) -> Value {
        let mut body = anthropic::build_request_blocks(&self.model, self.system_blocks(), history);
        body["metadata"] = self.metadata();
        body["stream"] = json!(true);
        body
    }
}

#[derive(Debug, Default)]
struct ClaudeStreamState {
    text: String,
    active_tools: BTreeMap<u64, (String, String)>,
    tools: Vec<(String, Value)>,
    usage: Usage,
    failure: Option<String>,
}

fn apply_claude_sse_event(
    data: &str,
    state: &mut ClaudeStreamState,
) -> Result<Option<String>, AgentError> {
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return Ok(None);
    }
    let v: Value = serde_json::from_str(data)
        .map_err(|e| AgentError::Provider(format!("subscription: invalid SSE JSON: {e}")))?;
    match v.get("type").and_then(|t| t.as_str()) {
        Some("message_start") => {
            if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
                let g = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                state.usage.input_tokens = g("input_tokens");
                state.usage.output_tokens = g("output_tokens");
            }
        }
        Some("content_block_start")
            if v.get("content_block")
                .and_then(|b| b.get("type"))
                .and_then(|t| t.as_str())
                == Some("tool_use") =>
        {
            let index = v.get("index").and_then(|x| x.as_u64()).unwrap_or(0);
            let block = &v["content_block"];
            let id = block
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            let args = block
                .get("input")
                .filter(|input| input.as_object().map(|o| !o.is_empty()).unwrap_or(true))
                .map(Value::to_string)
                .unwrap_or_default();
            state.active_tools.insert(index, (id, args));
        }
        Some("content_block_start") => {}
        Some("content_block_delta") => {
            let delta = &v["delta"];
            match delta.get("type").and_then(|t| t.as_str()) {
                Some("text_delta") => {
                    if let Some(text) = delta.get("text").and_then(|x| x.as_str()) {
                        state.text.push_str(text);
                        return Ok(Some(text.to_string()));
                    }
                }
                Some("input_json_delta") => {
                    let index = v.get("index").and_then(|x| x.as_u64()).unwrap_or(0);
                    let partial = delta
                        .get("partial_json")
                        .and_then(|x| x.as_str())
                        .unwrap_or("");
                    let entry = state
                        .active_tools
                        .entry(index)
                        .or_insert_with(|| (String::new(), String::new()));
                    entry.1.push_str(partial);
                }
                _ => {}
            }
        }
        Some("content_block_stop") => {
            let index = v.get("index").and_then(|x| x.as_u64()).unwrap_or(0);
            if let Some((id, args)) = state.active_tools.remove(&index) {
                let input = if args.is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::from_str(&args).unwrap_or(Value::Null)
                };
                state.tools.push((id, input));
            }
        }
        Some("message_delta") => {
            if let Some(u) = v.get("usage") {
                if let Some(output) = u.get("output_tokens").and_then(|x| x.as_u64()) {
                    state.usage.output_tokens = output;
                }
            }
        }
        Some("error") => {
            state.failure = Some(
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("stream error")
                    .to_string(),
            );
        }
        _ => {}
    }
    Ok(None)
}

fn sse_event_advances_turn(data: &str) -> bool {
    serde_json::from_str::<Value>(data)
        .ok()
        .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_owned))
        .is_some_and(|event_type| {
            matches!(
                event_type.as_str(),
                "content_block_start"
                    | "content_block_delta"
                    | "content_block_stop"
                    | "message_delta"
                    | "message_stop"
                    | "error"
            )
        })
}

fn finish_claude_stream(
    state: ClaudeStreamState,
) -> Result<(Vec<AssistantBlock>, Usage), AgentError> {
    if let Some(e) = state.failure {
        return Err(AgentError::Provider(format!("subscription: {e}")));
    }
    let mut blocks = Vec::new();
    if !state.text.is_empty() {
        blocks.push(AssistantBlock::Text(state.text));
    }
    for (id, input) in state.tools {
        blocks.push(AssistantBlock::ToolUse { id, input });
    }
    Ok((blocks, state.usage))
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
        self.next_cancellable(history, emit, None)
    }

    fn next_cancellable(
        &mut self,
        history: &[Message],
        emit: &mut dyn FnMut(StreamEvent),
        cancellation: Option<&crate::CancellationToken>,
    ) -> Result<Vec<AssistantBlock>, AgentError> {
        // #639: build + attest the exact request for THIS round's history, then send only the
        // attested object — the transport's product path accepts nothing un-attested.
        let attested = crate::provider::build_attested_provider_request(
            crate::provider::ProviderRequestBinding::Claude {
                access_token: &self.access_token,
                session_id: &self.session_id,
                account_uuid: &self.cfg.account_uuid,
                device_id: &self.cfg.device_id,
                model: &self.model,
                system: &self.system,
            },
            self.cfg.messages_url.clone(),
            self.request_headers(),
            self.request_body(history),
        )?;
        let mut state = ClaudeStreamState::default();
        let mut parse_error: Option<AgentError> = None;
        let response = self.http.post_attested_sse_cancellable(
            &attested,
            &mut |event| {
                if parse_error.is_some() {
                    return false;
                }
                let advances_turn = sse_event_advances_turn(&event.data);
                match apply_claude_sse_event(&event.data, &mut state) {
                    Ok(Some(delta)) => emit(StreamEvent::Token(delta)),
                    Ok(None) => {}
                    Err(e) => parse_error = Some(e),
                }
                advances_turn
            },
            cancellation,
        )?;
        if response.status == 401 || response.status == 403 {
            return Err(AgentError::Provider(
                "subscription: unauthorized — connect Claude again".into(),
            ));
        }
        if response.status >= 400 {
            let detail = response.body_preview.unwrap_or_default();
            return Err(AgentError::Provider(format!(
                "subscription: backend status {}{}",
                response.status,
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            )));
        }
        if let Some(e) = parse_error {
            return Err(e);
        }
        let (blocks, usage) = finish_claude_stream(state)?;
        let usage = usage.with_provider_response("claude", &self.model, &response.headers);
        self.last_usage = usage;
        Ok(blocks)
    }

    fn last_usage(&self) -> Option<Usage> {
        (!self.last_usage.is_empty()).then(|| self.last_usage.clone())
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
        assert_eq!(c.cli_version, "2.1.207");
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
            "claude-cli/2.1.207 (external, sdk-cli)"
        );
        assert_eq!(get("accept").unwrap(), "text/event-stream");
        assert!(get("x-claude-code-session-id").is_some());
    }

    #[test]
    fn claude_billing_block_remains_first_and_unchanged() {
        let p = SubscriptionProvider::new(
            "t",
            "m",
            "the real system prompt",
            SubscriptionConfig::default(),
        )
        .unwrap();
        let s = p.system_blocks();
        let first = s[0]["text"].as_str().unwrap();
        assert_eq!(
            first,
            "x-anthropic-billing-header: cc_version=2.1.207.cab; cc_entrypoint=sdk-cli; cch=00000;"
        );
        assert_eq!(s.as_array().unwrap().len(), 2);
        // second block is the real prompt, cached
        assert_eq!(s[1]["text"], "the real system prompt");
        assert_eq!(s[1]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn request_body_enables_streaming_and_metadata() {
        let cfg = SubscriptionConfig {
            account_uuid: "acc-1".into(),
            device_id: "dev-1".into(),
            ..Default::default()
        };
        let p = SubscriptionProvider::new("t", "m", "system", cfg).unwrap();
        let body = p.request_body(&[Message::user("hello")]);

        assert_eq!(body["stream"], true);
        assert_eq!(body["model"], "m");
        assert_eq!(body["messages"][0]["role"], "user");
        assert!(body["metadata"]["user_id"]
            .as_str()
            .unwrap()
            .contains("acc-1"));
    }

    #[test]
    fn claude_sse_event_returns_text_delta_immediately() {
        let mut state = ClaudeStreamState::default();
        let delta = apply_claude_sse_event(
            "{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}",
            &mut state,
        )
        .unwrap();

        assert_eq!(delta.as_deref(), Some("hello"));
        assert_eq!(state.text, "hello");
    }

    #[test]
    fn claude_status_events_do_not_satisfy_response_or_extend_idle_deadline() {
        assert!(!sse_event_advances_turn(
            r#"{"type":"message_start","message":{}}"#
        ));
        assert!(sse_event_advances_turn(
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"ok"}}"#
        ));
        assert!(sse_event_advances_turn(r#"{"type":"message_stop"}"#));
    }

    #[test]
    fn claude_sse_extracts_tool_use_and_usage() {
        let mut state = ClaudeStreamState::default();
        for data in [
            "{\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}",
            "{\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"isyncyou\",\"input\":{}}}",
            "{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"op\\\":\"}}",
            "{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"search\\\"}\"}}",
            "{\"type\":\"content_block_stop\",\"index\":1}",
            "{\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}",
        ] {
            apply_claude_sse_event(data, &mut state).unwrap();
        }

        let (blocks, usage) = finish_claude_stream(state).unwrap();
        assert_eq!(usage.input_tokens, 11);
        assert_eq!(usage.output_tokens, 7);
        match &blocks[0] {
            AssistantBlock::ToolUse { id, input } => {
                assert_eq!(id, "toolu_1");
                assert_eq!(input["op"], "search");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn claude_sse_surfaces_error() {
        let mut state = ClaudeStreamState::default();
        apply_claude_sse_event(
            "{\"type\":\"error\",\"error\":{\"message\":\"subscription rejected\"}}",
            &mut state,
        )
        .unwrap();

        assert!(finish_claude_stream(state)
            .unwrap_err()
            .to_string()
            .contains("subscription rejected"));
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
        let u = uuid_v4().unwrap();
        let parts: Vec<&str> = u.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert!(parts[2].starts_with('4')); // version 4
        assert!(matches!(
            parts[3].chars().next().unwrap(),
            '8' | '9' | 'a' | 'b'
        ));
    }
}
