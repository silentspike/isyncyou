//! OpenAI Chat Completions request/response helpers plus the legacy BYO API-key provider.
//! The BYO live type is quarantined behind `byo-api-providers` and is not part of the
//! #623 Claude/Codex product OAuth path.

use super::{AssistantBlock, Usage};
use crate::tool::{tool_schema, TOOL_NAME};
use crate::turn::{Message, Role};
use crate::AgentError;
use serde_json::{json, Value};
#[cfg(feature = "byo-api-providers")]
use std::sync::Arc;

#[cfg(feature = "byo-api-providers")]
const OPENAI_URL: &str = "https://api.openai.com/v1/chat/completions";

/// The single isyncyou tool in OpenAI's `{type:function, function:{...}}` shape.
fn openai_tool() -> Value {
    let s = tool_schema();
    json!({
        "type": "function",
        "function": {
            "name": s["name"],
            "description": s["description"],
            "parameters": s["input_schema"],
        }
    })
}

/// Map the agent history to OpenAI chat `messages`, pairing tool_calls ↔ tool messages.
pub(crate) fn build_messages(system: &str, history: &[Message]) -> Vec<Value> {
    let mut out = vec![json!({ "role": "system", "content": system })];
    for m in history {
        match m.role {
            Role::User => out.push(json!({ "role": "user", "content": m.content })),
            Role::Assistant => {
                let mut msg = json!({ "role": "assistant" });
                // content is null when the turn is purely tool calls.
                msg["content"] = if m.content.is_empty() {
                    Value::Null
                } else {
                    json!(m.content)
                };
                if !m.tool_uses.is_empty() {
                    let calls: Vec<Value> = m
                        .tool_uses
                        .iter()
                        .map(|tu| {
                            json!({
                                "id": tu.id,
                                "type": "function",
                                "function": {
                                    "name": TOOL_NAME,
                                    // OpenAI arguments is a JSON *string*.
                                    "arguments": tu.input.to_string(),
                                }
                            })
                        })
                        .collect();
                    msg["tool_calls"] = json!(calls);
                }
                out.push(msg);
            }
            Role::Tool => out.push(json!({
                "role": "tool",
                "tool_call_id": m.tool_use_id.clone().unwrap_or_default(),
                "content": m.content,
            })),
        }
    }
    out
}

/// Build the Chat Completions request body. `store` is false unless `store` is set true.
pub(crate) fn build_request(model: &str, system: &str, history: &[Message], store: bool) -> Value {
    json!({
        "model": model,
        "store": store,
        "tools": [openai_tool()],
        "tool_choice": "auto",
        "messages": build_messages(system, history),
    })
}

#[cfg(feature = "byo-api-providers")]
fn headers(api_key: &str) -> Vec<(String, String)> {
    vec![
        ("authorization".to_string(), format!("Bearer {api_key}")),
        ("content-type".to_string(), "application/json".to_string()),
    ]
}

/// Parse a Chat Completions response into assistant blocks + usage.
pub(crate) fn parse_response(v: &Value) -> Result<(Vec<AssistantBlock>, Usage), AgentError> {
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("openai error");
        return Err(AgentError::Provider(format!("openai: {msg}")));
    }
    let message = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"))
        .ok_or_else(|| AgentError::Provider("openai: response had no choices[0].message".into()))?;
    let mut blocks = Vec::new();
    if let Some(t) = message.get("content").and_then(|c| c.as_str()) {
        if !t.is_empty() {
            blocks.push(AssistantBlock::Text(t.to_string()));
        }
    }
    if let Some(calls) = message.get("tool_calls").and_then(|c| c.as_array()) {
        for call in calls {
            let id = call
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or_default()
                .to_string();
            // arguments is a JSON string — parse it back to a value.
            let args = call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .unwrap_or("{}");
            let input: Value = serde_json::from_str(args).unwrap_or(Value::Null);
            blocks.push(AssistantBlock::ToolUse { id, input });
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
            input_tokens: g("prompt_tokens"),
            output_tokens: g("completion_tokens"),
            ..Default::default()
        },
    ))
}

