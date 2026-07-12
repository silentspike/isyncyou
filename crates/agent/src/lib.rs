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
//! [`provider::FakeProvider`] is a deterministic, scripted provider for isolated tests.
//! The product Claude/Codex OAuth provider runtime is compiled by
//! `agent-oauth-providers`; #627's local CLI fallback/capture surface remains behind
//! `agent-subscription-experimental`.

pub mod archive;
pub mod confirm;
pub mod connectivity;
#[cfg(feature = "agent-subscription-experimental")]
pub mod drift_capture;
mod error;
pub mod http;
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
pub mod oauth;
pub mod provider;
pub mod retrieval;
pub mod secrets;
pub mod session;
mod session_crypto;
mod session_ids;
pub mod stream;
pub mod tool;
pub mod turn;

pub use archive::{ArchiveSource, ItemRef};
pub use confirm::{
    action_hash, ConfirmError, PendingAction, PendingActionBinding, PendingRegistry,
};
pub use error::AgentError;
pub use provider::{AssistantBlock, DoneReason, FakeProvider, LlmProvider, StreamEvent, Usage};
pub use retrieval::RetrievalExecutor;
pub use secrets::{
    provider_api_key_secret_id, provider_oauth_refresh_secret_id, set_process_credential_key,
    AgentCredentialStore, AtRestKey, CredentialKeySource, CredentialStore, CredentialStoreConfig,
    CredentialStoreResolver, LocalKey, ProvidedKey, ProviderCredentialResolver, Secret,
    SecretClass,
};
pub use session::{
    detect_fork, new_ulid, ActiveTurn, FileSessionCache, InMemoryTransport, LeaseRecord,
    LoadedSession, LocalSessionCache, MemorySessionCache, PutTurnOutcome, Session, SessionFork,
    SessionTransport, Turn, TurnLeaseState,
};
pub use session_crypto::{KdfProfile, PairingPayload, SessionCryptoConfig};
pub use session_ids::{DeviceId, LeaseId, SessionId, TurnId};
pub use stream::AgentStreamHub;
pub use tool::{
    help_text, parse_action, registry_tool_names, tool_schema, ToolAction, ToolClass, TOOL_NAME,
};
pub use turn::{run_turn, Message, Role, ToolExecutor, ToolUseRef, TurnOutcome};

#[cfg(feature = "retrieval")]
pub use archive::StoreArchive;
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
pub use oauth::{AgentOAuth, OAuthConfig, StartedLogin};
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
pub use provider::codex::{CodexConfig, CodexProvider};
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
pub use provider::subscription::{SubscriptionConfig, SubscriptionProvider};
#[cfg(feature = "byo-api-providers")]
pub use provider::{anthropic::AnthropicProvider, openai::OpenAiProvider};
#[cfg(feature = "onedrive")]
pub use session::OneDriveTransport;

#[cfg(test)]
mod tests {
    #[test]
    fn agent_public_api_does_not_export_raw_session_crypto() {
        let lib = include_str!("lib.rs");
        let production_exports = lib.split("#[cfg(test)]").next().unwrap_or(lib);
        assert!(!production_exports.contains("pub mod session_crypto"));
        assert!(!production_exports.contains("pub use session_crypto::{open"));
        assert!(!production_exports.contains("pub use session_crypto::{seal"));
        assert!(!production_exports.contains("SealedTurn"));
        assert!(!production_exports.contains("SessionKey"));
    }

    #[test]
    fn public_session_api_has_no_lease_free_cloud_append() {
        let session = include_str!("session.rs");
        let session_impl = session
            .split("impl<T: SessionTransport, C: LocalSessionCache> Session<T, C>")
            .nth(1)
            .and_then(|s| {
                s.split("impl<T: SessionTransport, C: LocalSessionCache> ActiveTurn")
                    .next()
            })
            .expect("session impl block");
        assert!(!session_impl.contains("pub fn append("));
        assert!(!session.contains("append_lease_free_for_test"));
    }

    #[test]
    fn product_oauth_feature_does_not_export_byo_api_providers() {
        let lib = include_str!("lib.rs");
        assert!(lib.contains(
            "#[cfg(feature = \"byo-api-providers\")]\npub use provider::{anthropic::AnthropicProvider, openai::OpenAiProvider};"
        ));
        assert!(
            !lib.contains(
                "#[cfg(feature = \"http\")]\npub use provider::{anthropic::AnthropicProvider, openai::OpenAiProvider};"
            ),
            "http/agent-oauth-providers must not re-export BYO API-key provider types"
        );

        let provider = include_str!("provider.rs");
        assert!(provider
            .contains("#[cfg(any(feature = \"byo-api-providers\", test))]\npub mod openai;"));
    }
}
