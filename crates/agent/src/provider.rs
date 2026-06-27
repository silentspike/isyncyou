//! Provider abstraction + streamed events. The turn loop drives any [`LlmProvider`];
//! [`FakeProvider`] is the deterministic CI provider (no real LLM tokens).

use crate::tool::ToolAction;

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
    /// A destructive action is awaiting human confirmation (REQ-AGENT-002). The turn
    /// stops here; the model never receives a capability token (REQ-AGENT-004).
    ConfirmationRequired {
        id: String,
        action: ToolAction,
        preview: String,
    },
    /// A non-fatal error message for the stream.
    Error(String),
    /// The turn finished.
    Done,
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
}

pub mod fake;
pub use fake::FakeProvider;
