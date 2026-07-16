//! ChatGPT/Codex OAuth-compatible provider runtime.
//!
//! Product auth is app OAuth plus encrypted credential storage. The #627 experimental
//! build may additionally use local `codex` CLI credentials for private drift/capture
//! work, but that fallback is outside the product boundary.
//!
//! Transport: the ChatGPT backend **Responses API over HTTP + SSE** (verified live — the
//! backend accepts plain HTTP POST with `accept: text/event-stream`; no WebSocket needed,
//! although the official client defaults to one). Recipe verified against the real client.

use super::{AssistantBlock, Usage};
use crate::tool::{tool_schema, TOOL_NAME};
use crate::turn::{Message, Role};
use crate::AgentError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Verified Codex-CLI mimicry recipe.
pub(crate) const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
pub(super) const ORIGINATOR: &str = crate::oauth::CODEX_OAUTH_ORIGINATOR;
pub(crate) const DEFAULT_CLI_VERSION: &str = "0.144.4";
const DEFAULT_MODEL: &str = "gpt-5.6-sol";
const MAX_REASONING_CONTEXT_BYTES: usize = 1024 * 1024;
const MAX_REASONING_SUMMARY_BYTES: usize = 64 * 1024;
const MAX_REASONING_ITEMS_PER_ROUND: usize = 4;

/// Closed reasoning levels offered by the product UI and accepted by the Codex backend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexReasoningEffort {
    Low,
    #[default]
    Medium,
    High,
    XHigh,
}

impl CodexReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            _ => None,
        }
    }
}

/// The ChatGPT/Codex wire configuration. Defaults to the verified recipe; the product
/// credential path supplies `account_id` from the encrypted store. #627 experimental
/// builds may derive the same value from local CLI state for drift/capture only.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CodexConfig {
    pub responses_url: String,
    pub account_id: String,
    pub cli_version: String,
    pub model: String,
    pub reasoning_effort: CodexReasoningEffort,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            responses_url: RESPONSES_URL.to_string(),
            account_id: String::new(),
            cli_version: DEFAULT_CLI_VERSION.to_string(),
            model: DEFAULT_MODEL.to_string(),
            reasoning_effort: CodexReasoningEffort::default(),
        }
    }
}

/// The agent's single tool in the Responses `function` shape (vs Anthropic's
/// `input_schema`). Reuses the same name/description/schema so behaviour matches.
pub(super) fn responses_tools() -> Value {
    let s = tool_schema();
    json!([{
        "type": "function",
        "name": s.get("name").cloned().unwrap_or(json!(TOOL_NAME)),
        "description": s.get("description").cloned().unwrap_or(Value::Null),
        "parameters": s.get("input_schema").cloned().unwrap_or(json!({"type":"object"})),
        "strict": false,
    }])
}

/// Map the agent history to the Responses `input` array (message / function_call /
/// function_call_output items), pairing tool_use ↔ tool_result by `call_id`.
pub(crate) fn build_input(history: &[Message]) -> Vec<Value> {
    build_input_with_reasoning(history, &[], 0)
}

#[derive(Clone)]
struct CodexReasoningContext {
    summary: Value,
    encrypted_content: String,
}

impl CodexReasoningContext {
    fn replay_value(&self) -> Value {
        json!({
            "type": "reasoning",
            "summary": self.summary,
            "encrypted_content": self.encrypted_content,
        })
    }
}

