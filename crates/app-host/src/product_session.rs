use base64::Engine as _;
use isyncyou_agent::{
    new_ulid, parse_action, payload_digest, request_object_digest, select_provider_context,
    tool_result_digest, AgentCredentialStore, AssistantBlock, IdempotencyTombstoneV1,
    LocalEffectCheckpointV1, NormalizedAssistantBlock, OneDriveSessionV2Transport, PairingPayload,
    PendingOwnerBinding, PersistedLeaseBinding, ProviderAttemptBindingV1, ReadToolCheckpointV1,
    RequestJournalV1, RequestPhase, RequestRouteDomain, RequestStepOutcomeV1, RequestStepRef,
    RequestUuidBindingV1, SanitizedUsage, Secret, SecretClass, SessionCommitV1, SessionId,
    SessionLeaseGuard, SessionObjectCrypto, SessionRecordKind, SessionRecordV2, SessionV2Error,
    SessionV2Store, SessionV2Transport, SourceRef, ToolAction, TurnObserver, TurnTerminalStatus,
    REQUEST_JOURNAL_VERSION, SESSION_RECORD_VERSION,
};
use serde::{Deserialize, Serialize};

const SESSION_INDEX_ID: &str = "product-session-v2-index";
const SESSION_SECRET_PREFIX: &str = "product-session-v2/";
const SESSION_INDEX_VERSION: u32 = 1;
const MAX_SESSIONS: usize = 128;
const MAX_DISPLAY_NAME_BYTES: usize = 128;
const MAX_INDEX_ENVELOPE_BYTES: usize = 256 * 1024;
const MAX_SESSION_SECRET_ENVELOPE_BYTES: usize = 16 * 1024;
const MAX_REQUEST_RECEIPTS: usize = 512;
const CURSOR_HMAC_DOMAIN: &[u8] = b"isyncyou-product-session-cursor-v1";
const DEFAULT_PAGE_SIZE: usize = 50;
const MAX_PAGE_SIZE: usize = 100;
const TURN_HOLDER_DOMAIN: &[u8] = b"isyncyou-session-holder-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductSessionSummaryV1 {
    pub session_id: String,
    pub display_name: Option<String>,
    pub created_at_ms: u64,
    pub archived: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProductSessionStoredSummaryV1 {
    session_id: String,
    display_name: Option<String>,
    created_at_ms: u64,
    archived: bool,
    local_account: String,
    // Missing means the record predates local-first creation. Those sessions were
    // created remotely before the local index was committed.
    #[serde(default)]
    remote_initialized: Option<bool>,
}

