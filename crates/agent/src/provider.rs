//! Provider abstraction + streamed events. The turn loop drives any [`LlmProvider`];
//! [`FakeProvider`] is the deterministic CI provider (no real LLM tokens).

use crate::tool::ToolAction;
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
use crate::AgentError;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressPhase {
    ProviderStarted,
}

impl ProgressPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProviderStarted => "provider_started",
        }
    }
}

/// One streamed event produced while a turn runs. This is the typed event set the
/// `AgentStreamHub` will carry to the UI (REQ-AGENT-007); here it is emitted via a sink.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Closed lifecycle progress. It carries no provider text or request metadata.
    Progress { phase: ProgressPhase },
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
            Self::Progress { .. } => "progress",
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
            Self::Progress { phase } => {
                serde_json::json!({ "event": "progress", "phase": phase.as_str() })
            }
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

    fn next_cancellable(
        &mut self,
        history: &[crate::turn::Message],
        emit: &mut dyn FnMut(StreamEvent),
        _cancellation: Option<&crate::CancellationToken>,
    ) -> Result<Vec<AssistantBlock>, crate::AgentError> {
        self.next(history, emit)
    }

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

// ------------------------------------------------------------------ #639 T6: runtime attestation

/// The iSyncYou harness contract version the runtime attestation enforces (#639). Bound into the
/// product activation record so a credential activated under an older contract cannot read as ready.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
pub const HARNESS_CONTRACT_VERSION: u32 = 1;

/// Which provider's positive harness allowlist to enforce.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessProvider {
    Claude,
    Codex,
}