fn build_input_with_reasoning(
    history: &[Message],
    reasoning_rounds: &[Vec<CodexReasoningContext>],
    assistant_base: usize,
) -> Vec<Value> {
    let mut out = Vec::new();
    let mut assistant_index = 0usize;
    for m in history {
        match m.role {
            Role::User => out.push(json!({
                "type": "message", "role": "user",
                "content": [{ "type": "input_text", "text": m.content }],
            })),
            Role::Assistant => {
                if let Some(round) = assistant_index
                    .checked_sub(assistant_base)
                    .and_then(|round| reasoning_rounds.get(round))
                {
                    out.extend(round.iter().map(CodexReasoningContext::replay_value));
                }
                assistant_index = assistant_index.saturating_add(1);
                if !m.content.is_empty() {
                    out.push(json!({
                        "type": "message", "role": "assistant",
                        "content": [{ "type": "output_text", "text": m.content }],
                    }));
                }
                for tu in &m.tool_uses {
                    out.push(json!({
                        "type": "function_call",
                        "call_id": tu.id,
                        "name": TOOL_NAME,
                        "arguments": tu.input.to_string(),
                    }));
                }
            }
            Role::Tool => out.push(json!({
                "type": "function_call_output",
                "call_id": m.tool_use_id.clone().unwrap_or_default(),
                "output": m.content,
            })),
        }
    }
    out
}

fn replayable_reasoning_context(item: &Value) -> Result<CodexReasoningContext, AgentError> {
    let object = item
        .as_object()
        .filter(|object| object.get("type").and_then(Value::as_str) == Some("reasoning"))
        .ok_or_else(|| AgentError::Provider("codex: invalid reasoning item".into()))?;
    let encrypted_content = object
        .get("encrypted_content")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= MAX_REASONING_CONTEXT_BYTES)
        .ok_or_else(|| AgentError::Provider("codex: invalid reasoning context".into()))?;
    let summary = object
        .get("summary")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    if !summary.is_array()
        || serde_json::to_vec(&summary)
            .map_err(|_| AgentError::Provider("codex: invalid reasoning summary".into()))?
            .len()
            > MAX_REASONING_SUMMARY_BYTES
    {
        return Err(AgentError::Provider(
            "codex: invalid reasoning summary".into(),
        ));
    }
    Ok(CodexReasoningContext {
        summary,
        encrypted_content: encrypted_content.to_owned(),
    })
}

/// Build the Responses request body. `instructions` carries the system prompt (the
/// Responses API's top-level system field).
pub(crate) fn build_request(
    model: &str,
    reasoning_effort: CodexReasoningEffort,
    instructions: &str,
    history: &[Message],
) -> Value {
    json!({
        "model": model,
        "reasoning": { "effort": reasoning_effort.as_str() },
        "instructions": instructions,
        "input": build_input(history),
        "tools": responses_tools(),
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
    })
}

type CodexToolArgs = (String, String);

fn apply_sse_event(
    data: &str,
    text: &mut String,
    tools: &mut Vec<CodexToolArgs>,
    reasoning: &mut Vec<CodexReasoningContext>,
    usage: &mut Usage,
    failure: &mut Option<String>,
) -> Result<Option<String>, AgentError> {
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return Ok(None);
    }
    let v: Value = serde_json::from_str(data)
        .map_err(|e| AgentError::Provider(format!("codex: invalid SSE JSON: {e}")))?;
    match v.get("type").and_then(|t| t.as_str()) {
        Some("response.output_text.delta") => {
            if let Some(d) = v.get("delta").and_then(|d| d.as_str()) {
                text.push_str(d);
                return Ok(Some(d.to_string()));
            }
        }
        Some("response.output_item.done") => {
            if let Some(item) = v.get("item") {
                match item.get("type").and_then(Value::as_str) {
                    Some("function_call") => {
                        let id = item
                            .get("call_id")
                            .and_then(|x| x.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let args = item
                            .get("arguments")
                            .and_then(|x| x.as_str())
                            .unwrap_or("{}")
                            .to_string();
                        tools.push((id, args));
                    }
                    Some("reasoning") => {
                        if reasoning.len() >= MAX_REASONING_ITEMS_PER_ROUND {
                            return Err(AgentError::Provider(
                                "codex: too many reasoning items".into(),
                            ));
                        }
                        reasoning.push(replayable_reasoning_context(item)?);
                    }
                    _ => {}
                }
            }
        }
        Some("response.completed") => {
            if let Some(u) = v.get("response").and_then(|r| r.get("usage")) {
                let g = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                usage.input_tokens = g("input_tokens");
                usage.output_tokens = g("output_tokens");
            }
        }
        Some("response.failed") => {
            *failure = Some(
                v.get("response")
                    .and_then(|r| r.get("error"))
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("response failed")
                    .to_string(),
            );
        }
        Some("error") => {
            *failure = Some(
                v.get("message")
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
                "response.output_text.delta"
                    | "response.output_item.done"
                    | "response.completed"
                    | "response.failed"
                    | "error"
            )
        })
}