impl ProductSessionStoredSummaryV1 {
    fn public(&self) -> ProductSessionSummaryV1 {
        ProductSessionSummaryV1 {
            session_id: self.session_id.clone(),
            display_name: self.display_name.clone(),
            created_at_ms: self.created_at_ms,
            archived: self.archived,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProductSessionPageV1 {
    pub sessions: Vec<ProductSessionSummaryV1>,
    pub selected_session_id: Option<String>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProductSessionRequestReceiptV1 {
    request_id: String,
    route: String,
    payload_digest: String,
    session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProductSessionIndexV1 {
    version: u32,
    generation: u64,
    selected_session_id: Option<String>,
    sessions: Vec<ProductSessionStoredSummaryV1>,
    request_receipts: Vec<ProductSessionRequestReceiptV1>,
}

impl Default for ProductSessionIndexV1 {
    fn default() -> Self {
        Self {
            version: SESSION_INDEX_VERSION,
            generation: 0,
            selected_session_id: None,
            sessions: Vec::new(),
            request_receipts: Vec::new(),
        }
    }
}

impl ProductSessionIndexV1 {
    fn validate(&self) -> Result<(), String> {
        if self.version != SESSION_INDEX_VERSION
            || self.sessions.len() > MAX_SESSIONS
            || self.request_receipts.len() > MAX_REQUEST_RECEIPTS
        {
            return Err("session_store_unavailable".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        for session in &self.sessions {
            SessionId::new(&session.session_id).map_err(|_| "session_store_unavailable")?;
            if !ids.insert(session.session_id.as_str())
                || session
                    .display_name
                    .as_ref()
                    .is_some_and(|name| name.is_empty() || name.len() > MAX_DISPLAY_NAME_BYTES)
                || session.local_account.is_empty()
                || session.local_account.len() > 128
            {
                return Err("session_store_unavailable".into());
            }
        }
        let mut request_ids = std::collections::BTreeSet::new();
        for receipt in &self.request_receipts {
            if !request_ids.insert(receipt.request_id.as_str())
                || !matches!(
                    receipt.route.as_str(),
                    "create" | "select" | "import" | "archive"
                )
                || receipt.payload_digest.len() != 43
                || !ids.contains(receipt.session_id.as_str())
            {
                return Err("session_store_unavailable".into());
            }
        }
        if self
            .selected_session_id
            .as_ref()
            .is_some_and(|selected| !ids.contains(selected.as_str()))
        {
            return Err("session_store_unavailable".into());
        }
        Ok(())
    }
}

pub struct ProductSessionRegistry<'a> {
    store: &'a AgentCredentialStore,
}

fn product_session_index_gate() -> &'static std::sync::Mutex<()> {
    static GATE: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    GATE.get_or_init(|| std::sync::Mutex::new(()))
}

pub struct ProductTurnRuntime {
    guard: SessionLeaseGuard<OneDriveSessionV2Transport>,
    session_id: String,
    request_id: String,
    request_binding: RequestUuidBindingV1,
    turn_id: String,
    provider_binding: ProviderAttemptBindingV1,
    intent_record_id: String,
    intent_record: SessionRecordV2,
    context_records: Vec<SessionRecordV2>,
    journal: RequestJournalV1,
    pending_request_objects: Vec<(String, Vec<u8>)>,
    prepublished_provider_step: Option<u8>,
    seen_tool_use_ids: std::collections::BTreeSet<String>,
    provider_history: Vec<isyncyou_agent::Message>,
    recovery_outcomes: Vec<RequestStepOutcomeV1>,
    sources: Vec<SourceRef>,
}

pub enum ProductTurnStart {
    Started(Box<ProductTurnRuntime>),
    Replay(ProductTurnReplay),
}

pub struct ProductTurnRequest<'a> {
    pub graph_token: &'a str,
    pub session_id: &'a str,
    pub request_id: &'a str,
    pub turn_id: &'a str,
    pub local_account: &'a str,
    pub prompt: &'a str,
    pub provider_binding: ProviderAttemptBindingV1,
    pub installation_principal: &'a str,
    pub created_at_ms: u64,
    pub cached_context: Option<ProductSessionContextSnapshot>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProductSessionContextSnapshot {
    pub manifest_generation: u64,
    pub records: Vec<SessionRecordV2>,
}

pub struct ProductTurnReplay {
    pub phase: RequestPhase,
    pub final_text: Option<String>,
    pub error_code: Option<String>,
}

fn validate_provider_recovery_binding(
    recorded: &ProviderAttemptBindingV1,
    current: &ProviderAttemptBindingV1,
) -> Result<(), String> {
    recorded.revalidate(current).map_err(map_session_error)
}

impl ProductTurnRuntime {
    pub fn request_status_binding(&self) -> (String, String) {
        (self.session_id.clone(), self.request_id.clone())
    }

    pub fn provider_history(
        &mut self,
        prompt: &str,
        executor: &dyn isyncyou_agent::ToolExecutor,
    ) -> Result<Vec<isyncyou_agent::Message>, isyncyou_agent::AgentError> {
        let mut history = self.provider_history.clone();
        history.push(isyncyou_agent::Message::user(prompt));
        let outcomes = self.recovery_outcomes.clone();
        recover_provider_messages_runtime(&mut history, &outcomes, self, executor)?;
        Ok(history)
    }

    pub fn pending_owner(&self, account: &str) -> isyncyou_agent::PendingOwnerBinding {
        isyncyou_agent::PendingOwnerBinding {
            account: account.to_owned(),
            session_id: self.session_id.clone(),
            request_id: self.request_id.clone(),
            turn_id: self.turn_id.clone(),
        }
    }

    fn persist_provider_step_outcome(
        &mut self,
        step_seq: u8,
        normalized_blocks: Vec<NormalizedAssistantBlock>,
        final_text: Option<String>,
        terminal_validation_error: Option<String>,
    ) -> Result<(), isyncyou_agent::AgentError> {
        let outcome_id = new_ulid().map_err(|_| {
            isyncyou_agent::AgentError::Provider("session_store_unavailable".into())
        })?;
        let outcome = RequestStepOutcomeV1 {
            outcome_version: 1,
            outcome_id: outcome_id.clone(),
            step_seq,
            previous_outcome_id: self
                .journal
                .completed_steps
                .last()
                .map(|step| step.outcome_id.clone()),
            provider: self.provider_binding.provider,
            model: self.provider_binding.model.clone(),
            normalized_blocks,
            final_text,
            sanitized_usage: None,
            terminal_validation_error,
            outcome_digest: String::new(),
        }
        .seal_digest()
        .map_err(|error| isyncyou_agent::AgentError::Provider(error.to_string()))?;
        let outcome_bytes = serde_json::to_vec(&outcome).map_err(|_| {
            isyncyou_agent::AgentError::Provider("session_store_unavailable".into())
        })?;
        self.journal.completed_steps.push(RequestStepRef {
            step_seq,
            outcome_id: outcome_id.clone(),
            outcome_sha256: request_object_digest(&outcome_bytes),
        });
        self.journal.next_step_seq = step_seq
            .checked_add(1)
            .ok_or_else(|| isyncyou_agent::AgentError::Provider("turn_step_invalid".into()))?;
        self.journal.phase = RequestPhase::ProviderStepCompleted;
        let journal_id = new_ulid().map_err(|_| {
            isyncyou_agent::AgentError::Provider("session_store_unavailable".into())
        })?;
        self.pending_request_objects
            .push((outcome_id, outcome_bytes));
        self.pending_request_objects.push((
            journal_id,
            serde_json::to_vec(&self.journal).map_err(|_| {
                isyncyou_agent::AgentError::Provider("session_store_unavailable".into())
            })?,
        ));
        Ok(())
    }

    pub fn finish_final(
        mut self,
        text: String,
        usage: Option<SanitizedUsage>,
        created_at_ms: u64,
    ) -> Result<ProductSessionContextSnapshot, String> {
        let assistant_id = new_ulid().map_err(|_| "session_store_unavailable")?;
        let terminal_id = new_ulid().map_err(|_| "session_store_unavailable")?;
        let lease = self.guard.binding().map_err(map_session_error)?;
        let visible_records = vec![
            SessionRecordV2 {
                record_version: SESSION_RECORD_VERSION,
                record_id: assistant_id.clone(),
                session_id: self.session_id.clone(),
                request_id: self.request_id.clone(),
                turn_id: self.turn_id.clone(),
                kind: SessionRecordKind::AssistantResult {
                    text,
                    sources: self.sources.clone(),
                    usage,
                },
                parent_record_ids: vec![self.intent_record_id.clone()],
                observed_head: Some(self.intent_record_id.clone()),
                lease: lease.clone(),
                created_at_ms,
            },
            SessionRecordV2 {
                record_version: SESSION_RECORD_VERSION,
                record_id: terminal_id,
                session_id: self.session_id.clone(),
                request_id: self.request_id.clone(),
                turn_id: self.turn_id.clone(),
                kind: SessionRecordKind::TurnTerminal {
                    status: TurnTerminalStatus::Complete,
                    error_code: None,
                },
                parent_record_ids: vec![assistant_id.clone()],
                observed_head: Some(assistant_id),
                lease,
                created_at_ms,
            },
        ];
        let tombstone = self.terminal_tombstone(TurnTerminalStatus::Complete, &visible_records)?;
        self.guard
            .publish_terminal_with_request_objects(
                visible_records.clone(),
                &self.request_id,
                tombstone,
                std::mem::take(&mut self.pending_request_objects),
            )
            .map_err(map_session_error)?;
        self.context_snapshot_after_publish(visible_records)
    }

    pub fn finish_terminal(
        mut self,
        status: TurnTerminalStatus,
        error_code: Option<String>,
        _phase: RequestPhase,
        created_at_ms: u64,
    ) -> Result<ProductSessionContextSnapshot, String> {
        let terminal_id = new_ulid().map_err(|_| "session_store_unavailable")?;
        let lease = self.guard.binding().map_err(map_session_error)?;
        let visible_records = vec![SessionRecordV2 {
            record_version: SESSION_RECORD_VERSION,
            record_id: terminal_id,
            session_id: self.session_id.clone(),
            request_id: self.request_id.clone(),
            turn_id: self.turn_id.clone(),
            kind: SessionRecordKind::TurnTerminal { status, error_code },
            parent_record_ids: vec![self.intent_record_id.clone()],
            observed_head: Some(self.intent_record_id.clone()),
            lease,
            created_at_ms,
        }];
        let tombstone = self.terminal_tombstone(status, &visible_records)?;
        self.guard
            .publish_terminal_with_request_objects(
                visible_records.clone(),
                &self.request_id,
                tombstone,
                std::mem::take(&mut self.pending_request_objects),
            )
            .map_err(map_session_error)?;
        self.context_snapshot_after_publish(visible_records)
    }

    fn context_snapshot_after_publish(
        mut self,
        terminal_records: Vec<SessionRecordV2>,
    ) -> Result<ProductSessionContextSnapshot, String> {
        // Releasing the lease increments the manifest generation. A snapshot
        // tagged with the pre-release value is stale immediately and forces the
        // next turn to re-read all visible records from OneDrive.
        let manifest_generation = self.guard.release().unwrap_or(u64::MAX);
        let mut records = self.context_records;
        records.push(self.intent_record);
        records.extend(terminal_records);
        if records.len() > 128 {
            records.drain(..records.len() - 128);
        }
        Ok(ProductSessionContextSnapshot {
            manifest_generation,
            records,
        })
    }

    pub fn finish_pending(
        mut self,
        created_at_ms: u64,
    ) -> Result<ProductSessionContextSnapshot, String> {
        let pending_id = new_ulid().map_err(|_| "session_store_unavailable")?;
        let lease = self.guard.binding().map_err(map_session_error)?;
        let journal = self.terminal_journal(RequestPhase::PendingConfirmation)?;
        let mut request_objects = std::mem::take(&mut self.pending_request_objects);
        request_objects.push(journal);
        let pending_record = SessionRecordV2 {
            record_version: SESSION_RECORD_VERSION,
            record_id: pending_id,
            session_id: self.session_id.clone(),
            request_id: self.request_id.clone(),
            turn_id: self.turn_id.clone(),
            kind: SessionRecordKind::PendingOperation {
                code: "confirmation_required".into(),
            },
            parent_record_ids: vec![self.intent_record_id.clone()],
            observed_head: Some(self.intent_record_id.clone()),
            lease,
            created_at_ms,
        };
        self.guard
            .publish(SessionCommitV1 {
                visible_records: vec![pending_record.clone()],
                request_objects,
                uuid_bindings: vec![],
            })
            .map_err(map_session_error)?;
        self.context_snapshot_after_publish(vec![pending_record])
    }

    fn terminal_journal(&self, phase: RequestPhase) -> Result<(String, Vec<u8>), String> {
        let journal_id = new_ulid().map_err(|_| "session_store_unavailable")?;
        let mut journal = self.journal.clone();
        journal.phase = phase;
        Ok((
            journal_id,
            serde_json::to_vec(&journal).map_err(|_| "session_store_unavailable")?,
        ))
    }

    fn terminal_tombstone(
        &self,
        status: TurnTerminalStatus,
        visible_records: &[SessionRecordV2],
    ) -> Result<IdempotencyTombstoneV1, String> {
        Ok(IdempotencyTombstoneV1 {
            tombstone_version: 1,
            route_domain: self.request_binding.route_domain.clone(),
            session_scope: self.request_binding.session_scope.clone(),
            request_key: self.request_binding.request_key.clone(),
            payload_digest: self.request_binding.payload_digest.clone(),
            terminal_status: status,
            public_result_digest: request_object_digest(
                &serde_json::to_vec(visible_records).map_err(|_| "session_store_unavailable")?,
            ),
            visible_record_ids: visible_records
                .iter()
                .map(|record| record.record_id.clone())
                .collect(),
        })
    }

    fn publish_journal(&mut self) -> Result<(), String> {
        let journal_id = new_ulid().map_err(|_| "session_store_unavailable")?;
        let mut request_objects = self.pending_request_objects.clone();
        request_objects.push((
            journal_id,
            serde_json::to_vec(&self.journal).map_err(|_| "session_store_unavailable")?,
        ));
        self.guard
            .publish(SessionCommitV1 {
                visible_records: vec![],
                request_objects,
                uuid_bindings: vec![],
            })
            .map_err(map_session_error)?;
        self.pending_request_objects.clear();
        Ok(())
    }
}

trait RecoveryRuntimeState {
    fn recovery_journal(&self) -> &RequestJournalV1;
    fn recovery_sources_mut(&mut self) -> &mut Vec<SourceRef>;
    fn persist_read_started(
        &mut self,
        step_seq: u8,
        tool_use_id: &str,
        action: &ToolAction,
        local_effect: Option<&LocalEffectCheckpointV1>,
    ) -> Result<(), isyncyou_agent::AgentError>;
    fn persist_read_completed(
        &mut self,
        step_seq: u8,
        tool_use_id: &str,
        action: &ToolAction,
        result: &str,
    ) -> Result<(), isyncyou_agent::AgentError>;
}

impl RecoveryRuntimeState for ProductTurnRuntime {
    fn recovery_journal(&self) -> &RequestJournalV1 {
        &self.journal
    }

    fn recovery_sources_mut(&mut self) -> &mut Vec<SourceRef> {
        &mut self.sources
    }

    fn persist_read_started(
        &mut self,
        step_seq: u8,
        tool_use_id: &str,
        action: &ToolAction,
        local_effect: Option<&LocalEffectCheckpointV1>,
    ) -> Result<(), isyncyou_agent::AgentError> {
        self.read_tool_started(step_seq, tool_use_id, action, local_effect)
    }

    fn persist_read_completed(
        &mut self,
        step_seq: u8,
        tool_use_id: &str,
        action: &ToolAction,
        result: &str,
    ) -> Result<(), isyncyou_agent::AgentError> {
        self.read_tool_completed(step_seq, tool_use_id, action, result)
    }
}

fn recover_provider_messages_runtime<R: RecoveryRuntimeState>(
    history: &mut Vec<isyncyou_agent::Message>,
    outcomes: &[RequestStepOutcomeV1],
    runtime: &mut R,
    executor: &dyn isyncyou_agent::ToolExecutor,
) -> Result<(), isyncyou_agent::AgentError> {
    for outcome in outcomes {
        let mut text = String::new();
        let mut tool_uses = Vec::new();
        for block in &outcome.normalized_blocks {
            match block {
                NormalizedAssistantBlock::Text { text: block_text } => {
                    text.push_str(block_text);
                }
                NormalizedAssistantBlock::ToolUse {
                    tool_use_id,
                    action,
                } => tool_uses.push(isyncyou_agent::ToolUseRef {
                    id: tool_use_id.clone(),
                    input: serde_json::to_value(action).map_err(|_| {
                        isyncyou_agent::AgentError::Provider("turn_outcome_unknown".into())
                    })?,
                }),
                NormalizedAssistantBlock::RejectedToolUse { tool_use_id, .. } => {
                    tool_uses.push(isyncyou_agent::ToolUseRef {
                        id: tool_use_id.clone(),
                        input: serde_json::json!({}),
                    });
                }
            }
        }
        history.push(isyncyou_agent::Message::assistant(text, tool_uses));
        for block in &outcome.normalized_blocks {
            match block {
                NormalizedAssistantBlock::Text { .. } => {}
                NormalizedAssistantBlock::RejectedToolUse { tool_use_id, .. } => {
                    let help = block.recover_rejected_tool_help().map_err(|_| {
                        isyncyou_agent::AgentError::Provider("turn_outcome_unknown".into())
                    })?;
                    history.push(isyncyou_agent::Message::tool(
                        tool_use_id.clone(),
                        help.ok_or_else(|| {
                            isyncyou_agent::AgentError::Provider("turn_outcome_unknown".into())
                        })?,
                    ));
                }
                NormalizedAssistantBlock::ToolUse {
                    tool_use_id,
                    action,
                } => {
                    let binding = isyncyou_agent::ReadExecutionBinding {
                        session_id: runtime.recovery_journal().session_id.clone(),
                        request_id: runtime.recovery_journal().request_id.clone(),
                        tool_use_id: tool_use_id.clone(),
                    };
                    if !runtime
                        .recovery_journal()
                        .read_checkpoints
                        .iter()
                        .any(|checkpoint| {
                            checkpoint.provider_step_seq == outcome.step_seq
                                && checkpoint.tool_use_id == *tool_use_id
                                && checkpoint.action == *action
                        })
                    {
                        if action.recovery_policy() == isyncyou_agent::RecoveryPolicy::NeverRepeat {
                            return Err(isyncyou_agent::AgentError::Provider(
                                "turn_outcome_unknown".into(),
                            ));
                        }
                        let local_effect = executor.prepare_read_effect(action, &binding)?;
                        runtime.persist_read_started(
                            outcome.step_seq,
                            tool_use_id,
                            action,
                            local_effect.as_ref(),
                        )?;
                    }
                    let checkpoint = runtime
                        .recovery_journal()
                        .read_checkpoints
                        .iter()
                        .find(|checkpoint| {
                            checkpoint.provider_step_seq == outcome.step_seq
                                && checkpoint.tool_use_id == *tool_use_id
                                && checkpoint.action == *action
                        })
                        .ok_or_else(|| {
                            isyncyou_agent::AgentError::Provider("turn_outcome_unknown".into())
                        })?
                        .clone();
                    if checkpoint.policy == isyncyou_agent::RecoveryPolicy::NeverRepeat {
                        return Err(isyncyou_agent::AgentError::Provider(
                            "turn_outcome_unknown".into(),
                        ));
                    }
                    let result = executor.execute_read_prepared(
                        action,
                        &binding,
                        checkpoint.local_effect.as_ref(),
                    )?;
                    if !checkpoint.result_sha256.is_empty()
                        && tool_result_digest(result.as_bytes()) != checkpoint.result_sha256
                    {
                        return Err(isyncyou_agent::AgentError::Provider(
                            "turn_outcome_unknown".into(),
                        ));
                    }
                    runtime.persist_read_completed(
                        outcome.step_seq,
                        tool_use_id,
                        action,
                        &result,
                    )?;
                    collect_source_refs(&result, runtime.recovery_sources_mut());
                    history.push(isyncyou_agent::Message::tool(tool_use_id.clone(), result));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
fn recover_provider_messages(
    history: &mut Vec<isyncyou_agent::Message>,
    outcomes: &[RequestStepOutcomeV1],
    journal: &RequestJournalV1,
    executor: &dyn isyncyou_agent::ToolExecutor,
    sources: &mut Vec<SourceRef>,
) -> Result<(), isyncyou_agent::AgentError> {
    struct TestRecoveryRuntime<'a> {
        journal: RequestJournalV1,
        sources: &'a mut Vec<SourceRef>,
    }

    impl RecoveryRuntimeState for TestRecoveryRuntime<'_> {
        fn recovery_journal(&self) -> &RequestJournalV1 {
            &self.journal
        }

        fn recovery_sources_mut(&mut self) -> &mut Vec<SourceRef> {
            self.sources
        }

        fn persist_read_started(
            &mut self,
            step_seq: u8,
            tool_use_id: &str,
            action: &ToolAction,
            local_effect: Option<&LocalEffectCheckpointV1>,
        ) -> Result<(), isyncyou_agent::AgentError> {
            self.journal.read_checkpoints.push(ReadToolCheckpointV1 {
                provider_step_seq: step_seq,
                tool_use_id: tool_use_id.to_owned(),
                action: action.clone(),
                policy: action.recovery_policy(),
                result_sha256: String::new(),
                local_effect: local_effect.cloned(),
            });
            Ok(())
        }

        fn persist_read_completed(
            &mut self,
            step_seq: u8,
            tool_use_id: &str,
            _action: &ToolAction,
            result: &str,
        ) -> Result<(), isyncyou_agent::AgentError> {
            let checkpoint = self
                .journal
                .read_checkpoints
                .iter_mut()
                .find(|checkpoint| {
                    checkpoint.provider_step_seq == step_seq
                        && checkpoint.tool_use_id == tool_use_id
                })
                .ok_or_else(|| {
                    isyncyou_agent::AgentError::Provider("turn_outcome_unknown".into())
                })?;
            checkpoint.result_sha256 = tool_result_digest(result.as_bytes());
            if let Some(effect) = &mut checkpoint.local_effect {
                effect.state = isyncyou_agent::LocalEffectState::Committed;
            }
            Ok(())
        }
    }

    let mut runtime = TestRecoveryRuntime {
        journal: journal.clone(),
        sources,
    };
    recover_provider_messages_runtime(history, outcomes, &mut runtime, executor)
}

impl TurnObserver for ProductTurnRuntime {
    fn next_provider_step(&self) -> u8 {
        self.journal.next_step_seq
    }

    fn read_execution_binding(
        &self,
        tool_use_id: &str,
    ) -> Option<isyncyou_agent::ReadExecutionBinding> {
        Some(isyncyou_agent::ReadExecutionBinding {
            session_id: self.session_id.clone(),
            request_id: self.request_id.clone(),
            tool_use_id: tool_use_id.to_owned(),
        })
    }

    fn provider_step_started(&mut self, step_seq: u8) -> Result<(), isyncyou_agent::AgentError> {
        if step_seq != self.journal.next_step_seq {
            return Err(isyncyou_agent::AgentError::Provider(
                "turn_outcome_unknown".into(),
            ));
        }
        if self.prepublished_provider_step.take() == Some(step_seq) {
            return Ok(());
        }
        self.journal.phase = RequestPhase::ProviderStepStarted;
        self.publish_journal()
            .map_err(isyncyou_agent::AgentError::Provider)
    }

    fn provider_step_completed(
        &mut self,
        step_seq: u8,
        blocks: &[AssistantBlock],
    ) -> Result<(), isyncyou_agent::AgentError> {
        if step_seq != self.journal.next_step_seq {
            return Err(isyncyou_agent::AgentError::Provider(
                "turn_outcome_unknown".into(),
            ));
        }
        let mut step_tool_use_ids = std::collections::BTreeSet::new();
        let invalid_tool_use_id = blocks
            .iter()
            .filter_map(|block| match block {
                AssistantBlock::ToolUse { id, .. } => Some(id),
                AssistantBlock::Text(_) => None,
            })
            .any(|id| {
                id.is_empty()
                    || id.len() > 128
                    || self.seen_tool_use_ids.contains(id)
                    || !step_tool_use_ids.insert(id.clone())
            });
        if invalid_tool_use_id {
            self.persist_provider_step_outcome(
                step_seq,
                Vec::new(),
                None,
                Some("duplicate_tool_use_id".into()),
            )?;
            return Err(isyncyou_agent::AgentError::Provider(
                "duplicate_tool_use_id".into(),
            ));
        }
        self.seen_tool_use_ids.extend(step_tool_use_ids);
        let mut normalized_blocks = Vec::with_capacity(blocks.len());
        let mut final_text = String::new();
        for block in blocks {
            match block {
                AssistantBlock::Text(text) => {
                    final_text.push_str(text);
                    normalized_blocks.push(NormalizedAssistantBlock::Text { text: text.clone() });
                }
                AssistantBlock::ToolUse { id, input } => match parse_action(input) {
                    Ok(action) => normalized_blocks.push(NormalizedAssistantBlock::ToolUse {
                        tool_use_id: id.clone(),
                        action,
                    }),
                    Err(help) => {
                        normalized_blocks.push(NormalizedAssistantBlock::RejectedToolUse {
                            tool_use_id: id.clone(),
                            stable_error_code: isyncyou_agent::tool::INVALID_TOOL_ARGUMENTS_CODE
                                .into(),
                            help_schema_version:
                                isyncyou_agent::tool::REJECTED_TOOL_HELP_SCHEMA_VERSION,
                            help_digest: tool_result_digest(help.as_bytes()),
                        })
                    }
                },
            }
        }
        self.persist_provider_step_outcome(
            step_seq,
            normalized_blocks,
            (!final_text.is_empty()).then_some(final_text),
            None,
        )
    }

    fn read_tool_started(
        &mut self,
        step_seq: u8,
        tool_use_id: &str,
        action: &ToolAction,
        local_effect: Option<&LocalEffectCheckpointV1>,
    ) -> Result<(), isyncyou_agent::AgentError> {
        if self.journal.read_checkpoints.len() >= isyncyou_agent::MAX_TOOL_CHECKPOINTS
            || self.journal.read_checkpoints.iter().any(|checkpoint| {
                checkpoint.provider_step_seq == step_seq && checkpoint.tool_use_id == tool_use_id
            })
            || action.recovery_policy() == isyncyou_agent::RecoveryPolicy::NeverRepeat
        {
            return Err(isyncyou_agent::AgentError::Provider(
                "turn_outcome_unknown".into(),
            ));
        }
        self.journal.read_checkpoints.push(ReadToolCheckpointV1 {
            provider_step_seq: step_seq,
            tool_use_id: tool_use_id.to_owned(),
            action: action.clone(),
            policy: action.recovery_policy(),
            result_sha256: String::new(),
            local_effect: local_effect.cloned(),
        });
        Ok(())
    }

    fn read_tool_completed(
        &mut self,
        step_seq: u8,
        tool_use_id: &str,
        _action: &ToolAction,
        result: &str,
    ) -> Result<(), isyncyou_agent::AgentError> {
        let checkpoint = self
            .journal
            .read_checkpoints
            .iter_mut()
            .rev()
            .find(|checkpoint| {
                checkpoint.provider_step_seq == step_seq && checkpoint.tool_use_id == tool_use_id
            })
            .ok_or_else(|| isyncyou_agent::AgentError::Provider("turn_outcome_unknown".into()))?;
        checkpoint.result_sha256 = tool_result_digest(result.as_bytes());
        if let Some(local_effect) = &mut checkpoint.local_effect {
            local_effect.state = isyncyou_agent::LocalEffectState::Committed;
        }
        collect_source_refs(result, &mut self.sources);
        Ok(())
    }
}

impl<'a> ProductSessionRegistry<'a> {
    pub fn new(store: &'a AgentCredentialStore) -> Self {
        Self { store }
    }

    fn lock_index_rmw(
        &self,
    ) -> Result<(std::sync::MutexGuard<'static, ()>, isyncyou_agent::FileLock), String> {
        let process_guard = product_session_index_gate()
            .lock()
            .map_err(|_| "session_store_unavailable".to_string())?;
        let file_guard = isyncyou_agent::FileLock::try_acquire_exclusive(
            &self.store.store_dir().join(".product-session-index.lock"),
        )
        .map_err(|_| "session_store_unavailable".to_string())?
        .ok_or_else(|| "session_store_busy".to_string())?;
        Ok((process_guard, file_guard))
    }

    pub fn create(
        &self,
        local_account: &str,
        request_id: &str,
        display_name: Option<&str>,
        created_at_ms: u64,
    ) -> Result<ProductSessionSummaryV1, String> {
        let _index_guard = self.lock_index_rmw()?;
        let display_name = normalize_display_name(display_name)?;
        let mut index = self.load_index()?;
        let digest =
            payload_digest(&(local_account, display_name.as_deref())).map_err(map_session_error)?;
        if let Some(receipt) = index
            .request_receipts
            .iter()
            .find(|receipt| receipt.request_id == request_id)
        {
            if receipt.route != "create" || receipt.payload_digest != digest {
                return Err("request_id_conflict".into());
            }
            return index
                .sessions
                .iter()
                .find(|session| session.session_id == receipt.session_id)
                .map(ProductSessionStoredSummaryV1::public)
                .ok_or_else(|| "session_store_unavailable".into());
        }
        if index.sessions.len() >= MAX_SESSIONS {
            return Err("session_limit_reached".into());
        }
        if index.request_receipts.len() >= MAX_REQUEST_RECEIPTS {
            return Err("session_request_capacity_reached".into());
        }
        let session_id = new_ulid().map_err(|_| "session_store_unavailable")?;
        let payload = PairingPayload::generate(
            SessionId::new(&session_id).map_err(|_| "session_store_unavailable")?,
        )
        .map_err(|_| "session_store_unavailable")?;
        self.store
            .put_bounded(
                SecretClass::SessionPairingKey,
                &session_secret_id(&session_id),
                &Secret::new(
                    payload
                        .encode()
                        .map_err(|_| "session_store_unavailable")?
                        .into_bytes(),
                ),
                MAX_SESSION_SECRET_ENVELOPE_BYTES,
            )
            .map_err(|_| "session_store_unavailable")?;
        let summary = ProductSessionStoredSummaryV1 {
            session_id: session_id.clone(),
            display_name,
            created_at_ms,
            archived: false,
            local_account: local_account.to_owned(),
            remote_initialized: Some(false),
        };
        index.sessions.push(summary.clone());
        index.request_receipts.push(ProductSessionRequestReceiptV1 {
            request_id: request_id.to_owned(),
            route: "create".into(),
            payload_digest: digest,
            session_id: session_id.clone(),
        });
        index.selected_session_id = Some(session_id);
        index.generation = index
            .generation
            .checked_add(1)
            .ok_or_else(|| "session_store_unavailable".to_string())?;
        self.save_index(&index)?;
        Ok(summary.public())
    }

    pub fn list_page(
        &self,
        local_account: &str,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> Result<ProductSessionPageV1, String> {
        let index = self.load_index()?;
        let limit = limit.unwrap_or(DEFAULT_PAGE_SIZE);
        if limit == 0 || limit > MAX_PAGE_SIZE {
            return Err("invalid_cursor".into());
        }
        let offset = cursor
            .map(|cursor| self.decode_cursor(cursor, index.generation))
            .transpose()?
            .unwrap_or(0);
        let mut sessions = index
            .sessions
            .iter()
            .filter(|session| session.local_account == local_account)
            .cloned()
            .collect::<Vec<_>>();
        sessions
            .sort_by_key(|session| (session.archived, std::cmp::Reverse(session.created_at_ms)));
        if offset > sessions.len() {
            return Err("invalid_cursor".into());
        }
        let selected_session_id = index.selected_session_id.filter(|selected| {
            sessions
                .iter()
                .any(|session| session.session_id == *selected)
        });
        let total = sessions.len();
        let sessions = sessions
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|session| session.public())
            .collect::<Vec<_>>();
        let next_offset = offset + sessions.len();
        let next_cursor = (next_offset < total)
            .then(|| self.encode_cursor(next_offset, index.generation))
            .transpose()?;
        Ok(ProductSessionPageV1 {
            sessions,
            selected_session_id,
            next_cursor,
        })
    }

    pub fn select(&self, request_id: &str, session_id: &str) -> Result<(), String> {
        let _index_guard = self.lock_index_rmw()?;
        let mut index = self.load_index()?;
        let digest = payload_digest(&session_id).map_err(map_session_error)?;
        if let Some(receipt) = index
            .request_receipts
            .iter()
            .find(|receipt| receipt.request_id == request_id)
        {
            return if receipt.route == "select"
                && receipt.payload_digest == digest
                && receipt.session_id == session_id
            {
                Ok(())
            } else {
                Err("request_id_conflict".into())
            };
        }
        if index.request_receipts.len() >= MAX_REQUEST_RECEIPTS {
            return Err("session_request_capacity_reached".into());
        }
        let selectable = index
            .sessions
            .iter()
            .any(|session| session.session_id == session_id && !session.archived);
        if !selectable {
            return Err("session_not_found".into());
        }
        self.load_payload(session_id)?;
        index.selected_session_id = Some(session_id.to_owned());
        index.request_receipts.push(ProductSessionRequestReceiptV1 {
            request_id: request_id.to_owned(),
            route: "select".into(),
            payload_digest: digest,
            session_id: session_id.to_owned(),
        });
        index.generation = index
            .generation
            .checked_add(1)
            .ok_or_else(|| "session_store_unavailable".to_string())?;
        self.save_index(&index)
    }

    pub fn archive(
        &self,
        graph_token: &str,
        request_id: &str,
        session_id: &str,
    ) -> Result<ProductSessionSummaryV1, String> {
        let _index_guard = self.lock_index_rmw()?;
        let mut index = self.load_index()?;
        let digest = payload_digest(&session_id).map_err(map_session_error)?;
        if let Some(receipt) = index
            .request_receipts
            .iter()
            .find(|receipt| receipt.request_id == request_id)
        {
            if receipt.route != "archive"
                || receipt.payload_digest != digest
                || receipt.session_id != session_id
            {
                return Err("request_id_conflict".into());
            }
            return index
                .sessions
                .iter()
                .find(|session| session.session_id == session_id && session.archived)
                .map(ProductSessionStoredSummaryV1::public)
                .ok_or_else(|| "session_store_unavailable".into());
        }
        if index.request_receipts.len() >= MAX_REQUEST_RECEIPTS {
            return Err("session_request_capacity_reached".into());
        }
        let position = index
            .sessions
            .iter()
            .position(|session| session.session_id == session_id)
            .ok_or_else(|| "session_not_found".to_string())?;
        if !index.sessions[position].archived {
            let store = self.open(graph_token, session_id)?;
            store
                .archive_session(session_id)
                .map_err(map_session_error)?;
            index.sessions[position].archived = true;
        }
        if index.selected_session_id.as_deref() == Some(session_id) {
            index.selected_session_id = None;
        }
        index.request_receipts.push(ProductSessionRequestReceiptV1 {
            request_id: request_id.to_owned(),
            route: "archive".into(),
            payload_digest: digest,
            session_id: session_id.to_owned(),
        });
        index.generation = index
            .generation
            .checked_add(1)
            .ok_or_else(|| "session_store_unavailable".to_string())?;
        self.save_index(&index)?;
        Ok(index.sessions[position].public())
    }

    pub fn import_pairing_payload(
        &self,
        request_id: &str,
        local_account: &str,
        payload: &PairingPayload,
        created_at_ms: u64,
    ) -> Result<ProductSessionSummaryV1, String> {
        let _index_guard = self.lock_index_rmw()?;
        let session_id = payload.session_id.as_str();
        let mut index = self.load_index()?;
        let digest = payload_digest(&(local_account, session_id)).map_err(map_session_error)?;
        if let Some(receipt) = index
            .request_receipts
            .iter()
            .find(|receipt| receipt.request_id == request_id)
        {
            if receipt.route != "import"
                || receipt.payload_digest != digest
                || receipt.session_id != session_id
            {
                return Err("request_id_conflict".into());
            }
            return index
                .sessions
                .iter()
                .find(|session| session.session_id == session_id)
                .map(ProductSessionStoredSummaryV1::public)
                .ok_or_else(|| "session_store_unavailable".into());
        }
        if index.request_receipts.len() >= MAX_REQUEST_RECEIPTS {
            return Err("session_request_capacity_reached".into());
        }
        if let Some(existing) = index
            .sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .cloned()
        {
            if existing.local_account != local_account {
                return Err("session_account_mismatch".into());
            }
            index.selected_session_id = Some(session_id.to_owned());
            index.request_receipts.push(ProductSessionRequestReceiptV1 {
                request_id: request_id.to_owned(),
                route: "import".into(),
                payload_digest: digest,
                session_id: session_id.to_owned(),
            });
            index.generation = index
                .generation
                .checked_add(1)
                .ok_or_else(|| "session_store_unavailable".to_string())?;
            self.save_index(&index)?;
            return Ok(existing.public());
        }
        if index.sessions.len() >= MAX_SESSIONS {
            return Err("session_limit_reached".into());
        }
        self.store
            .put_bounded(
                SecretClass::SessionPairingKey,
                &session_secret_id(session_id),
                &Secret::new(
                    payload
                        .encode()
                        .map_err(|_| "session_store_unavailable")?
                        .into_bytes(),
                ),
                MAX_SESSION_SECRET_ENVELOPE_BYTES,
            )
            .map_err(|_| "session_store_unavailable")?;
        let summary = ProductSessionStoredSummaryV1 {
            session_id: session_id.to_owned(),
            display_name: None,
            created_at_ms,
            archived: false,
            local_account: local_account.to_owned(),
            // Pairing reveal ensures the source manifest exists before exposing
            // the transfer code, so an imported session can read it immediately.
            remote_initialized: Some(true),
        };
        index.sessions.push(summary.clone());
        index.selected_session_id = Some(session_id.to_owned());
        index.request_receipts.push(ProductSessionRequestReceiptV1 {
            request_id: request_id.to_owned(),
            route: "import".into(),
            payload_digest: digest,
            session_id: session_id.to_owned(),
        });
        index.generation = index
            .generation
            .checked_add(1)
            .ok_or_else(|| "session_store_unavailable".to_string())?;
        self.save_index(&index)?;
        Ok(summary.public())
    }

    pub fn account_for(&self, session_id: &str) -> Result<String, String> {
        self.load_index()?
            .sessions
            .into_iter()
            .find(|session| session.session_id == session_id && !session.archived)
            .map(|session| session.local_account)
            .ok_or_else(|| "session_not_found".into())
    }

    pub fn open(
        &self,
        graph_token: &str,
        session_id: &str,
    ) -> Result<SessionV2Store<OneDriveSessionV2Transport>, String> {
        let transport =
            OneDriveSessionV2Transport::new(graph_token, session_id).map_err(map_session_error)?;
        self.open_with_transport(session_id, transport)
    }

    fn open_with_transport(
        &self,
        session_id: &str,
        transport: OneDriveSessionV2Transport,
    ) -> Result<SessionV2Store<OneDriveSessionV2Transport>, String> {
        let index = self.load_index()?;
        if !index
            .sessions
            .iter()
            .any(|session| session.session_id == session_id && !session.archived)
        {
            return Err("session_not_found".into());
        }
        let payload = self.load_payload(session_id)?;
        let object_crypto = SessionObjectCrypto::new(
            payload.pairing_secret(),
            payload
                .crypto_config()
                .map_err(|_| "session_store_unavailable")?,
        )
        .map_err(|_| "session_store_unavailable")?;
        transport
            .bind_manifest_crypto(object_crypto.clone())
            .map_err(map_session_error)?;
        let cursor_key = self
            .store
            .domain_hmac(CURSOR_HMAC_DOMAIN, session_id.as_bytes())
            .map_err(|_| "session_store_unavailable")?;
        Ok(SessionV2Store::new(transport, &cursor_key, object_crypto))
    }

    pub fn remote_initialization_pending(&self, session_id: &str) -> Result<bool, String> {
        self.load_index()?
            .sessions
            .into_iter()
            .find(|session| session.session_id == session_id && !session.archived)
            .map(|session| session.remote_initialized == Some(false))
            .ok_or_else(|| "session_not_found".into())
    }

    pub fn ensure_remote_session(
        &self,
        graph_token: &str,
        session_id: &str,
    ) -> Result<SessionV2Store<OneDriveSessionV2Transport>, String> {
        let transport =
            OneDriveSessionV2Transport::new(graph_token, session_id).map_err(map_session_error)?;
        self.ensure_remote_session_with_transport(session_id, transport)
    }

    fn ensure_remote_session_for_background_turn(
        &self,
        graph_token: &str,
        session_id: &str,
    ) -> Result<SessionV2Store<OneDriveSessionV2Transport>, String> {
        let transport =
            OneDriveSessionV2Transport::new(graph_token, session_id).map_err(map_session_error)?;
        // The route has already reserved the deterministic turn ID and detached
        // this worker from the synchronous response path. Keep the normal bounded
        // Graph request timeouts, but do not apply one aggregate interactive
        // deadline across manifest, history, and CAS work.
        transport
            .complete_interactive_admission()
            .map_err(map_session_error)?;
        self.ensure_remote_session_with_transport(session_id, transport)
    }

    fn ensure_remote_session_with_transport(
        &self,
        session_id: &str,
        transport: OneDriveSessionV2Transport,
    ) -> Result<SessionV2Store<OneDriveSessionV2Transport>, String> {
        let session = self.open_with_transport(session_id, transport.clone())?;
        if !self.remote_initialization_pending(session_id)? {
            return Ok(session);
        }
        transport.create_session().map_err(map_session_error)?;
        let _index_guard = self.lock_index_rmw()?;
        let mut index = self.load_index()?;
        let stored = index
            .sessions
            .iter_mut()
            .find(|session| session.session_id == session_id && !session.archived)
            .ok_or_else(|| "session_not_found".to_string())?;
        stored.remote_initialized = Some(true);
        index.generation = index
            .generation
            .checked_add(1)
            .ok_or_else(|| "session_store_unavailable".to_string())?;
        self.save_index(&index)?;
        Ok(session)
    }

    pub fn append_pending_cancelled(
        &self,
        graph_token: &str,
        owner: &PendingOwnerBinding,
        installation_principal: &str,
        created_at_ms: u64,
    ) -> Result<(), String> {
        if owner.session_id == "legacy-local" {
            return Ok(());
        }
        let store = self.open(graph_token, &owner.session_id)?;
        let records = store
            .recent_visible_records(&owner.session_id, 128)
            .map_err(map_session_error)?;
        if records.iter().any(|record| {
            record.request_id == owner.request_id
                && record.turn_id == owner.turn_id
                && matches!(
                    &record.kind,
                    SessionRecordKind::OperationState { code } if code == "cancelled"
                )
        }) {
            return Ok(());
        }
        let pending_record_id = records
            .iter()
            .rev()
            .find(|record| {
                record.request_id == owner.request_id
                    && record.turn_id == owner.turn_id
                    && matches!(&record.kind, SessionRecordKind::PendingOperation { .. })
            })
            .map(|record| record.record_id.clone())
            .ok_or_else(|| "session_state_invalid".to_string())?;
        let lease_id = new_ulid().map_err(|_| "session_store_unavailable")?;
        let holder_binding = self
            .store
            .domain_hmac(
                TURN_HOLDER_DOMAIN,
                format!("{installation_principal}:{}", owner.session_id).as_bytes(),
            )
            .map(|bytes| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
            .map_err(|_| "session_store_unavailable")?;
        let guard = store
            .acquire_lease(&owner.session_id, &lease_id, &holder_binding)
            .map_err(map_session_error)?;
        let record_id = new_ulid().map_err(|_| "session_store_unavailable")?;
        let lease = guard.binding().map_err(map_session_error)?;
        guard
            .publish(SessionCommitV1 {
                visible_records: vec![pending_cancelled_record(
                    owner,
                    record_id,
                    pending_record_id,
                    lease,
                    created_at_ms,
                )],
                request_objects: vec![],
                uuid_bindings: vec![],
            })
            .map_err(map_session_error)
    }

    pub fn begin_turn(&self, request: ProductTurnRequest<'_>) -> Result<ProductTurnStart, String> {
        let ProductTurnRequest {
            graph_token,
            session_id,
            request_id,
            turn_id,
            local_account,
            prompt,
            provider_binding,
            installation_principal,
            created_at_ms,
            cached_context,
        } = request;
        // This runs only in the admission worker. The route has already returned
        // the deterministic turn ID, so Graph manifest creation cannot delay the
        // user's acknowledgement of the turn.
        let store = self.ensure_remote_session_for_background_turn(graph_token, session_id)?;
        let (current, admission_time) = store
            .current_manifest_with_server_time(session_id)
            .map_err(map_session_error)?;
        let context_records = match cached_context {
            Some(snapshot) if snapshot.manifest_generation == current.manifest.generation => {
                snapshot.records
            }
            _ => store
                .recent_visible_records_from_manifest(&current, 128)
                .map_err(map_session_error)?,
        }
        .into_iter()
        .filter(|record| record.request_id != request_id)
        .collect::<Vec<_>>();
        let provider_history = select_provider_context(
            &context_records,
            None,
            &isyncyou_agent::ContextBudget::default(),
        )
        .into_iter()
        .map(|message| match message.role {
            "assistant" => isyncyou_agent::Message::assistant(message.text, vec![]),
            _ => isyncyou_agent::Message::user(message.text),
        })
        .collect();
        let request_binding = RequestUuidBindingV1::new(
            RequestRouteDomain::AgentTurn,
            session_id,
            request_id,
            payload_digest(&(session_id, local_account, prompt)).map_err(map_session_error)?,
        )
        .map_err(map_session_error)?;
        if let Some(replay) = store
            .request_replay_from_manifest(&current, &request_binding)
            .map_err(map_session_error)?
        {
            let mut final_text = None;
            let mut error_code = None;
            let mut intent_record = None;
            for record in replay.visible_records {
                match &record.kind {
                    SessionRecordKind::TurnIntent { .. } => intent_record = Some(record.clone()),
                    SessionRecordKind::AssistantResult { text, .. } => {
                        final_text = Some(text.clone())
                    }
                    SessionRecordKind::TurnTerminal {
                        error_code: code, ..
                    } => error_code.clone_from(code),
                    _ => {}
                }
            }
            if let Some(tombstone) = replay.tombstone {
                let phase = match tombstone.terminal_status {
                    TurnTerminalStatus::Complete => RequestPhase::Committed,
                    TurnTerminalStatus::Cancelled => RequestPhase::Cancelled,
                    TurnTerminalStatus::Error | TurnTerminalStatus::OutcomeUnknown => {
                        RequestPhase::Failed
                    }
                };
                return Ok(ProductTurnStart::Replay(ProductTurnReplay {
                    phase,
                    final_text,
                    error_code,
                }));
            }
            let journal = replay
                .journal
                .ok_or_else(|| "session_store_unavailable".to_string())?;
            validate_provider_recovery_binding(&journal.provider_binding, &provider_binding)?;
            if journal.phase.permits_automatic_resume() {
                let lease_id = new_ulid().map_err(|_| "session_store_unavailable")?;
                let holder_binding = self
                    .store
                    .domain_hmac(
                        TURN_HOLDER_DOMAIN,
                        format!("{installation_principal}:{session_id}").as_bytes(),
                    )
                    .map(|bytes| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
                    .map_err(|_| "session_store_unavailable")?;
                let guard = store
                    .acquire_lease_from_manifest(current.clone(), &lease_id, &holder_binding)
                    .map_err(map_session_error)?;
                let intent_record =
                    intent_record.ok_or_else(|| "session_store_unavailable".to_string())?;
                let runtime = ProductTurnRuntime {
                    guard,
                    session_id: session_id.to_owned(),
                    request_id: request_id.to_owned(),
                    turn_id: journal.turn_id.clone(),
                    request_binding: request_binding.clone(),
                    provider_binding,
                    intent_record_id: intent_record.record_id.clone(),
                    intent_record,
                    context_records: context_records.clone(),
                    journal: journal.clone(),
                    pending_request_objects: Vec::new(),
                    prepublished_provider_step: None,
                    seen_tool_use_ids: replay
                        .outcomes
                        .iter()
                        .flat_map(|outcome| &outcome.normalized_blocks)
                        .filter_map(|block| match block {
                            NormalizedAssistantBlock::ToolUse { tool_use_id, .. }
                            | NormalizedAssistantBlock::RejectedToolUse { tool_use_id, .. } => {
                                Some(tool_use_id.clone())
                            }
                            NormalizedAssistantBlock::Text { .. } => None,
                        })
                        .collect(),
                    provider_history,
                    recovery_outcomes: replay.outcomes,
                    sources: Vec::new(),
                };
                if runtime.journal.phase == RequestPhase::ProviderStepCompleted
                    && runtime.recovery_outcomes.last().is_some_and(|outcome| {
                        outcome
                            .normalized_blocks
                            .iter()
                            .all(|block| matches!(block, NormalizedAssistantBlock::Text { .. }))
                    })
                {
                    let text = runtime
                        .recovery_outcomes
                        .last()
                        .and_then(|outcome| outcome.final_text.clone())
                        .unwrap_or_default();
                    runtime.finish_final(text.clone(), None, created_at_ms)?;
                    return Ok(ProductTurnStart::Replay(ProductTurnReplay {
                        phase: RequestPhase::Committed,
                        final_text: Some(text),
                        error_code: None,
                    }));
                }
                runtime.guard.finish_admission();
                return Ok(ProductTurnStart::Started(Box::new(runtime)));
            }
            return Ok(ProductTurnStart::Replay(ProductTurnReplay {
                phase: journal.phase.recovery_phase(),
                final_text,
                error_code,
            }));
        }
        let lease_id = new_ulid().map_err(|_| "session_store_unavailable")?;
        let holder_binding = self
            .store
            .domain_hmac(
                TURN_HOLDER_DOMAIN,
                format!("{installation_principal}:{session_id}").as_bytes(),
            )
            .map(|bytes| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
            .map_err(|_| "session_store_unavailable")?;
        if turn_id.is_empty() || turn_id.len() > 128 {
            return Err("session_store_unavailable".into());
        }
        let turn_id = turn_id.to_owned();
        let intent_record_id = new_ulid().map_err(|_| "session_store_unavailable")?;
        let journal_id = new_ulid().map_err(|_| "session_store_unavailable")?;
        let journal = RequestJournalV1 {
            journal_version: REQUEST_JOURNAL_VERSION,
            session_id: session_id.to_owned(),
            request_id: request_id.to_owned(),
            turn_id: turn_id.clone(),
            provider_binding: provider_binding.clone(),
            // Publish the first provider-start marker with admission. This removes a second
            // manifest round trip before the first model request while retaining the
            // conservative crash boundary: a restart never repeats this provider step.
            phase: RequestPhase::ProviderStepStarted,
            next_step_seq: 0,
            completed_steps: vec![],
            read_checkpoints: vec![],
        };
        let journal_bytes =
            serde_json::to_vec(&journal).map_err(|_| "session_store_unavailable")?;
        let intent_record = SessionRecordV2 {
            record_version: SESSION_RECORD_VERSION,
            record_id: intent_record_id.clone(),
            session_id: session_id.to_owned(),
            request_id: request_id.to_owned(),
            turn_id: turn_id.clone(),
            kind: SessionRecordKind::TurnIntent {
                user_text: prompt.to_owned(),
            },
            parent_record_ids: vec![],
            observed_head: None,
            lease: PersistedLeaseBinding {
                lease_id: String::new(),
                holder_binding: String::new(),
                fence: 0,
                expires_at_server_ms: 0,
            },
            created_at_ms,
        };
        let guard = store
            .acquire_lease_and_publish_from_manifest_at(
                current,
                admission_time,
                &lease_id,
                &holder_binding,
                |lease| {
                    let mut intent_record = intent_record.clone();
                    intent_record.lease = lease.clone();
                    Ok(SessionCommitV1 {
                        visible_records: vec![intent_record],
                        request_objects: vec![(journal_id, journal_bytes)],
                        uuid_bindings: vec![request_binding.clone()],
                    })
                },
            )
            .map_err(map_session_error)?;
        let mut intent_record = intent_record;
        intent_record.lease = guard.binding().map_err(map_session_error)?;
        guard.finish_admission();
        Ok(ProductTurnStart::Started(Box::new(ProductTurnRuntime {
            guard,
            session_id: session_id.to_owned(),
            request_id: request_id.to_owned(),
            request_binding,
            turn_id,
            provider_binding,
            intent_record_id,
            intent_record,
            context_records,
            journal,
            pending_request_objects: Vec::new(),
            prepublished_provider_step: Some(0),
            seen_tool_use_ids: std::collections::BTreeSet::new(),
            provider_history,
            recovery_outcomes: Vec::new(),
            sources: Vec::new(),
        })))
    }

    pub(crate) fn pairing_payload(&self, session_id: &str) -> Result<PairingPayload, String> {
        let secret = self
            .store
            .get(
                SecretClass::SessionPairingKey,
                &session_secret_id(session_id),
            )
            .map_err(|_| "session_store_unavailable")?
            .ok_or_else(|| "session_store_unavailable".to_string())?;
        let encoded =
            std::str::from_utf8(secret.expose()).map_err(|_| "session_store_unavailable")?;
        let payload = PairingPayload::parse(encoded).map_err(|_| "session_store_unavailable")?;
        if payload.session_id.as_str() != session_id {
            return Err("session_store_unavailable".into());
        }
        Ok(payload)
    }

    fn load_payload(&self, session_id: &str) -> Result<PairingPayload, String> {
        self.pairing_payload(session_id)
    }

    fn load_index(&self) -> Result<ProductSessionIndexV1, String> {
        let Some(secret) = self
            .store
            .get(SecretClass::SessionPairingKey, SESSION_INDEX_ID)
            .map_err(|_| "session_store_unavailable")?
        else {
            return Ok(ProductSessionIndexV1::default());
        };
        let index: ProductSessionIndexV1 =
            serde_json::from_slice(secret.expose()).map_err(|_| "session_store_unavailable")?;
        index.validate()?;
        Ok(index)
    }

    fn save_index(&self, index: &ProductSessionIndexV1) -> Result<(), String> {
        index.validate()?;
        let bytes = serde_json::to_vec(index).map_err(|_| "session_store_unavailable")?;
        self.store
            .put_bounded(
                SecretClass::SessionPairingKey,
                SESSION_INDEX_ID,
                &Secret::new(bytes),
                MAX_INDEX_ENVELOPE_BYTES,
            )
            .map_err(|_| "session_store_unavailable".to_string())
    }

    fn encode_cursor(&self, offset: usize, generation: u64) -> Result<String, String> {
        let offset = u64::try_from(offset).map_err(|_| "invalid_cursor")?;
        let mut payload = Vec::with_capacity(48);
        payload.extend_from_slice(&generation.to_be_bytes());
        payload.extend_from_slice(&offset.to_be_bytes());
        payload.extend_from_slice(
            &self
                .store
                .domain_hmac(CURSOR_HMAC_DOMAIN, &payload)
                .map_err(|_| "session_store_unavailable")?,
        );
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload))
    }

    fn decode_cursor(&self, cursor: &str, expected_generation: u64) -> Result<usize, String> {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(cursor)
            .map_err(|_| "invalid_cursor")?;
        if bytes.len() != 48 {
            return Err("invalid_cursor".into());
        }
        let payload = &bytes[..16];
        let expected = self
            .store
            .domain_hmac(CURSOR_HMAC_DOMAIN, payload)
            .map_err(|_| "session_store_unavailable")?;
        if !constant_time_eq(&expected, &bytes[16..]) {
            return Err("invalid_cursor".into());
        }
        let generation = u64::from_be_bytes(payload[..8].try_into().map_err(|_| "invalid_cursor")?);
        if generation != expected_generation {
            return Err("stale_cursor".into());
        }
        let offset = u64::from_be_bytes(payload[8..].try_into().map_err(|_| "invalid_cursor")?);
        usize::try_from(offset).map_err(|_| "invalid_cursor".into())
    }
}

fn pending_cancelled_record(
    owner: &PendingOwnerBinding,
    record_id: String,
    pending_record_id: String,
    lease: isyncyou_agent::PersistedLeaseBinding,
    created_at_ms: u64,
) -> SessionRecordV2 {
    SessionRecordV2 {
        record_version: SESSION_RECORD_VERSION,
        record_id,
        session_id: owner.session_id.clone(),
        request_id: owner.request_id.clone(),
        turn_id: owner.turn_id.clone(),
        kind: SessionRecordKind::OperationState {
            code: "cancelled".into(),
        },
        parent_record_ids: vec![pending_record_id.clone()],
        observed_head: Some(pending_record_id),
        lease,
        created_at_ms,
    }
}

fn collect_source_refs(result: &str, output: &mut Vec<SourceRef>) {
    fn visit(value: &serde_json::Value, output: &mut Vec<SourceRef>) {
        if output.len() >= 64 {
            return;
        }
        match value {
            serde_json::Value::Array(values) => {
                for value in values {
                    visit(value, output);
                    if output.len() >= 64 {
                        break;
                    }
                }
            }
            serde_json::Value::Object(object) => {
                let source = object
                    .get("source")
                    .and_then(serde_json::Value::as_object)
                    .unwrap_or(object);
                let service = source.get("service").and_then(serde_json::Value::as_str);
                let item_id = source
                    .get("id")
                    .or_else(|| source.get("item_id"))
                    .and_then(serde_json::Value::as_str);
                if let (Some(service), Some(item_id)) = (service, item_id) {
                    let candidate = SourceRef {
                        service: service.to_owned(),
                        item_id: item_id.to_owned(),
                        label: object
                            .get("name")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned),
                    };
                    if service.len() <= 64
                        && item_id.len() <= 512
                        && serde_json::to_vec(&candidate).is_ok_and(|bytes| bytes.len() <= 2 * 1024)
                        && !output.iter().any(|existing| {
                            existing.service == candidate.service
                                && existing.item_id == candidate.item_id
                        })
                    {
                        output.push(candidate);
                    }
                }
                for value in object.values() {
                    visit(value, output);
                    if output.len() >= 64 {
                        break;
                    }
                }
            }
            _ => {}
        }
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(result) {
        visit(&value, output);
    }
}

fn normalize_display_name(display_name: Option<&str>) -> Result<Option<String>, String> {
    let Some(display_name) = display_name else {
        return Ok(None);
    };
    let display_name = display_name.trim();
    if display_name.is_empty() || display_name.len() > MAX_DISPLAY_NAME_BYTES {
        return Err("invalid_session_name".into());
    }
    Ok(Some(display_name.to_owned()))
}

fn session_secret_id(session_id: &str) -> String {
    format!("{SESSION_SECRET_PREFIX}{session_id}")
}

pub(crate) fn map_session_error(error: SessionV2Error) -> String {
    error.to_string()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RecoveryExecutor {
        result: String,
        calls: AtomicUsize,
    }

    impl isyncyou_agent::ToolExecutor for RecoveryExecutor {
        fn execute_read(&self, _action: &ToolAction) -> Result<String, isyncyou_agent::AgentError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.result.clone())
        }
    }

    fn recovery_binding() -> ProviderAttemptBindingV1 {
        ProviderAttemptBindingV1 {
            provider: isyncyou_agent::ProductProviderId::Codex,
            model: "model-a".into(),
            reasoning_effort: Some("medium".into()),
            credential_generation: "generation-a".into(),
            oauth_policy_fingerprint: "policy-a".into(),
            harness_contract_version: 1,
            origin_installation_digest: "installation-a".into(),
        }
    }

    fn read_action(id: &str) -> ToolAction {
        ToolAction::Read {
            account: "me".into(),
            service: "onedrive".into(),
            id: id.into(),
            max_bytes: Some(1024),
        }
    }

    fn recovery_outcome(
        binding: &ProviderAttemptBindingV1,
        step_seq: u8,
        previous_outcome_id: Option<String>,
        tool_use_id: &str,
        action: ToolAction,
    ) -> RequestStepOutcomeV1 {
        RequestStepOutcomeV1 {
            outcome_version: 1,
            outcome_id: new_ulid().unwrap(),
            step_seq,
            previous_outcome_id,
            provider: binding.provider,
            model: binding.model.clone(),
            normalized_blocks: vec![NormalizedAssistantBlock::ToolUse {
                tool_use_id: tool_use_id.into(),
                action,
            }],
            final_text: None,
            sanitized_usage: None,
            terminal_validation_error: None,
            outcome_digest: String::new(),
        }
        .seal_digest()
        .unwrap()
    }

    fn recovery_journal(
        binding: ProviderAttemptBindingV1,
        checkpoints: Vec<ReadToolCheckpointV1>,
        next_step_seq: u8,
    ) -> RequestJournalV1 {
        RequestJournalV1 {
            journal_version: REQUEST_JOURNAL_VERSION,
            session_id: "01J00000000000000000000000".into(),
            request_id: "123e4567-e89b-42d3-a456-426614174000".into(),
            turn_id: "01J00000000000000000000001".into(),
            provider_binding: binding,
            phase: RequestPhase::ProviderStepCompleted,
            next_step_seq,
            completed_steps: vec![],
            read_checkpoints: checkpoints,
        }
    }

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "isyncyou-628-session-{label}-{}-{}",
            std::process::id(),
            new_ulid().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn credential_store(root: &Path) -> AgentCredentialStore {
        let config = isyncyou_agent::CredentialStoreConfig::new(root);
        isyncyou_agent::CredentialStoreResolver::new(config)
            .with_provided_key([42u8; 32])
            .resolve()
            .unwrap()
    }

    fn insert_session(
        registry: &ProductSessionRegistry<'_>,
        session_id: &str,
        account: &str,
        created_at_ms: u64,
    ) {
        let payload = PairingPayload::generate(SessionId::new(session_id).unwrap()).unwrap();
        registry
            .store
            .put_bounded(
                SecretClass::SessionPairingKey,
                &session_secret_id(session_id),
                &Secret::new(payload.encode().unwrap().into_bytes()),
                MAX_SESSION_SECRET_ENVELOPE_BYTES,
            )
            .unwrap();
        let mut index = registry.load_index().unwrap();
        index.sessions.push(ProductSessionStoredSummaryV1 {
            session_id: session_id.into(),
            display_name: Some(format!("Session {created_at_ms}")),
            created_at_ms,
            archived: false,
            local_account: account.into(),
            remote_initialized: None,
        });
        index.selected_session_id = Some(session_id.into());
        index.generation += 1;
        registry.save_index(&index).unwrap();
    }

    #[test]
    fn product_session_index_rmw_serializes_concurrent_creates_without_lost_update() {
        let root = temp_root("concurrent-create");
        let store = credential_store(&root);
        let first_registry = ProductSessionRegistry::new(&store);
        let second_registry = ProductSessionRegistry::new(&store);
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                first_registry
                    .create(
                        "controlled",
                        "019f0000-0000-4000-8000-000000000401",
                        Some("First"),
                        1,
                    )
                    .unwrap()
            });
            let second = scope.spawn(|| {
                second_registry
                    .create(
                        "controlled",
                        "019f0000-0000-4000-8000-000000000402",
                        Some("Second"),
                        2,
                    )
                    .unwrap()
            });
            assert_ne!(
                first.join().unwrap().session_id,
                second.join().unwrap().session_id
            );
        });
        let registry = ProductSessionRegistry::new(&store);
        let index = registry.load_index().unwrap();
        assert_eq!(index.sessions.len(), 2);
        assert_eq!(index.request_receipts.len(), 2);
        assert_eq!(index.generation, 2);
    }

    #[test]
    fn session_cloud_records_do_not_persist_local_account_alias() {
        let summary = ProductSessionSummaryV1 {
            session_id: "01J00000000000000000000000".into(),
            display_name: Some("Shared".into()),
            created_at_ms: 1,
            archived: false,
        };
        let public = serde_json::to_string(&summary).unwrap();
        assert!(!public.contains("private-local-account"));
        assert!(!public.contains("local_account"));
    }

    #[test]
    fn product_session_create_is_local_and_defers_remote_manifest() {
        let root = temp_root("local-create");
        let store = credential_store(&root);
        let registry = ProductSessionRegistry::new(&store);
        let session = registry
            .create(
                "me",
                "123e4567-e89b-42d3-a456-426614174000",
                Some("Assistant"),
                1,
            )
            .unwrap();

        assert!(registry
            .remote_initialization_pending(&session.session_id)
            .unwrap());
        assert_eq!(
            registry
                .list_page("me", None, Some(1))
                .unwrap()
                .selected_session_id,
            Some(session.session_id)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pending_cancel_transcript_record_is_redacted_and_owner_bound() {
        let owner = PendingOwnerBinding {
            account: "private-account-alias".into(),
            session_id: "01J00000000000000000000000".into(),
            request_id: "123e4567-e89b-42d3-a456-426614174000".into(),
            turn_id: "01J00000000000000000000001".into(),
        };
        let record = pending_cancelled_record(
            &owner,
            "01J00000000000000000000002".into(),
            "01J00000000000000000000003".into(),
            isyncyou_agent::PersistedLeaseBinding {
                lease_id: "lease".into(),
                fence: 7,
                holder_binding: "holder".into(),
                expires_at_server_ms: 120_000,
            },
            10,
        );
        let encoded = serde_json::to_string(&record).unwrap();
        assert!(matches!(
            record.kind,
            SessionRecordKind::OperationState { ref code } if code == "cancelled"
        ));
        assert!(!encoded.contains("private-account-alias"));
        for forbidden in ["token", "action_hash", "payload", "ToolAction"] {
            assert!(!encoded.contains(forbidden), "record leaked {forbidden}");
        }
    }

    #[test]
    fn product_session_index_accepts_durable_archive_receipt() {
        let root = temp_root("archive-receipt");
        let store = credential_store(&root);
        let registry = ProductSessionRegistry::new(&store);
        insert_session(&registry, "01J00000000000000000000000", "me", 1);
        let mut index = registry.load_index().unwrap();
        index.request_receipts.push(ProductSessionRequestReceiptV1 {
            request_id: "123e4567-e89b-42d3-a456-426614174000".into(),
            route: "archive".into(),
            payload_digest: payload_digest(&"01J00000000000000000000000").unwrap(),
            session_id: "01J00000000000000000000000".into(),
        });
        registry.save_index(&index).unwrap();
        assert_eq!(registry.load_index().unwrap(), index);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn product_session_list_cursor_rejects_tamper_and_stale_generation() {
        let root = temp_root("cursor");
        let store = credential_store(&root);
        let registry = ProductSessionRegistry::new(&store);
        insert_session(&registry, "01J00000000000000000000000", "me", 1);
        insert_session(&registry, "01J00000000000000000000001", "me", 2);
        let first = registry.list_page("me", None, Some(1)).unwrap();
        let cursor = first.next_cursor.unwrap();
        let mut tampered = cursor.clone().into_bytes();
        tampered[0] = if tampered[0] == b'A' { b'B' } else { b'A' };
        assert_eq!(
            registry
                .list_page("me", Some(std::str::from_utf8(&tampered).unwrap()), Some(1),)
                .unwrap_err(),
            "invalid_cursor"
        );
        registry
            .select(
                "123e4567-e89b-42d3-a456-426614174000",
                "01J00000000000000000000000",
            )
            .unwrap();
        assert_eq!(
            registry
                .list_page("me", Some(&cursor), Some(1))
                .unwrap_err(),
            "stale_cursor"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn product_session_select_is_idempotent_and_rejects_request_rebinding() {
        let root = temp_root("select");
        let store = credential_store(&root);
        let registry = ProductSessionRegistry::new(&store);
        insert_session(&registry, "01J00000000000000000000000", "me", 1);
        insert_session(&registry, "01J00000000000000000000001", "me", 2);
        let request_id = "123e4567-e89b-42d3-a456-426614174000";
        registry
            .select(request_id, "01J00000000000000000000000")
            .unwrap();
        registry
            .select(request_id, "01J00000000000000000000000")
            .unwrap();
        assert_eq!(
            registry
                .select(request_id, "01J00000000000000000000001")
                .unwrap_err(),
            "request_id_conflict"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provider_generation_recovery_rejects_every_authority_binding_change() {
        let recorded = ProviderAttemptBindingV1 {
            provider: isyncyou_agent::ProductProviderId::Codex,
            model: "model-a".into(),
            reasoning_effort: Some("medium".into()),
            credential_generation: "generation-a".into(),
            oauth_policy_fingerprint: "policy-a".into(),
            harness_contract_version: 1,
            origin_installation_digest: "installation-a".into(),
        };
        assert!(validate_provider_recovery_binding(&recorded, &recorded).is_ok());

        type BindingMutation = Box<dyn Fn(&mut ProviderAttemptBindingV1)>;
        let mutations: [BindingMutation; 7] = [
            Box::new(|binding| binding.provider = isyncyou_agent::ProductProviderId::Claude),
            Box::new(|binding| binding.model = "model-b".into()),
            Box::new(|binding| binding.reasoning_effort = Some("high".into())),
            Box::new(|binding| binding.credential_generation = "generation-b".into()),
            Box::new(|binding| binding.oauth_policy_fingerprint = "policy-b".into()),
            Box::new(|binding| binding.harness_contract_version = 2),
            Box::new(|binding| binding.origin_installation_digest = "installation-b".into()),
        ];
        for mutate in mutations {
            let mut current = recorded.clone();
            mutate(&mut current);
            assert_eq!(
                validate_provider_recovery_binding(&recorded, &current),
                Err("provider_generation_changed".into())
            );
        }
    }

    #[test]
    fn crash_after_repeatable_read_before_second_step_resumes_after_digest_match() {
        let binding = recovery_binding();
        let action = read_action("item-a");
        let result = r#"{"source":{"service":"onedrive","id":"item-a"}}"#;
        let outcome = recovery_outcome(&binding, 0, None, "tool-a", action.clone());
        let journal = recovery_journal(
            binding,
            vec![ReadToolCheckpointV1 {
                provider_step_seq: 0,
                tool_use_id: "tool-a".into(),
                action,
                policy: isyncyou_agent::RecoveryPolicy::RepeatableReadAndCompare,
                result_sha256: tool_result_digest(result.as_bytes()),
                local_effect: None,
            }],
            1,
        );
        let executor = RecoveryExecutor {
            result: result.into(),
            calls: AtomicUsize::new(0),
        };
        let mut history = vec![isyncyou_agent::Message::user("question")];
        let mut sources = Vec::new();

        recover_provider_messages(&mut history, &[outcome], &journal, &executor, &mut sources)
            .unwrap();

        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert_eq!(history.len(), 3);
        assert_eq!(history[2].tool_use_id.as_deref(), Some("tool-a"));
        assert_eq!(sources.len(), 1);
    }

    #[test]
    fn repeatable_read_changed_result_returns_outcome_unknown() {
        let binding = recovery_binding();
        let action = read_action("item-a");
        let outcome = recovery_outcome(&binding, 0, None, "tool-a", action.clone());
        let journal = recovery_journal(
            binding,
            vec![ReadToolCheckpointV1 {
                provider_step_seq: 0,
                tool_use_id: "tool-a".into(),
                action,
                policy: isyncyou_agent::RecoveryPolicy::RepeatableReadAndCompare,
                result_sha256: tool_result_digest(b"old-result"),
                local_effect: None,
            }],
            1,
        );
        let executor = RecoveryExecutor {
            result: "changed-result".into(),
            calls: AtomicUsize::new(0),
        };
        let error = recover_provider_messages(
            &mut vec![isyncyou_agent::Message::user("question")],
            &[outcome],
            &journal,
            &executor,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            isyncyou_agent::AgentError::Provider(code) if code == "turn_outcome_unknown"
        ));
    }

    #[test]
    fn repeatable_read_retry_requires_matching_result_digest() {
        crash_after_repeatable_read_before_second_step_resumes_after_digest_match();
        repeatable_read_changed_result_returns_outcome_unknown();
    }

    #[test]
    fn lease_holder_binding_is_session_scoped_and_not_a_hardware_identifier() {
        let root = temp_root("holder-binding");
        let store = credential_store(&root);
        let derive = |installation: &str, session: &str| {
            store
                .domain_hmac(
                    TURN_HOLDER_DOMAIN,
                    format!("{installation}:{session}").as_bytes(),
                )
                .unwrap()
        };
        let first = derive("installation-a", "session-a");
        assert_ne!(first, derive("installation-a", "session-b"));
        assert_ne!(first, derive("installation-b", "session-a"));
        assert_eq!(first.len(), 32);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn multistep_crash_reconstructs_full_context_not_only_latest_outcome() {
        let binding = recovery_binding();
        let first_action = read_action("item-a");
        let second_action = read_action("item-b");
        let first = recovery_outcome(&binding, 0, None, "tool-a", first_action.clone());
        let second = recovery_outcome(
            &binding,
            1,
            Some(first.outcome_id.clone()),
            "tool-b",
            second_action.clone(),
        );
        let result = "same-result";
        let journal = recovery_journal(
            binding,
            vec![
                ReadToolCheckpointV1 {
                    provider_step_seq: 0,
                    tool_use_id: "tool-a".into(),
                    action: first_action,
                    policy: isyncyou_agent::RecoveryPolicy::RepeatableReadAndCompare,
                    result_sha256: tool_result_digest(result.as_bytes()),
                    local_effect: None,
                },
                ReadToolCheckpointV1 {
                    provider_step_seq: 1,
                    tool_use_id: "tool-b".into(),
                    action: second_action,
                    policy: isyncyou_agent::RecoveryPolicy::RepeatableReadAndCompare,
                    result_sha256: tool_result_digest(result.as_bytes()),
                    local_effect: None,
                },
            ],
            2,
        );
        let executor = RecoveryExecutor {
            result: result.into(),
            calls: AtomicUsize::new(0),
        };
        let mut history = vec![isyncyou_agent::Message::user("question")];
        recover_provider_messages(
            &mut history,
            &[first, second],
            &journal,
            &executor,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
        assert_eq!(history.len(), 5);
        assert_eq!(history[2].tool_use_id.as_deref(), Some("tool-a"));
        assert_eq!(history[4].tool_use_id.as_deref(), Some("tool-b"));
    }

    #[test]
    fn duplicate_tool_use_ids_persist_terminal_validation_outcome_before_failure() {
        let source = include_str!("product_session.rs");
        let invalid_branch = source
            .split("if invalid_tool_use_id")
            .nth(1)
            .and_then(|source| source.split("self.seen_tool_use_ids.extend").next())
            .expect("invalid tool-use branch");
        assert!(invalid_branch.contains("persist_provider_step_outcome"));
        assert!(invalid_branch.contains("Some(\"duplicate_tool_use_id\".into())"));

        let binding = recovery_binding();
        let outcome = RequestStepOutcomeV1 {
            outcome_version: 1,
            outcome_id: new_ulid().unwrap(),
            step_seq: 0,
            previous_outcome_id: None,
            provider: binding.provider,
            model: binding.model.clone(),
            normalized_blocks: vec![],
            final_text: None,
            sanitized_usage: None,
            terminal_validation_error: Some("duplicate_tool_use_id".into()),
            outcome_digest: String::new(),
        }
        .seal_digest()
        .unwrap();
        outcome.validate(&binding).unwrap();
        assert_eq!(
            outcome.terminal_validation_error.as_deref(),
            Some("duplicate_tool_use_id")
        );
    }
}
