//! Anthropic Messages request/response helpers plus the legacy BYO API-key provider.
//! The shared request building + response parsing are used by the Claude OAuth-compatible
//! runtime; the `AnthropicProvider` API-key live type is quarantined behind
//! `byo-api-providers`.

use super::{AssistantBlock, Usage};
use crate::tool::{tool_schema, TOOL_NAME};
use crate::turn::{Message, Role};
use crate::AgentError;
use serde_json::{json, Value};

#[cfg(feature = "byo-api-providers")]
const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
#[cfg(feature = "byo-api-providers")]
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Map the agent history to Anthropic's `messages` array, pairing tool_use ↔ tool_result.
pub(crate) fn build_messages(history: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in history {
        match m.role {
            Role::User => out.push(json!({ "role": "user", "content": m.content })),
            Role::Assistant => {
                let mut content = Vec::new();
                if !m.content.is_empty() {
                    content.push(json!({ "type": "text", "text": m.content }));
                }
                for tu in &m.tool_uses {
                    content.push(json!({
                        "type": "tool_use", "id": tu.id, "name": TOOL_NAME, "input": tu.input
                    }));
                }
                out.push(json!({ "role": "assistant", "content": content }));
            }
            Role::Tool => out.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": m.tool_use_id.clone().unwrap_or_default(),
                    "content": m.content,
                }],
            })),
        }
    }
    out
}

/// Build the full Messages request body. `tool_schema()` already has Anthropic's
/// `{name, description, input_schema}` shape.
#[cfg(any(feature = "byo-api-providers", test))]
pub(crate) fn build_request(model: &str, system: &str, history: &[Message]) -> Value {
    build_request_blocks(model, json!(system), history)
}

/// Like [`build_request`] but takes the `system` field as an arbitrary value — a plain
/// string for the official provider, or a multi-block array (e.g. a leading billing
/// block) for the experimental subscription provider. Keeps the messages/tools shaping
/// in one place so both providers stay wire-identical except for `system`.
pub(crate) fn build_request_blocks(model: &str, system: Value, history: &[Message]) -> Value {
    json!({
        "model": model,
        "max_tokens": DEFAULT_MAX_TOKENS,
        "system": system,
        "tools": [tool_schema()],
        "messages": build_messages(history),
    })
}

#[cfg(feature = "byo-api-providers")]
fn headers(api_key: &str) -> Vec<(String, String)> {
    vec![
        ("x-api-key".to_string(), api_key.to_string()),
        (
            "anthropic-version".to_string(),
            ANTHROPIC_VERSION.to_string(),
        ),
        ("content-type".to_string(), "application/json".to_string()),
    ]
}

/// Parse a Messages response into assistant blocks + usage. Surfaces API errors clearly
/// without leaking the key.
pub(crate) fn parse_response(v: &Value) -> Result<(Vec<AssistantBlock>, Usage), AgentError> {
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("anthropic error");
        return Err(AgentError::Provider(format!("anthropic: {msg}")));
    }
    let content = v
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or_else(|| AgentError::Provider("anthropic: response had no content array".into()))?;
    let mut blocks = Vec::new();
    for item in content {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                    blocks.push(AssistantBlock::Text(t.to_string()));
                }
            }
            Some("tool_use") => blocks.push(AssistantBlock::ToolUse {
                id: item
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or_default()
                    .to_string(),
                input: item.get("input").cloned().unwrap_or(Value::Null),
            }),
            _ => {}
        }
    }
    let usage = v.get("usage");
    let g = |k: &str| {
        usage
            .and_then(|u| u.get(k))
            .and_then(|x| x.as_u64())
            .unwrap_or(0)
    };
    Ok((
        blocks,
        Usage {
            input_tokens: g("input_tokens"),
            output_tokens: g("output_tokens"),
        },
    ))
}

#[cfg(feature = "byo-api-providers")]
mod live {
    use super::*;
    use crate::provider::{LlmProvider, StreamEvent};