#[cfg(feature = "http")]
fn closed_backend_failure_code(status: u16, detail: &str) -> &'static str {
    let detail = detail.to_ascii_lowercase();
    if detail.contains("reasoning") {
        "reasoning_context"
    } else if detail.contains("function_call") {
        "function_call_pairing"
    } else if detail.contains("context_length") || detail.contains("input token") {
        "context_limit"
    } else if status == 400 {
        "http_bad_request"
    } else if status == 429 {
        "http_rate_limited"
    } else if status >= 500 {
        "http_server_failure"
    } else {
        "http_request_rejected"
    }
}

#[cfg(feature = "http")]
fn closed_provider_failure(code: &str) -> AgentError {
    AgentError::Provider(format!("codex_safe:{code}"))
}

fn finish_sse_blocks(
    text: String,
    tools: Vec<CodexToolArgs>,
    usage: Usage,
    failure: Option<String>,
) -> Result<(Vec<AssistantBlock>, Usage), AgentError> {
    if let Some(e) = failure {
        return Err(AgentError::Provider(format!("codex: {e}")));
    }
    let mut blocks = Vec::new();
    if !text.is_empty() {
        blocks.push(AssistantBlock::Text(text));
    }
    for (id, args) in tools {
        let input: Value = serde_json::from_str(&args).unwrap_or(Value::Null);
        blocks.push(AssistantBlock::ToolUse { id, input });
    }
    Ok((blocks, usage))
}

/// Parse the SSE Responses stream into assistant blocks + usage. Concatenates
/// `response.output_text.delta` text and collects `function_call` output items.
#[cfg(test)]
pub(crate) fn parse_sse(body: &str) -> Result<(Vec<AssistantBlock>, Usage), AgentError> {
    let mut text = String::new();
    let mut tools: Vec<CodexToolArgs> = Vec::new();
    let mut reasoning = Vec::new();
    let mut usage = Usage::default();
    let mut failure: Option<String> = None;

    for line in body.lines() {
        let data = match line.strip_prefix("data: ") {
            Some(d) => d,
            None => continue,
        };
        apply_sse_event(
            data,
            &mut text,
            &mut tools,
            &mut reasoning,
            &mut usage,
            &mut failure,
        )?;
    }

    finish_sse_blocks(text, tools, usage, failure)
}

#[cfg(feature = "http")]
mod live {
    use super::*;
    use crate::provider::{LlmProvider, StreamEvent};

    /// Live ChatGPT/Codex subscription provider over the agent's blocking HTTP transport.
    pub struct CodexProvider {
        http: crate::http::ProductHttpTransport,
        access_token: String,
        instructions: String,
        cfg: CodexConfig,
        reasoning_rounds: Vec<Vec<CodexReasoningContext>>,
        history_assistant_base: Option<usize>,
        pub last_usage: Usage,
    }

    impl CodexProvider {
        pub fn new(
            access_token: impl Into<String>,
            instructions: impl Into<String>,
            cfg: CodexConfig,
        ) -> Result<Self, AgentError> {
            Ok(Self {
                http: crate::http::ProductHttpTransport::shared()?,
                access_token: access_token.into(),
                instructions: instructions.into(),
                cfg,
                reasoning_rounds: Vec::new(),
                history_assistant_base: None,
                last_usage: Usage::default(),
            })
        }

