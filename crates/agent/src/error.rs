use thiserror::Error;

/// Errors raised by the agent harness.
#[derive(Debug, Error)]
pub enum AgentError {
    /// The caller cancelled the turn. No later provider/tool result may be committed.
    #[error("turn cancelled")]
    Cancelled,
    /// A model tool call could not be parsed into a typed [`crate::ToolAction`].
    #[error("tool argument error: {0}")]
    ToolArgs(String),
    /// The provider failed to produce a usable response.
    #[error("provider error: {0}")]
    Provider(String),
    /// The HTTP transport failed (only reachable with the `http` feature).
    #[error("transport error: {0}")]
    Transport(String),
}
