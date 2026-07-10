//! Provider abstraction + streamed events. The turn loop drives any [`LlmProvider`];
//! [`FakeProvider`] is the deterministic CI provider (no real LLM tokens).

use crate::tool::ToolAction;
use std::collections::BTreeMap;

/// Why a turn stream reached its terminal `done` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoneReason {
    Complete,
    PendingConfirmation,
    Cancelled,
    Error,
}

impl DoneReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::PendingConfirmation => "pending_confirmation",
            Self::Cancelled => "cancelled",
            Self::Error => "error",
        }
    }
}

/// One streamed event produced while a turn runs. This is the typed event set the
/// `AgentStreamHub` will carry to the UI (REQ-AGENT-007); here it is emitted via a sink.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A chunk of assistant text.
    Token(String),
    /// The model invoked the `isyncyou` tool.
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// A tool produced a result. `untrusted` is true for results carrying archived
    /// content (mail/document bodies) — the prompt-injection boundary (REQ-AGENT-005).
    ToolResult {
        id: String,
        content: String,
        untrusted: bool,
    },
    /// A progressive-search stage boundary (S-AG.18/#643): `stage` is "names" (fast
    /// subject match), "bodies" (full-text), or "deep"; `status` is "running" | "done";
    /// `hits` is the running deduped total. Lets the UI show a per-stage checkmark.
    SearchStage {
        stage: String,
        status: String,
        hits: usize,
    },
    /// Items a search stage added (deduped against earlier stages), streamed so the UI
    /// can grow the result list before the turn's final answer. Each item is
    /// source-tagged (`{service, id, name, item_type, path}`).
    PartialResult {
        stage: String,
        items: serde_json::Value,
    },
    /// A destructive action is awaiting human confirmation (REQ-AGENT-002). The turn
    /// stops here; the model never receives a capability token (REQ-AGENT-004).
    ConfirmationRequired {
        id: String,
        action: Box<ToolAction>,
        preview: String,
        action_hash: String,
        risk: String,
        expires_at_ms: u64,
        token: String,
    },
    /// A non-fatal error message for the stream.
    Error(String),
    /// The turn finished.
    Done { reason: DoneReason },
}

impl StreamEvent {
    pub fn done(reason: DoneReason) -> Self {
        Self::Done { reason }
    }

    pub fn event_name(&self) -> &'static str {
        match self {
            Self::Token(_) => "token",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolResult { .. } => "tool_result",
            Self::SearchStage { .. } => "search_stage",
            Self::PartialResult { .. } => "partial_result",
            Self::ConfirmationRequired { .. } => "confirmation_required",
            Self::Error(_) => "error",
            Self::Done { .. } => "done",
        }
    }

    /// Serialize the public stream event shape once, in the agent core, so SSE and
    /// bridge transports cannot drift. This is a UI data signal; it deliberately omits
    /// the raw destructive action until Task 2 registers a canonical PendingAction.
    pub fn to_public_json(&self) -> serde_json::Value {
        match self {
            Self::Token(t) => serde_json::json!({ "event": "token", "text": t }),
            Self::ToolCall { id, name, input } => {
                serde_json::json!({ "event": "tool_call", "id": id, "name": name, "input": input })
            }
            Self::ToolResult {
                id,
                content,
                untrusted,
            } => serde_json::json!({
                "event": "tool_result", "id": id, "content": content, "untrusted": untrusted
            }),
            Self::ConfirmationRequired {
                id,
                preview,
                action_hash,
                risk,
                expires_at_ms,
                token,
                ..
            } => serde_json::json!({
                "event": "confirmation_required",
                "pending_id": id,
                "tool_id": id,
                "preview": preview,
                "action_hash": action_hash,
                "risk": risk,
                "expires_at_ms": expires_at_ms,
                "token": token
            }),
            Self::SearchStage {
                stage,
                status,
                hits,
            } => serde_json::json!({
                "event": "search_stage", "stage": stage, "status": status, "hits": hits
            }),
            Self::PartialResult { stage, items } => {
                serde_json::json!({ "event": "partial_result", "stage": stage, "items": items })
            }
            Self::Error(e) => serde_json::json!({ "event": "error", "message": e }),
            Self::Done { reason } => {
                serde_json::json!({ "event": "done", "reason": reason.as_str() })
            }
        }
    }

    pub fn to_public_json_string(&self) -> String {
        self.to_public_json().to_string()
    }
}

/// One block of a single assistant response: either text, or a tool invocation.
#[derive(Debug, Clone)]
pub enum AssistantBlock {
    Text(String),
    /// `input` is the raw JSON for the `isyncyou` tool; the loop parses it into a
    /// typed [`ToolAction`].
    ToolUse {
        id: String,
        input: serde_json::Value,
    },
}