        /// The Codex-CLI identity headers + Bearer (the legitimately-obtained token).
        pub(crate) fn request_headers(&self) -> Vec<(String, String)> {
            vec![
                (
                    "authorization".to_string(),
                    format!("Bearer {}", self.access_token),
                ),
                (
                    "chatgpt-account-id".to_string(),
                    self.cfg.account_id.clone(),
                ),
                ("originator".to_string(), ORIGINATOR.to_string()),
                (
                    "user-agent".to_string(),
                    format!("{ORIGINATOR}/{}", self.cfg.cli_version),
                ),
                // NB: do NOT set content-type here — post_json's `.json()` already sets it
                // once; a duplicate content-type header makes the ChatGPT backend 400.
                ("accept".to_string(), "text/event-stream".to_string()),
            ]
        }
    }

    impl LlmProvider for CodexProvider {
        fn name(&self) -> &str {
            "codex"
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
            let assistant_base = *self.history_assistant_base.get_or_insert_with(|| {
                history
                    .iter()
                    .filter(|message| message.role == Role::Assistant)
                    .count()
            });
            // #639: attest THIS round's request, then send only the attested object.
            let attested = crate::provider::build_attested_provider_request(
                crate::provider::ProviderRequestBinding::Codex {
                    access_token: &self.access_token,
                    account_id: &self.cfg.account_id,
                    model: &self.cfg.model,
                    reasoning_effort: self.cfg.reasoning_effort,
                    instructions: &self.instructions,
                },
                self.cfg.responses_url.clone(),
                self.request_headers(),
                {
                    let mut body = build_request(
                        &self.cfg.model,
                        self.cfg.reasoning_effort,
                        &self.instructions,
                        history,
                    );
                    body["input"] = Value::Array(build_input_with_reasoning(
                        history,
                        &self.reasoning_rounds,
                        assistant_base,
                    ));
                    body
                },
            )?;
            let mut text = String::new();
            let mut tools: Vec<CodexToolArgs> = Vec::new();
            let mut reasoning = Vec::new();
            let mut usage = Usage::default();
            let mut failure: Option<String> = None;
            let mut parse_error: Option<AgentError> = None;
            let response = self.http.post_attested_sse_cancellable(
                &attested,
                &mut |event| {
                    if parse_error.is_some() {
                        return false;
                    }
                    let advances_turn = sse_event_advances_turn(&event.data);
                    match apply_sse_event(
                        &event.data,
                        &mut text,
                        &mut tools,
                        &mut reasoning,
                        &mut usage,
                        &mut failure,
                    ) {
                        Ok(Some(delta)) => emit(StreamEvent::Token(delta)),
                        Ok(None) => {}
                        Err(e) => parse_error = Some(e),
                    }
                    advances_turn
                },
                cancellation,
            )?;
            if response.status == 401 || response.status == 403 {
                return Err(closed_provider_failure("authorization_rejected"));
            }
            if response.status >= 400 {
                let detail = response.body_preview.unwrap_or_default();
                return Err(closed_provider_failure(closed_backend_failure_code(
                    response.status,
                    &detail,
                )));
            }
            if parse_error.is_some() {
                return Err(closed_provider_failure("parse_failure"));
            }
            let (blocks, usage) = finish_sse_blocks(text, tools, usage, failure)
                .map_err(|_| closed_provider_failure("stream_failure"))?;
            self.reasoning_rounds.push(reasoning);
            let usage = usage.with_provider_response("codex", &self.cfg.model, &response.headers);
            self.last_usage = usage;
            Ok(blocks)
        }

        fn last_usage(&self) -> Option<Usage> {
            (!self.last_usage.is_empty()).then(|| self.last_usage.clone())
        }
    }
}