#[cfg(feature = "byo-api-providers")]
mod live {
    use super::*;
    use crate::provider::{LlmProvider, StreamEvent};

    /// Live OpenAI provider over the agent's own blocking HTTP transport.
    pub struct OpenAiProvider {
        http: Arc<crate::http::HttpTransport>,
        api_key: String,
        model: String,
        system: String,
        /// Retain content on OpenAI's side. Defaults to false for M365 content.
        pub store: bool,
        pub last_usage: Usage,
    }

    impl OpenAiProvider {
        pub fn new(
            api_key: impl Into<String>,
            model: impl Into<String>,
            system: impl Into<String>,
        ) -> Result<Self, AgentError> {
            Ok(Self {
                http: crate::http::HttpTransport::shared()?,
                api_key: api_key.into(),
                model: model.into(),
                system: system.into(),
                store: false,
                last_usage: Usage::default(),
            })
        }
    }

    impl LlmProvider for OpenAiProvider {
        fn name(&self) -> &str {
            "openai"
        }

        fn next(
            &mut self,
            history: &[Message],
            emit: &mut dyn FnMut(StreamEvent),
        ) -> Result<Vec<AssistantBlock>, AgentError> {
            let body = build_request(&self.model, &self.system, history, self.store);
            let (status, text) = self
                .http
                .post_json(OPENAI_URL, &headers(&self.api_key), &body)?;
            if status == 401 || status == 403 {
                return Err(AgentError::Provider(
                    "openai: unauthorized (check the API key)".into(),
                ));
            }
            let v: Value = serde_json::from_str(&text)
                .map_err(|e| AgentError::Provider(format!("openai: invalid JSON: {e}")))?;
            let (blocks, usage) = parse_response(&v)?;
            self.last_usage = usage;
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
pub use live::OpenAiProvider;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::ToolUseRef;

    #[test]
    fn build_request_sets_store_false_by_default_and_advertises_the_tool() {
        let history = vec![Message::user("hi")];
        let body = build_request("gpt-test", "sys", &history, false);
        assert_eq!(body["store"], false);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "isyncyou");
        // system is the first message, then the user turn
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "hi");
    }

    #[test]
    fn build_messages_round_trips_tool_calls_with_string_arguments() {
        let history = vec![
            Message::user("q"),
            Message::assistant(
                "",
                vec![ToolUseRef {
                    id: "c1".into(),
                    input: json!({"op":"search","account":"me","query":"x"}),
                }],
            ),
            Message::tool("c1", "hit: item-42"),
        ];
        let msgs = build_messages("sys", &history);
        // assistant tool call: arguments is a JSON *string*
        let call = &msgs[2]["tool_calls"][0];
        assert_eq!(call["id"], "c1");
        assert_eq!(call["type"], "function");
        assert!(call["function"]["arguments"]
            .as_str()
            .unwrap()
            .contains("\"op\":\"search\""));
        // tool result bound by tool_call_id
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "c1");
    }

    #[test]
    fn parse_response_extracts_text_tool_calls_and_usage() {
        let v = json!({
            "choices": [{
                "message": {
                    "content": "Looking...",
                    "tool_calls": [{
                        "id": "c9",
                        "type": "function",
                        "function": {"name": "isyncyou", "arguments": "{\"op\":\"search\",\"account\":\"me\",\"query\":\"spotify\"}"}
                    }]
                }
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 7}
        });
        let (blocks, usage) = parse_response(&v).unwrap();
        assert!(matches!(&blocks[0], AssistantBlock::Text(t) if t == "Looking..."));
        match &blocks[1] {
            AssistantBlock::ToolUse { id, input } => {
                assert_eq!(id, "c9");
                assert_eq!(input["query"], "spotify"); // arguments string parsed back
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            usage,
            Usage {
                input_tokens: 5,
                output_tokens: 7,
                ..Default::default()
            }
        );
    }

    #[test]
    fn parse_response_surfaces_api_errors() {
        let v = json!({"error": {"message": "Incorrect API key provided"}});
        assert!(parse_response(&v)
            .unwrap_err()
            .to_string()
            .contains("openai"));
    }
}