    /// Live Anthropic provider over the agent's own blocking HTTP transport.
    pub struct AnthropicProvider {
        http: crate::http::HttpTransport,
        api_key: String,
        model: String,
        system: String,
        pub last_usage: Usage,
    }

    impl AnthropicProvider {
        pub fn new(
            api_key: impl Into<String>,
            model: impl Into<String>,
            system: impl Into<String>,
        ) -> Result<Self, AgentError> {
            Ok(Self {
                http: crate::http::HttpTransport::new()?,
                api_key: api_key.into(),
                model: model.into(),
                system: system.into(),
                last_usage: Usage::default(),
            })
        }
    }

    impl LlmProvider for AnthropicProvider {
        fn name(&self) -> &str {
            "anthropic"
        }

        fn next(
            &mut self,
            history: &[Message],
            emit: &mut dyn FnMut(StreamEvent),
        ) -> Result<Vec<AssistantBlock>, AgentError> {
            let body = build_request(&self.model, &self.system, history);
            let (status, text) =
                self.http
                    .post_json(ANTHROPIC_URL, &headers(&self.api_key), &body)?;
            if status == 401 || status == 403 {
                return Err(AgentError::Provider(
                    "anthropic: unauthorized (check the API key)".into(),
                ));
            }
            let v: Value = serde_json::from_str(&text)
                .map_err(|e| AgentError::Provider(format!("anthropic: invalid JSON: {e}")))?;
            let (blocks, usage) = parse_response(&v)?;
            self.last_usage = usage;
            // Non-streaming call: emit text as tokens so the UI still streams live.
            for b in &blocks {
                if let AssistantBlock::Text(t) = b {
                    emit(StreamEvent::Token(t.clone()));
                }
            }
            Ok(blocks)
        }
    }
}

#[cfg(feature = "byo-api-providers")]
pub use live::AnthropicProvider;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::ToolUseRef;

    #[test]
    fn build_request_advertises_the_isyncyou_tool_and_system() {
        let history = vec![Message::user("find the spotify invoice")];
        let body = build_request("claude-test", "you are helpful", &history);
        assert_eq!(body["model"], "claude-test");
        assert_eq!(body["system"], "you are helpful");
        assert_eq!(body["tools"][0]["name"], "isyncyou");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "find the spotify invoice");
    }

    #[test]
    fn build_messages_round_trips_tool_use_and_result() {
        let history = vec![
            Message::user("q"),
            Message::assistant(
                "",
                vec![ToolUseRef {
                    id: "t1".into(),
                    input: json!({"op":"search","account":"me","query":"x"}),
                }],
            ),
            Message::tool("t1", "hit: item-42"),
        ];
        let msgs = build_messages(&history);
        // assistant turn carries a tool_use block with id t1
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][0]["id"], "t1");
        assert_eq!(msgs[1]["content"][0]["name"], "isyncyou");
        // tool result is a user message bound by tool_use_id
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "t1");
    }

    #[test]
    fn parse_response_extracts_text_and_tool_use_and_usage() {
        let v = json!({
            "content": [
                {"type":"text","text":"Looking..."},
                {"type":"tool_use","id":"t9","name":"isyncyou","input":{"op":"search","account":"me","query":"spotify"}}
            ],
            "usage": {"input_tokens": 12, "output_tokens": 34}
        });
        let (blocks, usage) = parse_response(&v).unwrap();
        assert!(matches!(&blocks[0], AssistantBlock::Text(t) if t == "Looking..."));
        match &blocks[1] {
            AssistantBlock::ToolUse { id, input } => {
                assert_eq!(id, "t9");
                assert_eq!(input["op"], "search");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            usage,
            Usage {
                input_tokens: 12,
                output_tokens: 34
            }
        );
    }

    #[test]
    fn parse_response_surfaces_api_errors_without_leaking() {
        let v = json!({"error": {"type":"authentication_error","message":"invalid x-api-key"}});
        let err = parse_response(&v).unwrap_err();
        assert!(err.to_string().contains("anthropic"));
    }
}