#[cfg(feature = "http")]
pub use live::CodexProvider;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::ToolUseRef;

    #[test]
    fn default_config_is_the_codex_recipe() {
        let c = CodexConfig::default();
        assert_eq!(
            c.responses_url,
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(c.model, "gpt-5.6-sol");
        assert_eq!(c.reasoning_effort, CodexReasoningEffort::Medium);
        assert_eq!(c.cli_version, "0.144.4");
    }

    #[test]
    fn build_request_uses_responses_shape_with_instructions_and_tool() {
        let h = vec![Message::user("find the spotify invoice")];
        let body = build_request("gpt-5.6-terra", CodexReasoningEffort::High, "be terse", &h);
        assert_eq!(body["model"], "gpt-5.6-terra");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["instructions"], "be terse");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "isyncyou");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
    }

    #[cfg(feature = "http")]
    #[test]
    fn codex_subscription_identity_envelope_remains_unchanged() {
        let p = CodexProvider::new(
            "tok123",
            "instructions",
            CodexConfig {
                account_id: "acct_123".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let h = p.request_headers();
        let get = |k: &str| h.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());

        assert_eq!(
            h.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            vec![
                "authorization",
                "chatgpt-account-id",
                "originator",
                "user-agent",
                "accept",
            ]
        );

        assert_eq!(get("authorization").unwrap(), "Bearer tok123");
        assert_eq!(get("chatgpt-account-id").unwrap(), "acct_123");
        assert_eq!(get("originator").unwrap(), "codex_cli_rs");
        assert_eq!(get("user-agent").unwrap(), "codex_cli_rs/0.144.4");
        assert_eq!(get("accept").unwrap(), "text/event-stream");
        assert!(get("content-type").is_none());
    }

    #[test]
    fn build_input_round_trips_tool_call_and_result() {
        let h = vec![
            Message::user("q"),
            Message::assistant(
                "",
                vec![ToolUseRef {
                    id: "call_1".into(),
                    input: json!({"op":"search","account":"me","query":"x"}),
                }],
            ),
            Message::tool("call_1", "hit: item-42"),
        ];
        let input = build_input(&h);
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["output"], "hit: item-42");
    }

    #[test]
    fn build_input_replays_reasoning_before_matching_tool_round() {
        let h = vec![
            Message::assistant("older visible context", Vec::new()),
            Message::user("q"),
            Message::assistant(
                "",
                vec![ToolUseRef {
                    id: "call_1".into(),
                    input: json!({"op":"search","account":"me","query":"x"}),
                }],
            ),
            Message::tool("call_1", "hit: item-42"),
        ];
        let reasoning = replayable_reasoning_context(&json!({
            "type": "reasoning",
            "id": "provider-item-id",
            "summary": [{"type":"summary_text","text":"private summary"}],
            "content": [{"type":"reasoning_text","text":"private plaintext"}],
            "encrypted_content": "opaque-context",
        }))
        .unwrap();

        let input = build_input_with_reasoning(&h, &[vec![reasoning]], 1);
        let reasoning_index = input
            .iter()
            .position(|item| item["type"] == "reasoning")
            .unwrap();
        let call_index = input
            .iter()
            .position(|item| item["type"] == "function_call")
            .unwrap();
        let output_index = input
            .iter()
            .position(|item| item["type"] == "function_call_output")
            .unwrap();

        assert!(reasoning_index < call_index && call_index < output_index);
        assert_eq!(
            input[reasoning_index]["encrypted_content"],
            "opaque-context"
        );
        assert!(input[reasoning_index].get("id").is_none());
        assert!(input[reasoning_index].get("content").is_none());
        assert!(!serde_json::to_string(&input)
            .unwrap()
            .contains("private plaintext"));
    }

    #[test]
    fn codex_reasoning_context_is_bounded_and_requires_encrypted_content() {
        assert!(replayable_reasoning_context(&json!({
            "type": "reasoning",
            "summary": [],
        }))
        .is_err());
        assert!(replayable_reasoning_context(&json!({
            "type": "reasoning",
            "summary": [],
            "encrypted_content": "x".repeat(MAX_REASONING_CONTEXT_BYTES + 1),
        }))
        .is_err());
    }

    #[cfg(feature = "http")]
    #[test]
    fn codex_backend_failures_reduce_to_closed_diagnostics() {
        assert_eq!(
            closed_backend_failure_code(400, "reasoning item is required"),
            "reasoning_context"
        );
        assert_eq!(
            closed_backend_failure_code(400, "function_call pairing failed"),
            "function_call_pairing"
        );
        assert_eq!(
            closed_backend_failure_code(400, "unrecognized private provider detail"),
            "http_bad_request"
        );
        assert!(!closed_backend_failure_code(400, "secret-value").contains("secret-value"));
    }

    #[test]
    fn parse_sse_collects_text_and_usage() {
        let sse = "\
event: response.output_text.delta\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\
\n\
event: response.output_text.delta\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\" codex\"}\n\
\n\
event: response.completed\n\
data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":22,\"output_tokens\":8}}}\n\
\n";
        let (blocks, usage) = parse_sse(sse).unwrap();
        assert!(matches!(&blocks[0], AssistantBlock::Text(t) if t == "hello codex"));
        assert_eq!(usage.input_tokens, 22);
        assert_eq!(usage.output_tokens, 8);
    }

    #[test]
    fn apply_sse_event_returns_text_delta_immediately() {
        let mut text = String::new();
        let mut tools = Vec::new();
        let mut reasoning = Vec::new();
        let mut usage = Usage::default();
        let mut failure = None;

        let delta = apply_sse_event(
            "{\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}",
            &mut text,
            &mut tools,
            &mut reasoning,
            &mut usage,
            &mut failure,
        )
        .unwrap();

        assert_eq!(delta.as_deref(), Some("hello"));
        assert_eq!(text, "hello");
        assert!(tools.is_empty());
        assert!(reasoning.is_empty());
        assert_eq!(usage, Usage::default());
        assert!(failure.is_none());
    }

    #[test]
    fn apply_sse_event_captures_replayable_reasoning_without_plaintext() {
        let mut text = String::new();
        let mut tools = Vec::new();
        let mut reasoning = Vec::new();
        let mut usage = Usage::default();
        let mut failure = None;

        apply_sse_event(
            r#"{"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","summary":[],"content":[{"type":"reasoning_text","text":"do not retain"}],"encrypted_content":"opaque"}}"#,
            &mut text,
            &mut tools,
            &mut reasoning,
            &mut usage,
            &mut failure,
        )
        .unwrap();

        assert_eq!(reasoning.len(), 1);
        let replay = reasoning[0].replay_value();
        assert_eq!(replay["encrypted_content"], "opaque");
        assert!(replay.get("id").is_none());
        assert!(replay.get("content").is_none());
    }

    #[test]
    fn codex_status_events_do_not_satisfy_response_or_extend_idle_deadline() {
        assert!(!sse_event_advances_turn(
            r#"{"type":"response.in_progress"}"#
        ));
        assert!(sse_event_advances_turn(
            r#"{"type":"response.output_text.delta","delta":"ok"}"#
        ));
        assert!(sse_event_advances_turn(
            r#"{"type":"response.completed","response":{}}"#
        ));
    }

    #[test]
    fn parse_sse_extracts_function_call() {
        let sse = "\
data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_9\",\"name\":\"isyncyou\",\"arguments\":\"{\\\"op\\\":\\\"search\\\"}\"}}\n\
\n\
data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n";
        let (blocks, _u) = parse_sse(sse).unwrap();
        match &blocks[0] {
            AssistantBlock::ToolUse { id, input } => {
                assert_eq!(id, "call_9");
                assert_eq!(input["op"], "search");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_sse_surfaces_failure() {
        let sse = "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"nope\"}}}\n";
        assert!(parse_sse(sse).unwrap_err().to_string().contains("codex"));
    }
}
