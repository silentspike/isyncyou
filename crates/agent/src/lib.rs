//! In-app M365 agent harness (Epic #614, story S-AG.2 / #617).
//!
//! This crate is the provider-agnostic core of the in-app assistant. It deliberately
//! holds **no** Microsoft Graph concerns (that is `isyncyou-graph`) and **does not**
//! reuse `GraphClient` for LLM calls — it has its own [`http::HttpTransport`].
//!
//! # App-scope invariant (the safety model, REQ-AGENT-001)
//! The agent is exposed exactly **one** tool, [`tool::TOOL_NAME`] (`isyncyou`), whose
//! subcommands act only on the user's M365 domain. There is no shell, arbitrary
//! filesystem, OS, device, or free-form-HTTP tool. [`tool::registry_tool_names`]
//! returns that single name and a snapshot test asserts it, so "full power" can never
//! reach the host system.
//!
//! # Tool policy (REQ-AGENT-002)
//! Read-class actions execute immediately; destructive-class actions never execute in
//! the loop — they stop the turn with [`turn::TurnOutcome::PendingConfirmation`] for a
//! human to confirm out of band (the model never receives a capability token,
//! REQ-AGENT-004).
//!
//! # Providers
//! [`provider::FakeProvider`] is a deterministic, scripted provider — the only provider
//! used in CI (no real LLM tokens, REQ-AGENT-008). Official + experimental providers
//! implement [`provider::LlmProvider`] in later stories.

pub mod archive;
mod error;
pub mod http;
pub mod provider;
pub mod retrieval;
pub mod tool;
pub mod turn;

pub use archive::{ArchiveSource, ItemRef};
pub use error::AgentError;
pub use provider::{AssistantBlock, FakeProvider, LlmProvider, StreamEvent};
pub use retrieval::RetrievalExecutor;
pub use tool::{
    help_text, parse_action, registry_tool_names, tool_schema, ToolAction, ToolClass, TOOL_NAME,
};
pub use turn::{run_turn, Message, Role, ToolExecutor, TurnOutcome};

#[cfg(feature = "retrieval")]
pub use archive::StoreArchive;