/// A request the transport is allowed to send (#639). It can ONLY be produced by
/// [`build_attested_provider_request`], which validates it against the positive harness allowlist,
/// so a caller cannot hand the transport an un-attested `(Value, headers)`. It is immutable: any
/// header/body change requires building — and re-attesting — a fresh request.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
#[derive(Debug, Clone)]
pub(crate) struct AttestedProviderRequest {
    url: String,
    headers: Vec<(String, String)>,
    body: serde_json::Value,
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
impl AttestedProviderRequest {
    pub(crate) fn url(&self) -> &str {
        &self.url
    }
    pub(crate) fn headers(&self) -> &[(String, String)] {
        &self.headers
    }
    pub(crate) fn body(&self) -> &serde_json::Value {
        &self.body
    }
}

/// Values already owned by a concrete product provider and bound into this round's attestation.
/// Callers cannot substitute a different token, account, model, or prompt after construction.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
pub(crate) enum ProviderRequestBinding<'a> {
    Claude {
        access_token: &'a str,
        session_id: &'a str,
        account_uuid: &'a str,
        device_id: &'a str,
        model: &'a str,
        system: &'a str,
    },
    Codex {
        access_token: &'a str,
        account_id: &'a str,
        model: &'a str,
        reasoning_effort: codex::CodexReasoningEffort,
        instructions: &'a str,
    },
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
impl ProviderRequestBinding<'_> {
    fn provider(&self) -> HarnessProvider {
        match self {
            Self::Claude { .. } => HarnessProvider::Claude,
            Self::Codex { .. } => HarnessProvider::Codex,
        }
    }
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn harness_violation(msg: &str) -> AgentError {
    AgentError::Provider(format!("harness attestation failed: {msg}"))
}

/// The default-client components that must never appear anywhere in a product request.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
const FORBIDDEN_HARNESS_KEYS: &[&str] = &[
    "client_context",
    "commands",
    "cwd",
    "default_system_prompt",
    "filesystem",
    "history",
    "mcp",
    "mcp_servers",
    "memories",
    "plugins",
    "rules",
    "shell",
    "skills",
    "system_prompt",
    "workspace",
];

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn reject_forbidden_components(value: &serde_json::Value) -> Result<(), AgentError> {
    match value {
        serde_json::Value::Object(object) => {
            for (key, nested) in object {
                if FORBIDDEN_HARNESS_KEYS.contains(&key.as_str()) {
                    return Err(harness_violation(&format!(
                        "default-client component present: {key}"
                    )));
                }
                reject_forbidden_components(nested)?;
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                reject_forbidden_components(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// The request `tools` must be the exact provider-specific projection of the single canonical
/// `isyncyou` schema. Name/count-only checks would allow a widened input contract.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn attest_single_isyncyou_tool(
    provider: HarnessProvider,
    tools: Option<&serde_json::Value>,
) -> Result<(), AgentError> {
    let expected = match provider {
        HarnessProvider::Claude => serde_json::json!([crate::tool::tool_schema()]),
        HarnessProvider::Codex => codex::responses_tools(),
    };
    if tools != Some(&expected) {
        return Err(harness_violation(
            "tool schema is not the canonical isyncyou schema",
        ));
    }
    Ok(())
}

/// Header order, names, and values are part of the retained provider envelope. This also rejects
/// extra/duplicate headers and binds auth/account identity to the provider instance.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn require_exact_headers(
    headers: &[(String, String)],
    expected: Vec<(String, String)>,
) -> Result<(), AgentError> {
    if headers != expected.as_slice() {
        return Err(harness_violation(
            "provider headers differ from the exact envelope",
        ));
    }
    Ok(())
}

/// #639 (F4): no header name may appear more than once (case-insensitive) — a duplicate could
/// smuggle a second, unattested value past a name-only check.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn reject_duplicate_headers(headers: &[(String, String)]) -> Result<(), AgentError> {
    for i in 0..headers.len() {
        for j in (i + 1)..headers.len() {
            if headers[i].0.eq_ignore_ascii_case(&headers[j].0) {
                return Err(harness_violation(&format!(
                    "duplicate header: {}",
                    headers[i].0
                )));
            }
        }
    }
    Ok(())
}

/// Validate a provider request against the positive harness allowlist — exact top-level keys,
/// exactly one `isyncyou` tool, the retained envelope invariants (Claude billing block first,
/// Codex `store:false`), streaming, and all mandatory headers. No default-client component may appear.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn attest_product_harness(
    binding: &ProviderRequestBinding<'_>,
    url: &str,
    headers: &[(String, String)],
    body: &serde_json::Value,
) -> Result<(), AgentError> {
    let provider = binding.provider();
    reject_forbidden_components(body)?;
    reject_duplicate_headers(headers)?;
    let obj = body
        .as_object()
        .ok_or_else(|| harness_violation("request body must be an object"))?;
    let keys: std::collections::BTreeSet<&str> = obj.keys().map(|k| k.as_str()).collect();
    let stream_true = obj.get("stream") == Some(&serde_json::Value::Bool(true));
    let responses_lite = matches!(
        binding,
        ProviderRequestBinding::Codex { model, .. } if codex::uses_responses_lite(model)
    );
    let tools = if responses_lite {
        obj.get("input")
            .and_then(serde_json::Value::as_array)
            .and_then(|input| input.first())
            .and_then(|item| item.get("tools"))
    } else {
        obj.get("tools")
    };
    attest_single_isyncyou_tool(provider, tools)?;
    match provider {
        HarnessProvider::Claude => {
            let ProviderRequestBinding::Claude {
                access_token,
                session_id,
                account_uuid,
                device_id,
                model,
                system: expected_system,
            } = binding
            else {
                return Err(harness_violation("claude binding mismatch"));
            };
            if access_token.is_empty()
                || session_id.is_empty()
                || model.is_empty()
                || expected_system.is_empty()
            {
                return Err(harness_violation(
                    "claude binding contains an empty required value",
                ));
            }
            // #639 (F4): the request URL is bound exactly to the official endpoint.
            if url != subscription::MESSAGES_URL {
                return Err(harness_violation(
                    "claude request URL is not the official endpoint",
                ));
            }
            let allowed: std::collections::BTreeSet<&str> = [
                "max_tokens",
                "messages",
                "metadata",
                "model",
                "stream",
                "system",
                "tools",
            ]
            .into_iter()
            .collect();
            if keys != allowed {
                return Err(harness_violation(
                    "claude request has non-allowlisted top-level keys",
                ));
            }
            let system = obj
                .get("system")
                .and_then(|s| s.as_array())
                .ok_or_else(|| harness_violation("claude system must be a block array"))?;
            if system.len() != 2 {
                return Err(harness_violation(
                    "claude system must be the billing block + one prompt",
                ));
            }
            let first = system
                .first()
                .and_then(|b| b.get("text"))
                .and_then(|t| t.as_str())
                .unwrap_or_default();
            // #639 (F4): the retained billing block is bound EXACTLY (not merely by prefix).
            if first != subscription::expected_product_billing_block() {
                return Err(harness_violation(
                    "claude billing block must be first and exactly the official envelope",
                ));
            }
            let expected_prompt = serde_json::json!({
                "type": "text",
                "text": expected_system,
                "cache_control": { "type": "ephemeral" },
            });
            if system.get(1) != Some(&expected_prompt) {
                return Err(harness_violation(
                    "claude prompt block differs from the selected harness",
                ));
            }
            if obj.get("model").and_then(|v| v.as_str()) != Some(*model)
                || obj.get("max_tokens").and_then(|v| v.as_u64()) != Some(4096)
                || !obj.get("messages").is_some_and(|v| v.is_array())
            {
                return Err(harness_violation(
                    "claude protocol fields differ from the request binding",
                ));
            }
            let metadata = obj
                .get("metadata")
                .and_then(|v| v.as_object())
                .filter(|v| v.len() == 1)
                .and_then(|v| v.get("user_id"))
                .and_then(|v| v.as_str())
                .and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok())
                .and_then(|v| v.as_object().cloned())
                .ok_or_else(|| {
                    harness_violation("claude metadata is not the closed identity object")
                })?;
            let metadata_keys: std::collections::BTreeSet<&str> =
                metadata.keys().map(String::as_str).collect();
            if metadata_keys
                != std::collections::BTreeSet::from(["account_uuid", "device_id", "session_id"])
                || metadata.get("session_id").and_then(|v| v.as_str()) != Some(*session_id)
                || metadata.get("account_uuid").and_then(|v| v.as_str()) != Some(*account_uuid)
                || metadata.get("device_id").and_then(|v| v.as_str()) != Some(*device_id)
            {
                return Err(harness_violation(
                    "claude metadata identity differs from the binding",
                ));
            }
            if !stream_true {
                return Err(harness_violation("claude stream must be true"));
            }
            require_exact_headers(
                headers,
                vec![
                    ("authorization".into(), format!("Bearer {access_token}")),
                    (
                        "anthropic-version".into(),
                        subscription::ANTHROPIC_VERSION.into(),
                    ),
                    ("anthropic-beta".into(), subscription::ANTHROPIC_BETA.into()),
                    (
                        "anthropic-dangerous-direct-browser-access".into(),
                        "true".into(),
                    ),
                    (
                        "user-agent".into(),
                        format!(
                            "claude-cli/{} (external, sdk-cli)",
                            subscription::DEFAULT_CLI_VERSION
                        ),
                    ),
                    ("x-app".into(), "cli".into()),
                    ("x-claude-code-session-id".into(), (*session_id).to_string()),
                    ("content-type".into(), "application/json".into()),
                    ("accept".into(), "text/event-stream".into()),
                ],
            )?;
        }
        HarnessProvider::Codex => {
            let ProviderRequestBinding::Codex {
                access_token,
                account_id,
                model,
                reasoning_effort,
                instructions,
            } = binding
            else {
                return Err(harness_violation("codex binding mismatch"));
            };
            if access_token.is_empty()
                || account_id.is_empty()
                || model.is_empty()
                || instructions.is_empty()
            {
                return Err(harness_violation(
                    "codex binding contains an empty required value",
                ));
            }
            // #639 (F4): the request URL is bound exactly to the official endpoint.
            if url != codex::RESPONSES_URL {
                return Err(harness_violation(
                    "codex request URL is not the official endpoint",
                ));
            }
            let allowed: std::collections::BTreeSet<&str> = if responses_lite {
                [
                    "input",
                    "include",
                    "model",
                    "parallel_tool_calls",
                    "reasoning",
                    "store",
                    "stream",
                    "tool_choice",
                ]
                .into_iter()
                .collect()
            } else {
                [
                    "input",
                    "instructions",
                    "include",
                    "model",
                    "parallel_tool_calls",
                    "reasoning",
                    "store",
                    "stream",
                    "tool_choice",
                    "tools",
                ]
                .into_iter()
                .collect()
            };
            if keys != allowed {
                return Err(harness_violation(
                    "codex request has non-allowlisted top-level keys",
                ));
            }
            if obj.get("store") != Some(&serde_json::Value::Bool(false)) {
                return Err(harness_violation("codex store must be false"));
            }
            let reasoning = obj
                .get("reasoning")
                .and_then(|value| value.as_object())
                .ok_or_else(|| harness_violation("codex reasoning shape is invalid"))?;
            let input = obj
                .get("input")
                .and_then(|value| value.as_array())
                .ok_or_else(|| harness_violation("codex input shape is invalid"))?;
            let expected_tool_choice = codex::tool_choice_for_input(input);
            let expected_reasoning_len = if responses_lite { 2 } else { 1 };
            let lite_prefix_matches = !responses_lite
                || input.first().is_some_and(|value| {
                    value
                        == &serde_json::json!({
                            "type": "additional_tools",
                            "role": "developer",
                            "tools": codex::responses_tools(),
                        })
                }) && input.get(1).is_some_and(|value| {
                    value
                        == &serde_json::json!({
                            "type": "message",
                            "role": "developer",
                            "content": [{"type": "input_text", "text": instructions}],
                        })
                });
            if obj.get("model").and_then(|v| v.as_str()) != Some(*model)
                || (!responses_lite
                    && obj.get("instructions").and_then(|v| v.as_str()) != Some(*instructions))
                || reasoning.len() != expected_reasoning_len
                || reasoning.get("effort").and_then(|v| v.as_str())
                    != Some(reasoning_effort.as_str())
                || (responses_lite
                    && reasoning.get("context").and_then(|v| v.as_str()) != Some("all_turns"))
                || obj.get("include") != Some(&serde_json::json!(["reasoning.encrypted_content"]))
                || obj.get("parallel_tool_calls") != Some(&serde_json::Value::Bool(false))
                || obj.get("tool_choice").and_then(|v| v.as_str()) != Some(expected_tool_choice)
                || !lite_prefix_matches
            {
                return Err(harness_violation(
                    "codex protocol fields differ from the request binding",
                ));
            }
            if !stream_true {
                return Err(harness_violation("codex stream must be true"));
            }
            let mut expected_headers = vec![
                ("authorization".into(), format!("Bearer {access_token}")),
                ("chatgpt-account-id".into(), (*account_id).to_string()),
                ("originator".into(), codex::ORIGINATOR.into()),
                (
                    "user-agent".into(),
                    format!("{}/{}", codex::ORIGINATOR, codex::DEFAULT_CLI_VERSION),
                ),
                ("accept".into(), "text/event-stream".into()),
            ];
            if responses_lite {
                expected_headers.push((
                    "x-openai-internal-codex-responses-lite".into(),
                    "true".into(),
                ));
            }
            require_exact_headers(headers, expected_headers)?;
        }
    }
    Ok(())
}

/// Build + attest the exact request that will be sent this round (#639). `next()` calls this per
/// round with its **current history**, so what the transport sends is always freshly attested. On
/// any violation it returns `Err` and no request is produced.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
pub(crate) fn build_attested_provider_request(
    binding: ProviderRequestBinding<'_>,
    url: String,
    headers: Vec<(String, String)>,
    body: serde_json::Value,
) -> Result<AttestedProviderRequest, AgentError> {
    attest_product_harness(&binding, &url, &headers, &body)?;
    Ok(AttestedProviderRequest { url, headers, body })
}

/// #639 T7: the STATIC harness attestation used by product readiness. It proves the SHIPPED
/// harness (fixed system template + single `isyncyou` tool + provider envelope) for `provider`
/// still conforms to `HARNESS_CONTRACT_VERSION`, independent of any credential or history. It is a
/// defense-in-depth guard distinct from the per-round [`build_attested_provider_request`] that
/// authorizes each actually-sent request: a build whose harness has drifted from the contract can
/// never read as ready. Placeholder credentials are used only to materialize the request shape.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
pub fn attest_static_product_harness(
    provider: HarnessProvider,
    expected_system: &str,
) -> Result<(), AgentError> {
    if expected_system.is_empty() {
        return Err(harness_violation("static harness system prompt is empty"));
    }
    let history: [crate::turn::Message; 0] = [];
    match provider {
        HarnessProvider::Claude => {
            let p = subscription::SubscriptionProvider::new(
                "static-attestation-probe",
                "static-attestation",
                expected_system,
                subscription::SubscriptionConfig::default(),
            )?;
            attest_product_harness(
                &ProviderRequestBinding::Claude {
                    access_token: "static-attestation-probe",
                    session_id: &p.session_id,
                    account_uuid: &p.cfg.account_uuid,
                    device_id: &p.cfg.device_id,
                    model: "static-attestation",
                    system: expected_system,
                },
                subscription::MESSAGES_URL,
                &p.request_headers(),
                &p.request_body(&history),
            )
        }
        HarnessProvider::Codex => {
            let p = codex::CodexProvider::new(
                "static-attestation-probe",
                expected_system,
                codex::CodexConfig {
                    account_id: "static-account-binding".into(),
                    ..Default::default()
                },
            )?;
            attest_product_harness(
                &ProviderRequestBinding::Codex {
                    access_token: "static-attestation-probe",
                    account_id: "static-account-binding",
                    model: &codex::CodexConfig::default().model,
                    reasoning_effort: codex::CodexConfig::default().reasoning_effort,
                    instructions: expected_system,
                },
                codex::RESPONSES_URL,
                &p.request_headers(),
                &codex::build_request(
                    &codex::CodexConfig::default().model,
                    codex::CodexConfig::default().reasoning_effort,
                    expected_system,
                    &history,
                ),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    use std::collections::BTreeSet;

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn assert_no_default_client_components(value: &serde_json::Value) {
        const FORBIDDEN_KEYS: &[&str] = &[
            "client_context",
            "commands",
            "cwd",
            "default_system_prompt",
            "filesystem",
            "history",
            "mcp",
            "mcp_servers",
            "memories",
            "plugins",
            "rules",
            "shell",
            "skills",
            "system_prompt",
            "workspace",
        ];
        match value {
            serde_json::Value::Object(object) => {
                for (key, nested) in object {
                    assert!(
                        !FORBIDDEN_KEYS.contains(&key.as_str()),
                        "custom harness retained forbidden default-client key: {key}"
                    );
                    assert_no_default_client_components(nested);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    assert_no_default_client_components(item);
                }
            }
            _ => {}
        }
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn harness_requests() -> (serde_json::Value, serde_json::Value) {
        let history = [crate::turn::Message::user("controlled user request")];
        let claude = subscription::SubscriptionProvider::new(
            "claude-oauth-token",
            "claude-test",
            "iSyncYou controlled system prompt",
            subscription::SubscriptionConfig {
                account_uuid: "account-identity".into(),
                device_id: "device-identity".into(),
                ..Default::default()
            },
        )
        .unwrap()
        .request_body(&history);
        let codex = codex::build_request(
            "codex-test",
            codex::CodexReasoningEffort::Medium,
            "iSyncYou controlled system prompt",
            &history,
        );
        (claude, codex)
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn custom_harness_replaces_only_non_auth_non_billing_sections() {
        let claude_provider = subscription::SubscriptionProvider::new(
            "claude-oauth-token",
            "claude-test",
            "iSyncYou controlled system prompt",
            subscription::SubscriptionConfig::default(),
        )
        .unwrap();
        let claude_headers = claude_provider.request_headers();
        let claude_system = claude_provider.system_blocks();
        let codex_provider = codex::CodexProvider::new(
            "codex-oauth-token",
            "iSyncYou controlled system prompt",
            codex::CodexConfig {
                account_id: "codex-account-identity".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let codex_headers = codex_provider.request_headers();

        assert_eq!(claude_headers[0].0, "authorization");
        assert_eq!(claude_headers[0].1, "Bearer claude-oauth-token");
        assert_eq!(
            claude_system[0]["text"],
            "x-anthropic-billing-header: cc_version=2.1.207.cab; cc_entrypoint=sdk-cli; cch=00000;"
        );
        assert_eq!(
            claude_system[1]["text"],
            "iSyncYou controlled system prompt"
        );
        assert_eq!(codex_headers[0].0, "authorization");
        assert_eq!(codex_headers[0].1, "Bearer codex-oauth-token");
        assert_eq!(codex_headers[1].0, "chatgpt-account-id");
        assert_eq!(codex_headers[1].1, "codex-account-identity");
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn custom_harness_removes_default_prompt_tools_skills_mcp_rules_and_history() {
        let (claude, codex) = harness_requests();
        assert_no_default_client_components(&claude);
        assert_no_default_client_components(&codex);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn custom_harness_installs_only_isyncyou_prompt_and_tool() {
        let (claude, codex) = harness_requests();

        assert_eq!(claude["system"].as_array().unwrap().len(), 2);
        assert_eq!(
            claude["system"][1]["text"],
            "iSyncYou controlled system prompt"
        );
        assert_eq!(claude["tools"].as_array().unwrap().len(), 1);
        assert_eq!(claude["tools"][0]["name"], crate::tool::TOOL_NAME);
        assert_eq!(codex["instructions"], "iSyncYou controlled system prompt");
        assert_eq!(codex["tools"].as_array().unwrap().len(), 1);
        assert_eq!(codex["tools"][0]["name"], crate::tool::TOOL_NAME);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn product_harness_retains_only_required_provider_identity_fields() {
        let claude = subscription::SubscriptionProvider::new(
            "claude-oauth-token",
            "claude-test",
            "iSyncYou controlled system prompt",
            subscription::SubscriptionConfig::default(),
        )
        .unwrap();
        let claude_names = claude
            .request_headers()
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        let codex = codex::CodexProvider::new(
            "codex-oauth-token",
            "iSyncYou controlled system prompt",
            codex::CodexConfig {
                account_id: "codex-account-identity".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let codex_names = codex
            .request_headers()
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>();

        assert_eq!(
            claude_names,
            [
                "authorization",
                "anthropic-version",
                "anthropic-beta",
                "anthropic-dangerous-direct-browser-access",
                "user-agent",
                "x-app",
                "x-claude-code-session-id",
                "content-type",
                "accept",
            ]
        );
        assert_eq!(
            codex_names,
            [
                "authorization",
                "chatgpt-account-id",
                "originator",
                "user-agent",
                "accept",
                "x-openai-internal-codex-responses-lite",
            ]
        );
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn codex_static_product_harness_accepts_responses_lite_shape() {
        attest_static_product_harness(HarnessProvider::Codex, "iSyncYou controlled system prompt")
            .expect("the shipped default Codex harness must remain ready");
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn product_harness_contains_no_default_client_agent_components() {
        let (claude, codex) = harness_requests();
        assert_no_default_client_components(&claude);
        assert_no_default_client_components(&codex);

        let claude_keys = claude
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let codex_keys = codex
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        assert_eq!(
            claude_keys,
            BTreeSet::from([
                "max_tokens".into(),
                "messages".into(),
                "metadata".into(),
                "model".into(),
                "stream".into(),
                "system".into(),
                "tools".into(),
            ])
        );
        assert_eq!(
            codex_keys,
            BTreeSet::from([
                "input".into(),
                "include".into(),
                "instructions".into(),
                "model".into(),
                "parallel_tool_calls".into(),
                "reasoning".into(),
                "store".into(),
                "stream".into(),
                "tool_choice".into(),
                "tools".into(),
            ])
        );
    }

    // ---------------------------------------------------------------- #639 T6: attestation tests

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn valid_claude_request(message: &str) -> (String, Vec<(String, String)>, serde_json::Value) {
        let history = [crate::turn::Message::user(message)];
        let mut provider = subscription::SubscriptionProvider::new(
            "claude-oauth-token",
            "claude-test",
            "iSyncYou controlled system prompt",
            subscription::SubscriptionConfig {
                account_uuid: "account-identity".into(),
                device_id: "device-identity".into(),
                ..Default::default()
            },
        )
        .unwrap();
        provider.session_id = "11111111-1111-4111-8111-111111111111".into();
        (
            subscription::MESSAGES_URL.to_string(),
            provider.request_headers(),
            provider.request_body(&history),
        )
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn valid_codex_request(message: &str) -> (String, Vec<(String, String)>, serde_json::Value) {
        let history = [crate::turn::Message::user(message)];
        let provider = codex::CodexProvider::new(
            "codex-oauth-token",
            "iSyncYou controlled system prompt",
            codex::CodexConfig {
                account_id: "codex-account-identity".into(),
                model: "codex-test".into(),
                ..Default::default()
            },
        )
        .unwrap();
        (
            codex::RESPONSES_URL.to_string(),
            provider.request_headers(),
            codex::build_request(
                "codex-test",
                codex::CodexReasoningEffort::Medium,
                "iSyncYou controlled system prompt",
                &history,
            ),
        )
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn attest_test_request(
        provider: HarnessProvider,
        url: String,
        headers: Vec<(String, String)>,
        body: serde_json::Value,
    ) -> Result<AttestedProviderRequest, AgentError> {
        let binding = match provider {
            HarnessProvider::Claude => ProviderRequestBinding::Claude {
                access_token: "claude-oauth-token",
                session_id: "11111111-1111-4111-8111-111111111111",
                account_uuid: "account-identity",
                device_id: "device-identity",
                model: "claude-test",
                system: "iSyncYou controlled system prompt",
            },
            HarnessProvider::Codex => ProviderRequestBinding::Codex {
                access_token: "codex-oauth-token",
                account_id: "codex-account-identity",
                model: "codex-test",
                reasoning_effort: codex::CodexReasoningEffort::Medium,
                instructions: "iSyncYou controlled system prompt",
            },
        };
        build_attested_provider_request(binding, url, headers, body)
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn every_provider_round_attests_its_current_history() {
        // Two rounds with different histories each produce a freshly attested request whose body
        // reflects THAT round's history — attestation is not a one-time static gate.
        for provider_kind in [HarnessProvider::Claude, HarnessProvider::Codex] {
            for message in ["first round request", "second round request"] {
                let (url, headers, body) = match provider_kind {
                    HarnessProvider::Claude => valid_claude_request(message),
                    HarnessProvider::Codex => valid_codex_request(message),
                };
                let attested = attest_test_request(provider_kind, url, headers, body).unwrap();
                assert!(
                    serde_json::to_string(attested.body())
                        .unwrap()
                        .contains(message),
                    "{provider_kind:?} attested body must carry the current round's history"
                );
            }
        }
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn transport_accepts_only_attested_provider_request() {
        // The only way to obtain an AttestedProviderRequest (the sole value the product transport accepts)
        // is through build_attested_provider_request, which validates the plan. A conforming plan
        // yields one whose accessors expose exactly the attested url/headers/body; a non-conforming
        // plan yields none.
        let (url, headers, body) = valid_claude_request("controlled user request");
        let attested = attest_test_request(
            HarnessProvider::Claude,
            url.clone(),
            headers.clone(),
            body.clone(),
        )
        .unwrap();
        assert_eq!(attested.url(), url);
        assert_eq!(attested.headers(), headers.as_slice());
        assert_eq!(attested.body(), &body);

        let mut forbidden = body;
        forbidden
            .as_object_mut()
            .unwrap()
            .insert("mcp_servers".into(), json!(["default-client-mcp"]));
        assert!(
            attest_test_request(HarnessProvider::Claude, url, headers, forbidden).is_err(),
            "transport must not receive an attested request for a non-conforming plan"
        );
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn attestation_cannot_be_reused_after_header_or_body_mutation() {
        // AttestedProviderRequest is immutable (private fields, no setters): any header/body change
        // forces a rebuild, and the rebuild re-attests — a mutated plan cannot be re-attested.
        let (url, headers, body) = valid_codex_request("controlled user request");
        attest_test_request(
            HarnessProvider::Codex,
            url.clone(),
            headers.clone(),
            body.clone(),
        )
        .expect("baseline plan attests");

        // Body mutation: store:false -> true (retained-envelope invariant broken).
        let mut mutated_body = body.clone();
        mutated_body
            .as_object_mut()
            .unwrap()
            .insert("store".into(), json!(true));
        assert!(
            attest_test_request(
                HarnessProvider::Codex,
                url.clone(),
                headers.clone(),
                mutated_body,
            )
            .is_err(),
            "mutated body must fail re-attestation"
        );

        // Header mutation: drop the mandatory authorization header.
        let mutated_headers: Vec<(String, String)> = headers
            .into_iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("authorization"))
            .collect();
        assert!(
            attest_test_request(HarnessProvider::Codex, url, mutated_headers, body).is_err(),
            "mutated headers must fail re-attestation"
        );
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn codex_reasoning_effort_is_bound_into_request_attestation() {
        let (url, headers, body) = valid_codex_request("controlled user request");
        attest_test_request(
            HarnessProvider::Codex,
            url.clone(),
            headers.clone(),
            body.clone(),
        )
        .expect("baseline reasoning effort attests");

        let mut mutated = body;
        mutated["reasoning"]["effort"] = json!("xhigh");
        assert!(
            attest_test_request(HarnessProvider::Codex, url, headers, mutated).is_err(),
            "reasoning effort must match the prepared provider binding"
        );
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn attestation_rejects_each_claude_field_mutation() {
        let base = || valid_claude_request("controlled user request");
        let attest = |headers: Vec<(String, String)>, body: serde_json::Value| {
            attest_test_request(
                HarnessProvider::Claude,
                subscription::MESSAGES_URL.to_string(),
                headers,
                body,
            )
        };

        // Extra top-level key.
        let (_, h, mut b) = base();
        b.as_object_mut().unwrap().insert("extra".into(), json!(1));
        assert!(attest(h, b).is_err(), "extra top-level key must fail");

        // Billing block no longer first.
        let (_, h, mut b) = base();
        let sys = b["system"].as_array().unwrap().clone();
        b["system"] = json!([sys[1].clone(), sys[0].clone()]);
        assert!(attest(h, b).is_err(), "reordered system blocks must fail");

        // System length != 2 (inject a third block).
        let (_, h, mut b) = base();
        b["system"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"text","text":"smuggled"}));
        assert!(attest(h, b).is_err(), "extra system block must fail");

        // stream not true.
        let (_, h, mut b) = base();
        b["stream"] = json!(false);
        assert!(attest(h, b).is_err(), "stream:false must fail");

        // More than one tool.
        let (_, h, mut b) = base();
        let tool = b["tools"][0].clone();
        b["tools"] = json!([tool.clone(), tool]);
        assert!(attest(h, b).is_err(), "second tool must fail");

        // Wrong tool name.
        let (_, h, mut b) = base();
        b["tools"][0]["name"] = json!("not-isyncyou");
        assert!(attest(h, b).is_err(), "renamed tool must fail");

        // Widened tool schema.
        let (_, h, mut b) = base();
        b["tools"][0]["input_schema"]["additionalProperties"] = json!(true);
        assert!(attest(h, b).is_err(), "changed tool schema must fail");

        // Prompt differs from the binding.
        let (_, h, mut b) = base();
        b["system"][1]["text"] = json!("different system prompt");
        assert!(attest(h, b).is_err(), "changed prompt must fail");

        // Missing mandatory header.
        let (_, h, b) = base();
        let stripped: Vec<_> = h
            .into_iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("anthropic-version"))
            .collect();
        assert!(attest(stripped, b).is_err(), "missing header must fail");

        // Wrong mandatory value and an extra header are both rejected by the exact envelope.
        let (_, mut h, b) = base();
        h[1].1 = "2099-01-01".into();
        assert!(attest(h, b).is_err(), "wrong header value must fail");
        let (_, mut h, b) = base();
        h.push(("x-extra".into(), "not-allowed".into()));
        assert!(attest(h, b).is_err(), "extra header must fail");

        // The retained metadata identity is bound to this provider instance, not only to the
        // expected field names.
        let (_, h, mut b) = base();
        let mut identity: serde_json::Value =
            serde_json::from_str(b["metadata"]["user_id"].as_str().unwrap()).unwrap();
        identity["account_uuid"] = json!("different-account");
        b["metadata"]["user_id"] = json!(identity.to_string());
        assert!(attest(h, b).is_err(), "changed account identity must fail");

        let (_, h, mut b) = base();
        let mut identity: serde_json::Value =
            serde_json::from_str(b["metadata"]["user_id"].as_str().unwrap()).unwrap();
        identity["device_id"] = json!("different-device");
        b["metadata"]["user_id"] = json!(identity.to_string());
        assert!(attest(h, b).is_err(), "changed device identity must fail");
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn attestation_rejects_each_codex_field_mutation() {
        let base = || valid_codex_request("controlled user request");
        let attest = |headers: Vec<(String, String)>, body: serde_json::Value| {
            attest_test_request(
                HarnessProvider::Codex,
                codex::RESPONSES_URL.to_string(),
                headers,
                body,
            )
        };

        // store:false -> true.
        let (_, h, mut b) = base();
        b["store"] = json!(true);
        assert!(attest(h, b).is_err(), "store:true must fail");

        // Extra top-level key.
        let (_, h, mut b) = base();
        b.as_object_mut().unwrap().insert("extra".into(), json!(1));
        assert!(attest(h, b).is_err(), "extra top-level key must fail");

        // stream not true.
        let (_, h, mut b) = base();
        b["stream"] = json!(false);
        assert!(attest(h, b).is_err(), "stream:false must fail");

        // Wrong tool name.
        let (_, h, mut b) = base();
        b["tools"][0]["name"] = json!("not-isyncyou");
        assert!(attest(h, b).is_err(), "renamed tool must fail");

        let (_, h, mut b) = base();
        b["tools"][0]["parameters"]["additionalProperties"] = json!(true);
        assert!(attest(h, b).is_err(), "changed tool schema must fail");

        let (_, h, mut b) = base();
        b["tool_choice"] = json!("required");
        assert!(
            attest(h, b).is_err(),
            "the product must not force a tool call for ordinary chat"
        );

        let (_, h, mut b) = base();
        b["instructions"] = json!("different instructions");
        assert!(attest(h, b).is_err(), "changed instructions must fail");

        // Missing mandatory account header.
        let (_, h, b) = base();
        let stripped: Vec<_> = h
            .into_iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("chatgpt-account-id"))
            .collect();
        assert!(
            attest(stripped, b).is_err(),
            "missing account header must fail"
        );

        let (_, mut h, b) = base();
        h[1].1 = "other-account".into();
        assert!(attest(h, b).is_err(), "changed account binding must fail");
        let (_, mut h, b) = base();
        h.push(("x-extra".into(), "not-allowed".into()));
        assert!(attest(h, b).is_err(), "extra header must fail");
    }

    // #639 (F4): the runtime attestation binds the URL, the EXACT billing envelope, rejects
    // duplicate headers, and requires a bearer authorization — not just header names.
    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn attestation_binds_url_exact_billing_and_header_integrity() {
        // Wrong URL is rejected for both providers.
        let (_, h, b) = valid_claude_request("q");
        assert!(attest_test_request(
            HarnessProvider::Claude,
            "https://evil.example/v1/messages".into(),
            h,
            b
        )
        .is_err());
        let (_, h, b) = valid_codex_request("q");
        assert!(attest_test_request(
            HarnessProvider::Codex,
            "https://evil.example/responses".into(),
            h,
            b
        )
        .is_err());

        // A tampered billing block (right prefix, wrong content) is rejected (exact, not prefix).
        let (url, h, mut b) = valid_claude_request("q");
        b["system"][0]["text"] = json!(
            "x-anthropic-billing-header: cc_version=9.9.9.cab; cc_entrypoint=sdk-cli; cch=00000;"
        );
        assert!(attest_test_request(HarnessProvider::Claude, url, h, b).is_err());

        // A duplicate header is rejected.
        let (url, mut h, b) = valid_claude_request("q");
        h.push(("anthropic-version".into(), "sneaky".into()));
        assert!(attest_test_request(HarnessProvider::Claude, url, h, b).is_err());

        // A non-bearer authorization is rejected.
        let (url, h, b) = valid_claude_request("q");
        let mutated: Vec<(String, String)> = h
            .into_iter()
            .map(|(k, v)| {
                if k.eq_ignore_ascii_case("authorization") {
                    (k, "Basic abc".into())
                } else {
                    (k, v)
                }
            })
            .collect();
        assert!(attest_test_request(HarnessProvider::Claude, url, mutated, b).is_err());
    }

    // #639 (F4): product providers hold only ProductHttpTransport and send only ATTESTED requests.
    // They cannot access the general OAuth/BYO transport through their stored transport capability.
    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn product_providers_send_only_attested_requests() {
        for src in [
            include_str!("provider/subscription.rs"),
            include_str!("provider/codex.rs"),
        ] {
            assert!(
                src.contains("post_attested_sse"),
                "product provider must send via post_attested_sse"
            );
            assert!(
                src.contains("crate::http::ProductHttpTransport"),
                "product provider must hold only the restricted product transport"
            );
            assert!(
                !src.contains("post_json_sse"),
                "product provider must not call the un-attested post_json_sse directly"
            );
            assert!(
                !src.contains("crate::http::HttpTransport::shared"),
                "product provider must not acquire the general OAuth/BYO transport"
            );
        }
    }

    #[test]
    fn agent_stream_event_json_is_single_line_and_stable() {
        let events = [
            StreamEvent::Progress {
                phase: ProgressPhase::ProviderStarted,
            },
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
        assert!(names.contains(&"progress"));
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
            } else if event.event_name() == "progress" {
                assert_eq!(parsed["phase"], "provider_started");
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