/// A language-model provider. Given the conversation so far, produce the next assistant
/// message (text + optional tool calls), streaming tokens via `emit`.
pub trait LlmProvider {
    /// Short provider name (e.g. `"fake"`, `"anthropic"`, `"openai"`).
    fn name(&self) -> &str;

    /// Produce the next assistant message. Implementations stream text via `emit` and
    /// return the structured blocks so the loop can act on any tool calls.
    fn next(
        &mut self,
        history: &[crate::turn::Message],
        emit: &mut dyn FnMut(StreamEvent),
    ) -> Result<Vec<AssistantBlock>, crate::AgentError>;

    fn last_usage(&self) -> Option<Usage> {
        None
    }
}

/// Token usage reported by a provider (surfaced to the UI's usage chip).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub provider: String,
    pub model: String,
    pub request_id: Option<String>,
    pub rate_limit: BTreeMap<String, String>,
}

impl Usage {
    pub fn with_provider_response(
        mut self,
        provider: &str,
        model: &str,
        headers: &BTreeMap<String, String>,
    ) -> Self {
        self.provider = provider.to_string();
        self.model = model.to_string();
        self.request_id = headers
            .get("x-request-id")
            .or_else(|| headers.get("request-id"))
            .cloned();
        self.rate_limit = headers
            .iter()
            .filter(|(k, _)| k.contains("ratelimit") || k.as_str() == "retry-after")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        self
    }

    pub fn is_empty(&self) -> bool {
        self.input_tokens == 0
            && self.output_tokens == 0
            && self.provider.is_empty()
            && self.model.is_empty()
            && self.request_id.is_none()
            && self.rate_limit.is_empty()
    }

    pub fn to_public_json(&self) -> serde_json::Value {
        serde_json::json!({
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "provider": self.provider,
            "model": self.model,
            "request_id": self.request_id,
            "rate_limit": self.rate_limit,
        })
    }
}

// Shared request/parse helpers are unit-tested without live provider features. The legacy
// BYO API-key live providers are kept behind `byo-api-providers`; #623 product OAuth uses
// `subscription`/`codex` instead.
#[cfg(any(feature = "http", test))]
pub mod anthropic;
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
pub mod codex;
pub mod fake;
#[cfg(any(feature = "byo-api-providers", test))]
pub mod openai;
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
pub mod subscription;
pub use fake::FakeProvider;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_stream_event_json_is_single_line_and_stable() {
        let events = [
            StreamEvent::Token("hello".into()),
            StreamEvent::ToolCall {
                id: "t1".into(),
                name: "isyncyou".into(),
                input: json!({"op": "search"}),
            },
            StreamEvent::ToolResult {
                id: "t1".into(),
                content: "{}".into(),
                untrusted: true,
            },
            StreamEvent::ConfirmationRequired {
                id: "pending-1".into(),
                action: Box::new(ToolAction::Backup {
                    account: "me".into(),
                    services: vec!["mail".into()],
                }),
                preview: "Requires confirmation".into(),
                action_hash: "a".repeat(64),
                risk: "destructive".into(),
                expires_at_ms: 60_000,
                token: "confirm-token".into(),
            },
            StreamEvent::Error("redacted".into()),
            StreamEvent::done(DoneReason::Cancelled),
        ];
        let names: Vec<_> = events.iter().map(StreamEvent::event_name).collect();
        assert!(names.contains(&"token"));
        assert!(names.contains(&"tool_call"));
        assert!(names.contains(&"tool_result"));
        assert!(names.contains(&"confirmation_required"));
        assert!(names.contains(&"error"));
        assert!(names.contains(&"done"));
        for event in events {
            let line = event.to_public_json_string();
            assert!(
                !line.contains('\n'),
                "event JSON must be single-line: {line}"
            );
            let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(parsed["event"], event.event_name());
            if event.event_name() == "done" {
                assert_eq!(parsed["reason"], "cancelled");
            }
        }
    }

    #[test]
    fn usage_public_json_keeps_only_provider_metadata() {
        let headers = BTreeMap::from([
            ("x-request-id".to_string(), "req-123".to_string()),
            (
                "x-ratelimit-remaining-requests".to_string(),
                "12".to_string(),
            ),
            ("retry-after".to_string(), "7".to_string()),
            ("authorization".to_string(), "Bearer secret".to_string()),
            ("chatgpt-account-id".to_string(), "acct-secret".to_string()),
        ]);
        let usage = Usage {
            input_tokens: 10,
            output_tokens: 3,
            ..Default::default()
        }
        .with_provider_response("codex", "gpt-5.5", &headers);
        let public = usage.to_public_json();

        assert_eq!(public["provider"], "codex");
        assert_eq!(public["model"], "gpt-5.5");
        assert_eq!(public["request_id"], "req-123");
        assert_eq!(public["rate_limit"]["x-ratelimit-remaining-requests"], "12");
        assert_eq!(public["rate_limit"]["retry-after"], "7");
        assert!(!public.to_string().contains("secret"));
    }
}
