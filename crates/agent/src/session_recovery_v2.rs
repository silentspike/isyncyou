//! Versioned progressive-turn recovery records introduced by #643.
//!
//! The pre-#643 V1 DTOs remain frozen in [`crate::session_v2`]. New product turns
//! write only these V2 records so deep-search authority, structured sources, and
//! deterministic finalization survive restart without changing V1 semantic bytes.

use crate::activity::{
    CoverageNoteReason, ProgressiveExitStateV1, ProgressiveFinalizationV1, StageStatus,
    TurnExitKind, ACTIVITY_ID_BYTES,
};
use crate::session_v2::{
    payload_digest, request_object_digest, tool_result_digest, valid_uuid_v4,
    LocalEffectCheckpointV1, ProviderAttemptBindingV1, RequestPhase, RequestStepRef,
    SanitizedUsage, SessionV2Error, SourceRef, MAX_FINAL_TEXT_BYTES, MAX_NORMALIZED_BLOCKS,
    MAX_PROVIDER_STEPS, MAX_REQUEST_OUTCOME_BYTES, MAX_SOURCE_REFS, MAX_SOURCE_REF_BYTES,
    MAX_STEP_OUTCOME_BYTES, MAX_TOOL_CHECKPOINTS, MAX_TOOL_USE_ID_BYTES, REQUEST_JOURNAL_VERSION,
};
use crate::{ProductProviderId, RecoveryPolicy, ToolAction};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const REQUEST_JOURNAL_V2_VERSION: u32 = 2;
pub const REQUEST_OUTCOME_V2_VERSION: u32 = 2;
pub const READ_CHECKPOINT_V2_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum LegacyToolActionV1 {
    Search {
        account: String,
        #[serde(default)]
        services: Vec<String>,
        query: String,
        #[serde(default)]
        limit: Option<u32>,
    },
    DeepSearch {
        account: String,
        #[serde(default)]
        services: Vec<String>,
        query: String,
        #[serde(default)]
        cursor: Option<u32>,
        #[serde(default)]
        max_reads: Option<u32>,
    },
    Read {
        account: String,
        service: String,
        id: String,
        #[serde(default)]
        max_bytes: Option<u64>,
    },
    List {
        account: String,
        service: String,
        #[serde(default)]
        parent: Option<String>,
        #[serde(default)]
        limit: Option<u32>,
        #[serde(default)]
        offset: Option<u32>,
    },
    Export {
        account: String,
        service: String,
        id: String,
    },
    RestoreLocal {
        account: String,
        service: String,
        id: String,
    },
    Backup {
        account: String,
        #[serde(default)]
        services: Vec<String>,
    },
    RestoreCloud {
        account: String,
        service: String,
        id: String,
    },
    LiveWrite {
        account: String,
        service: String,
        #[serde(default)]
        target: Option<String>,
        change: serde_json::Value,
    },
    Share {
        account: String,
        service: String,
        id: String,
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        link_type: Option<String>,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        recipients: Vec<String>,
        #[serde(default)]
        role: Option<String>,
        #[serde(default)]
        recipient: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LegacyNormalizedAssistantBlockV1 {
    Text {
        text: String,
    },
    ToolUse {
        tool_use_id: String,
        action: LegacyToolActionV1,
    },
    RejectedToolUse {
        tool_use_id: String,
        stable_error_code: String,
        help_schema_version: u32,
        help_digest: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyRequestStepOutcomeV1 {
    pub outcome_version: u32,
    pub outcome_id: String,
    pub step_seq: u8,
    pub previous_outcome_id: Option<String>,
    pub provider: ProductProviderId,
    pub model: String,
    pub normalized_blocks: Vec<LegacyNormalizedAssistantBlockV1>,
    pub final_text: Option<String>,
    pub sanitized_usage: Option<SanitizedUsage>,
    pub terminal_validation_error: Option<String>,
    pub outcome_digest: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyReadToolCheckpointV1 {
    pub provider_step_seq: u8,
    pub tool_use_id: String,
    pub action: LegacyToolActionV1,
    pub policy: RecoveryPolicy,
    pub result_sha256: String,
    pub local_effect: Option<LocalEffectCheckpointV1>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyRequestJournalV1 {
    pub journal_version: u32,
    pub session_id: String,
    pub request_id: String,
    pub turn_id: String,
    pub provider_binding: ProviderAttemptBindingV1,
    pub phase: RequestPhase,
    pub next_step_seq: u8,
    pub completed_steps: Vec<RequestStepRef>,
    pub read_checkpoints: Vec<LegacyReadToolCheckpointV1>,
}

impl LegacyRequestStepOutcomeV1 {
    fn validate(&self, binding: &ProviderAttemptBindingV1) -> Result<(), SessionV2Error> {
        if self.outcome_version != 1
            || !valid_ulid_wire(&self.outcome_id)
            || self.step_seq >= MAX_PROVIDER_STEPS
            || self.provider != binding.provider
            || self.model != binding.model
            || self.normalized_blocks.len() > MAX_NORMALIZED_BLOCKS
            || self
                .final_text
                .as_ref()
                .is_some_and(|text| text.len() > MAX_FINAL_TEXT_BYTES)
        {
            return Err(SessionV2Error::InvalidJournal);
        }
        let mut ids = BTreeSet::new();
        for block in &self.normalized_blocks {
            match block {
                LegacyNormalizedAssistantBlockV1::Text { text } => {
                    if text.len() > MAX_FINAL_TEXT_BYTES {
                        return Err(SessionV2Error::InvalidJournal);
                    }
                }
                LegacyNormalizedAssistantBlockV1::ToolUse { tool_use_id, .. } => {
                    if tool_use_id.is_empty()
                        || tool_use_id.len() > MAX_TOOL_USE_ID_BYTES
                        || !ids.insert(tool_use_id)
                    {
                        return Err(SessionV2Error::DuplicateToolUseId);
                    }
                }
                LegacyNormalizedAssistantBlockV1::RejectedToolUse {
                    tool_use_id,
                    stable_error_code,
                    help_schema_version,
                    help_digest,
                } => {
                    if tool_use_id.is_empty()
                        || tool_use_id.len() > MAX_TOOL_USE_ID_BYTES
                        || stable_error_code != "invalid_tool_arguments"
                        || *help_schema_version != 1
                        || !ids.insert(tool_use_id)
                    {
                        return Err(SessionV2Error::InvalidJournal);
                    }
                    let help = crate::tool::render_rejected_tool_help(1, stable_error_code)
                        .ok_or(SessionV2Error::InvalidJournal)?;
                    if tool_result_digest(help.as_bytes()) != *help_digest {
                        return Err(SessionV2Error::InvalidJournal);
                    }
                }
            }
        }
        let mut semantic = self.clone();
        semantic.outcome_digest.clear();
        if payload_digest(&semantic)? != self.outcome_digest {
            return Err(SessionV2Error::InvalidJournal);
        }
        let bytes = serde_json::to_vec(self).map_err(|_| SessionV2Error::InvalidJournal)?;
        if bytes.len() > MAX_STEP_OUTCOME_BYTES {
            return Err(SessionV2Error::SessionLimit);
        }
        Ok(())
    }
}

impl LegacyRequestJournalV1 {
    pub fn validate_chain_with<F>(
        &self,
        mut load: F,
    ) -> Result<Vec<LegacyRequestStepOutcomeV1>, SessionV2Error>
    where
        F: FnMut(&str) -> Result<Vec<u8>, SessionV2Error>,
    {
        if self.journal_version != REQUEST_JOURNAL_VERSION
            || self.session_id.is_empty()
            || self.session_id.len() > 128
            || !valid_uuid_v4(&self.request_id)
            || !valid_ulid_wire(&self.turn_id)
            || self.provider_binding.model.is_empty()
            || self.provider_binding.harness_contract_version == 0
            || self.next_step_seq > MAX_PROVIDER_STEPS
            || self.completed_steps.len() != usize::from(self.next_step_seq)
            || self.read_checkpoints.len() > MAX_TOOL_CHECKPOINTS
        {
            return Err(SessionV2Error::InvalidJournal);
        }
        let mut outcomes = Vec::with_capacity(self.completed_steps.len());
        let mut previous = None;
        let mut total_bytes = 0usize;
        for (expected, reference) in self.completed_steps.iter().enumerate() {
            if reference.step_seq != u8::try_from(expected).unwrap_or(u8::MAX)
                || !valid_ulid_wire(&reference.outcome_id)
            {
                return Err(SessionV2Error::InvalidJournal);
            }
            let bytes = load(&reference.outcome_id)?;
            if request_object_digest(&bytes) != reference.outcome_sha256 {
                return Err(SessionV2Error::InvalidJournal);
            }
            total_bytes = total_bytes
                .checked_add(bytes.len())
                .ok_or(SessionV2Error::SessionLimit)?;
            if total_bytes > usize::try_from(MAX_REQUEST_OUTCOME_BYTES).unwrap_or(usize::MAX) {
                return Err(SessionV2Error::SessionLimit);
            }
            let outcome: LegacyRequestStepOutcomeV1 =
                serde_json::from_slice(&bytes).map_err(|_| SessionV2Error::InvalidJournal)?;
            outcome.validate(&self.provider_binding)?;
            if outcome.step_seq != reference.step_seq
                || outcome.outcome_id != reference.outcome_id
                || outcome.previous_outcome_id != previous
            {
                return Err(SessionV2Error::InvalidJournal);
            }
            previous = Some(outcome.outcome_id.clone());
            outcomes.push(outcome);
        }
        for checkpoint in &self.read_checkpoints {
            if checkpoint.provider_step_seq >= self.next_step_seq
                || checkpoint.tool_use_id.is_empty()
                || checkpoint.tool_use_id.len() > MAX_TOOL_USE_ID_BYTES
                || checkpoint.result_sha256.len() != 43
            {
                return Err(SessionV2Error::InvalidJournal);
            }
        }
        Ok(outcomes)
    }
}

fn valid_ulid_wire(value: &str) -> bool {
    value.len() == 26
        && value
            .bytes()
            .all(|byte| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&byte))
}

#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalJsonValueV1(serde_json::Value);

impl CanonicalJsonValueV1 {
    pub fn try_from_value(value: serde_json::Value) -> Result<Self, SessionV2Error> {
        Ok(Self(canonicalize_json(value, 0)?))
    }

    pub fn into_value(self) -> serde_json::Value {
        self.0
    }
}

impl Serialize for CanonicalJsonValueV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CanonicalJsonValueV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        Self::try_from_value(value).map_err(serde::de::Error::custom)
    }
}

fn canonicalize_json(
    value: serde_json::Value,
    depth: usize,
) -> Result<serde_json::Value, SessionV2Error> {
    if depth > 32 {
        return Err(SessionV2Error::InvalidJournal);
    }
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::String(_) => {
            Ok(value)
        }
        serde_json::Value::Number(ref number) if number.is_i64() || number.is_u64() => Ok(value),
        serde_json::Value::Number(_) => Err(SessionV2Error::InvalidJournal),
        serde_json::Value::Array(values) if values.len() <= 256 => values
            .into_iter()
            .map(|value| canonicalize_json(value, depth + 1))
            .collect::<Result<Vec<_>, _>>()
            .map(serde_json::Value::Array),
        serde_json::Value::Object(values) if values.len() <= 256 => {
            let sorted = values
                .into_iter()
                .map(|(key, value)| Ok((key, canonicalize_json(value, depth + 1)?)))
                .collect::<Result<BTreeMap<_, _>, SessionV2Error>>()?;
            Ok(serde_json::Value::Object(sorted.into_iter().collect()))
        }
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            Err(SessionV2Error::InvalidJournal)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedToolActionV2 {
    pub action_version: u32,
    pub action: PersistedToolActionKindV2,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case", deny_unknown_fields)]
pub enum PersistedToolActionKindV2 {
    Search {
        account: String,
        services: Vec<String>,
        query: String,
        limit: Option<u32>,
    },
    DeepSearch {
        activity_id: String,
        continuation: String,
        candidates: Vec<String>,
    },
    Read {
        account: String,
        service: String,
        id: String,
        max_bytes: Option<u64>,
    },
    List {
        account: String,
        service: String,
        parent: Option<String>,
        limit: Option<u32>,
        offset: Option<u32>,
    },
    Export {
        account: String,
        service: String,
        id: String,
    },
    RestoreLocal {
        account: String,
        service: String,
        id: String,
    },
    Backup {
        account: String,
        services: Vec<String>,
    },
    RestoreCloud {
        account: String,
        service: String,
        id: String,
    },
    LiveWrite {
        account: String,
        service: String,
        target: Option<String>,
        change: CanonicalJsonValueV1,
    },
    Share {
        account: String,
        service: String,
        id: String,
        mode: Option<String>,
        link_type: Option<String>,
        scope: Option<String>,
        recipients: Vec<String>,
        role: Option<String>,
        recipient: Option<String>,
    },
}

impl PersistedToolActionV2 {
    pub fn from_runtime(action: &ToolAction) -> Result<Self, SessionV2Error> {
        let action = match action {
            ToolAction::Search {
                account,
                services,
                query,
                limit,
            } => PersistedToolActionKindV2::Search {
                account: account.clone(),
                services: services.clone(),
                query: query.clone(),
                limit: *limit,
            },
            ToolAction::DeepSearch {
                activity_id,
                continuation,
                candidates,
            } => PersistedToolActionKindV2::DeepSearch {
                activity_id: activity_id.clone(),
                continuation: continuation.clone(),
                candidates: candidates.clone(),
            },
            ToolAction::Read {
                account,
                service,
                id,
                max_bytes,
            } => PersistedToolActionKindV2::Read {
                account: account.clone(),
                service: service.clone(),
                id: id.clone(),
                max_bytes: *max_bytes,
            },
            ToolAction::List {
                account,
                service,
                parent,
                limit,
                offset,
            } => PersistedToolActionKindV2::List {
                account: account.clone(),
                service: service.clone(),
                parent: parent.clone(),
                limit: *limit,
                offset: *offset,
            },
            ToolAction::Export {
                account,
                service,
                id,
            } => PersistedToolActionKindV2::Export {
                account: account.clone(),
                service: service.clone(),
                id: id.clone(),
            },
            ToolAction::RestoreLocal {
                account,
                service,
                id,
            } => PersistedToolActionKindV2::RestoreLocal {
                account: account.clone(),
                service: service.clone(),
                id: id.clone(),
            },
            ToolAction::Backup { account, services } => PersistedToolActionKindV2::Backup {
                account: account.clone(),
                services: services.clone(),
            },
            ToolAction::RestoreCloud {
                account,
                service,
                id,
            } => PersistedToolActionKindV2::RestoreCloud {
                account: account.clone(),
                service: service.clone(),
                id: id.clone(),
            },
            ToolAction::LiveWrite {
                account,
                service,
                target,
                change,
            } => PersistedToolActionKindV2::LiveWrite {
                account: account.clone(),
                service: service.clone(),
                target: target.clone(),
                change: CanonicalJsonValueV1::try_from_value(change.clone())?,
            },
            ToolAction::Share {
                account,
                service,
                id,
                mode,
                link_type,
                scope,
                recipients,
                role,
                recipient,
            } => PersistedToolActionKindV2::Share {
                account: account.clone(),
                service: service.clone(),
                id: id.clone(),
                mode: mode.clone(),
                link_type: link_type.clone(),
                scope: scope.clone(),
                recipients: recipients.clone(),
                role: role.clone(),
                recipient: recipient.clone(),
            },
        };
        let value = Self {
            action_version: 2,
            action,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_runtime(&self) -> Result<ToolAction, SessionV2Error> {
        self.validate()?;
        Ok(match &self.action {
            PersistedToolActionKindV2::Search {
                account,
                services,
                query,
                limit,
            } => ToolAction::Search {
                account: account.clone(),
                services: services.clone(),
                query: query.clone(),
                limit: *limit,
            },
            PersistedToolActionKindV2::DeepSearch {
                activity_id,
                continuation,
                candidates,
            } => ToolAction::DeepSearch {
                activity_id: activity_id.clone(),
                continuation: continuation.clone(),
                candidates: candidates.clone(),
            },
            PersistedToolActionKindV2::Read {
                account,
                service,
                id,
                max_bytes,
            } => ToolAction::Read {
                account: account.clone(),
                service: service.clone(),
                id: id.clone(),
                max_bytes: *max_bytes,
            },
            PersistedToolActionKindV2::List {
                account,
                service,
                parent,
                limit,
                offset,
            } => ToolAction::List {
                account: account.clone(),
                service: service.clone(),
                parent: parent.clone(),
                limit: *limit,
                offset: *offset,
            },
            PersistedToolActionKindV2::Export {
                account,
                service,
                id,
            } => ToolAction::Export {
                account: account.clone(),
                service: service.clone(),
                id: id.clone(),
            },
            PersistedToolActionKindV2::RestoreLocal {
                account,
                service,
                id,
            } => ToolAction::RestoreLocal {
                account: account.clone(),
                service: service.clone(),
                id: id.clone(),
            },
            PersistedToolActionKindV2::Backup { account, services } => ToolAction::Backup {
                account: account.clone(),
                services: services.clone(),
            },
            PersistedToolActionKindV2::RestoreCloud {
                account,
                service,
                id,
            } => ToolAction::RestoreCloud {
                account: account.clone(),
                service: service.clone(),
                id: id.clone(),
            },
            PersistedToolActionKindV2::LiveWrite {
                account,
                service,
                target,
                change,
            } => ToolAction::LiveWrite {
                account: account.clone(),
                service: service.clone(),
                target: target.clone(),
                change: change.clone().into_value(),
            },
            PersistedToolActionKindV2::Share {
                account,
                service,
                id,
                mode,
                link_type,
                scope,
                recipients,
                role,
                recipient,
            } => ToolAction::Share {
                account: account.clone(),
                service: service.clone(),
                id: id.clone(),
                mode: mode.clone(),
                link_type: link_type.clone(),
                scope: scope.clone(),
                recipients: recipients.clone(),
                role: role.clone(),
                recipient: recipient.clone(),
            },
        })
    }

    pub fn recovery_policy(&self) -> Result<RecoveryPolicy, SessionV2Error> {
        Ok(self.to_runtime()?.recovery_policy())
    }

    fn validate(&self) -> Result<(), SessionV2Error> {
        if self.action_version != 2 {
            return Err(SessionV2Error::InvalidJournal);
        }
        let bytes = serde_json::to_vec(self).map_err(|_| SessionV2Error::InvalidJournal)?;
        if bytes.len() > 64 * 1024 {
            return Err(SessionV2Error::SessionLimit);
        }
        let runtime = match &self.action {
            PersistedToolActionKindV2::LiveWrite { .. } => return Ok(()),
            _ => self.to_runtime_unchecked(),
        };
        let runtime = runtime?;
        serde_json::to_value(runtime)
            .map_err(|_| SessionV2Error::InvalidJournal)
            .map(|_| ())
    }

    fn to_runtime_unchecked(&self) -> Result<ToolAction, SessionV2Error> {
        let mut clone = self.clone();
        clone.action_version = 2;
        match clone.action {
            PersistedToolActionKindV2::Search {
                account,
                services,
                query,
                limit,
            } => Ok(ToolAction::Search {
                account,
                services,
                query,
                limit,
            }),
            PersistedToolActionKindV2::DeepSearch {
                activity_id,
                continuation,
                candidates,
            } => Ok(ToolAction::DeepSearch {
                activity_id,
                continuation,
                candidates,
            }),
            PersistedToolActionKindV2::Read {
                account,
                service,
                id,
                max_bytes,
            } => Ok(ToolAction::Read {
                account,
                service,
                id,
                max_bytes,
            }),
            PersistedToolActionKindV2::List {
                account,
                service,
                parent,
                limit,
                offset,
            } => Ok(ToolAction::List {
                account,
                service,
                parent,
                limit,
                offset,
            }),
            PersistedToolActionKindV2::Export {
                account,
                service,
                id,
            } => Ok(ToolAction::Export {
                account,
                service,
                id,
            }),
            PersistedToolActionKindV2::RestoreLocal {
                account,
                service,
                id,
            } => Ok(ToolAction::RestoreLocal {
                account,
                service,
                id,
            }),
            PersistedToolActionKindV2::Backup { account, services } => {
                Ok(ToolAction::Backup { account, services })
            }
            PersistedToolActionKindV2::RestoreCloud {
                account,
                service,
                id,
            } => Ok(ToolAction::RestoreCloud {
                account,
                service,
                id,
            }),
            PersistedToolActionKindV2::LiveWrite {
                account,
                service,
                target,
                change,
            } => Ok(ToolAction::LiveWrite {
                account,
                service,
                target,
                change: change.into_value(),
            }),
            PersistedToolActionKindV2::Share {
                account,
                service,
                id,
                mode,
                link_type,
                scope,
                recipients,
                role,
                recipient,
            } => Ok(ToolAction::Share {
                account,
                service,
                id,
                mode,
                link_type,
                scope,
                recipients,
                role,
                recipient,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PersistedNormalizedAssistantBlockV2 {
    Text {
        text: String,
    },
    ToolUse {
        tool_use_id: String,
        action: PersistedToolActionV2,
    },
    RejectedToolUse {
        tool_use_id: String,
        stable_error_code: String,
        help_schema_version: u32,
        help_digest: String,
    },
}

impl PersistedNormalizedAssistantBlockV2 {
    pub fn tool_use_id(&self) -> Option<&str> {
        match self {
            Self::Text { .. } => None,
            Self::ToolUse { tool_use_id, .. } | Self::RejectedToolUse { tool_use_id, .. } => {
                Some(tool_use_id)
            }
        }
    }

    pub fn recover_rejected_tool_help(&self) -> Result<Option<String>, SessionV2Error> {
        let Self::RejectedToolUse {
            stable_error_code,
            help_schema_version,
            help_digest,
            ..
        } = self
        else {
            return Ok(None);
        };
        let help = crate::tool::render_rejected_tool_help(*help_schema_version, stable_error_code)
            .ok_or(SessionV2Error::InvalidJournal)?;
        if crate::session_v2::tool_result_digest(help.as_bytes()) != *help_digest {
            return Err(SessionV2Error::InvalidJournal);
        }
        Ok(Some(help))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestStepOutcomeV2 {
    pub outcome_version: u32,
    pub outcome_id: String,
    pub step_seq: u8,
    pub previous_outcome_id: Option<String>,
    pub provider: ProductProviderId,
    pub model: String,
    pub normalized_blocks: Vec<PersistedNormalizedAssistantBlockV2>,
    pub final_text: Option<String>,
    pub assistant_sources: Vec<SourceRef>,
    pub sanitized_usage: Option<SanitizedUsage>,
    pub terminal_validation_error: Option<String>,
    pub finalization: Option<ProgressiveFinalizationV1>,
    pub outcome_digest: String,
}

impl RequestStepOutcomeV2 {
    pub fn seal_digest(mut self) -> Result<Self, SessionV2Error> {
        self.outcome_digest.clear();
        self.outcome_digest = semantic_digest(&self)?;
        Ok(self)
    }

    pub fn validate(&self, binding: &ProviderAttemptBindingV1) -> Result<(), SessionV2Error> {
        if self.outcome_version != REQUEST_OUTCOME_V2_VERSION
            || !valid_ulid(&self.outcome_id)
            || self.step_seq >= MAX_PROVIDER_STEPS
            || self.provider != binding.provider
            || self.model != binding.model
            || self.normalized_blocks.len() > MAX_NORMALIZED_BLOCKS
            || self
                .final_text
                .as_ref()
                .is_some_and(|text| text.len() > MAX_FINAL_TEXT_BYTES)
            || !valid_sources(&self.assistant_sources)
            || self
                .terminal_validation_error
                .as_ref()
                .is_some_and(|code| !valid_closed_code(code))
        {
            return Err(SessionV2Error::InvalidJournal);
        }
        if self.finalization.is_some()
            && (self.final_text.is_none()
                || self.normalized_blocks.iter().any(|block| {
                    !matches!(block, PersistedNormalizedAssistantBlockV2::Text { .. })
                }))
        {
            return Err(SessionV2Error::RecoveryOutcomeUnknown);
        }
        let mut ids = BTreeSet::new();
        for block in &self.normalized_blocks {
            match block {
                PersistedNormalizedAssistantBlockV2::Text { text }
                    if text.len() <= MAX_FINAL_TEXT_BYTES => {}
                PersistedNormalizedAssistantBlockV2::Text { .. } => {
                    return Err(SessionV2Error::InvalidJournal);
                }
                PersistedNormalizedAssistantBlockV2::ToolUse {
                    tool_use_id,
                    action,
                } => {
                    action.validate()?;
                    if !valid_tool_use_id(tool_use_id) || !ids.insert(tool_use_id) {
                        return Err(SessionV2Error::DuplicateToolUseId);
                    }
                }
                PersistedNormalizedAssistantBlockV2::RejectedToolUse { tool_use_id, .. } => {
                    if !valid_tool_use_id(tool_use_id) || !ids.insert(tool_use_id) {
                        return Err(SessionV2Error::DuplicateToolUseId);
                    }
                    block.recover_rejected_tool_help()?;
                }
            }
        }
        if let (Some(text), Some(finalization)) = (&self.final_text, &self.finalization) {
            let digest = ring::digest::digest(&ring::digest::SHA256, text.as_bytes());
            let hex = digest
                .as_ref()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            if finalization.finalized_text_sha256 != hex
                || !valid_progressive_finalization(finalization)
            {
                return Err(SessionV2Error::RecoveryOutcomeUnknown);
            }
        }
        let mut semantic = self.clone();
        semantic.outcome_digest.clear();
        if semantic_digest(&semantic)? != self.outcome_digest {
            return Err(SessionV2Error::InvalidJournal);
        }
        if serde_json::to_vec(self)
            .map_err(|_| SessionV2Error::InvalidJournal)?
            .len()
            > MAX_STEP_OUTCOME_BYTES
        {
            return Err(SessionV2Error::SessionLimit);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadToolCheckpointV2 {
    pub checkpoint_version: u32,
    pub provider_step_seq: u8,
    pub tool_use_id: String,
    pub action: PersistedToolActionV2,
    pub policy: RecoveryPolicy,
    pub result_sha256: String,
    pub assistant_sources: Vec<SourceRef>,
    pub local_effect: Option<LocalEffectCheckpointV1>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestJournalV2 {
    pub journal_version: u32,
    pub session_id: String,
    pub request_id: String,
    pub turn_id: String,
    pub provider_binding: ProviderAttemptBindingV1,
    pub phase: RequestPhase,
    pub next_step_seq: u8,
    pub completed_steps: Vec<RequestStepRef>,
    pub read_checkpoints: Vec<ReadToolCheckpointV2>,
    pub progressive_exit: Option<ProgressiveExitStateV1>,
}

impl RequestJournalV2 {
    pub fn validate_chain_with<F>(
        &self,
        mut load: F,
    ) -> Result<Vec<RequestStepOutcomeV2>, SessionV2Error>
    where
        F: FnMut(&str) -> Result<Vec<u8>, SessionV2Error>,
    {
        if self.journal_version != REQUEST_JOURNAL_V2_VERSION
            || self.session_id.is_empty()
            || self.session_id.len() > 128
            || !valid_uuid_v4(&self.request_id)
            || !valid_ulid(&self.turn_id)
            || self.next_step_seq > MAX_PROVIDER_STEPS
            || self.completed_steps.len() != usize::from(self.next_step_seq)
            || self.read_checkpoints.len() > MAX_TOOL_CHECKPOINTS
        {
            return Err(SessionV2Error::InvalidJournal);
        }
        if self
            .progressive_exit
            .as_ref()
            .is_some_and(|exit| !valid_progressive_exit(exit))
        {
            return Err(SessionV2Error::RecoveryOutcomeUnknown);
        }
        let mut outcomes = Vec::with_capacity(self.completed_steps.len());
        let mut previous = None::<String>;
        let mut total_bytes = 0_u64;
        let mut all_ids = BTreeSet::new();
        for (expected, step) in self.completed_steps.iter().enumerate() {
            if usize::from(step.step_seq) != expected
                || !valid_ulid(&step.outcome_id)
                || !valid_base64url_digest(&step.outcome_sha256)
            {
                return Err(SessionV2Error::InvalidJournal);
            }
            let bytes = load(&step.outcome_id)?;
            total_bytes = total_bytes
                .checked_add(bytes.len() as u64)
                .ok_or(SessionV2Error::SessionLimit)?;
            if total_bytes > MAX_REQUEST_OUTCOME_BYTES
                || request_object_digest(&bytes) != step.outcome_sha256
            {
                return Err(SessionV2Error::InvalidJournal);
            }
            let outcome: RequestStepOutcomeV2 =
                serde_json::from_slice(&bytes).map_err(|_| SessionV2Error::InvalidJournal)?;
            outcome.validate(&self.provider_binding)?;
            if outcome.step_seq != step.step_seq
                || outcome.outcome_id != step.outcome_id
                || outcome.previous_outcome_id != previous
            {
                return Err(SessionV2Error::InvalidJournal);
            }
            for block in &outcome.normalized_blocks {
                if let Some(id) = block.tool_use_id() {
                    if !all_ids.insert(id.to_owned()) {
                        return Err(SessionV2Error::DuplicateToolUseId);
                    }
                }
            }
            previous = Some(step.outcome_id.clone());
            outcomes.push(outcome);
        }
        let mut checkpoint_keys = BTreeSet::new();
        for checkpoint in &self.read_checkpoints {
            let runtime_action = checkpoint.action.to_runtime()?;
            let matching = outcomes
                .get(usize::from(checkpoint.provider_step_seq))
                .and_then(|outcome| {
                    outcome
                        .normalized_blocks
                        .iter()
                        .find_map(|block| match block {
                            PersistedNormalizedAssistantBlockV2::ToolUse {
                                tool_use_id,
                                action,
                            } if tool_use_id == &checkpoint.tool_use_id => Some(action),
                            _ => None,
                        })
                });
            if checkpoint.checkpoint_version != READ_CHECKPOINT_V2_VERSION
                || !checkpoint_keys
                    .insert((checkpoint.provider_step_seq, checkpoint.tool_use_id.clone()))
                || !valid_tool_use_id(&checkpoint.tool_use_id)
                || matching != Some(&checkpoint.action)
                || checkpoint.policy != runtime_action.recovery_policy()
                || checkpoint.policy == RecoveryPolicy::NeverRepeat
                || !checkpoint.result_sha256.is_empty()
                    && !valid_base64url_digest(&checkpoint.result_sha256)
                || !valid_sources(&checkpoint.assistant_sources)
            {
                return Err(SessionV2Error::InvalidJournal);
            }
        }
        validate_exit_consistency(self, &outcomes)?;
        Ok(outcomes)
    }
}

fn validate_exit_consistency(
    journal: &RequestJournalV2,
    outcomes: &[RequestStepOutcomeV2],
) -> Result<(), SessionV2Error> {
    let Some(exit) = &journal.progressive_exit else {
        if outcomes
            .iter()
            .any(|outcome| outcome.finalization.is_some())
        {
            return Err(SessionV2Error::RecoveryOutcomeUnknown);
        }
        return Ok(());
    };
    let finalization_indexes = outcomes
        .iter()
        .enumerate()
        .filter_map(|(index, outcome)| outcome.finalization.as_ref().map(|_| index))
        .collect::<Vec<_>>();
    if finalization_indexes
        .iter()
        .any(|index| *index + 1 != outcomes.len())
    {
        return Err(SessionV2Error::RecoveryOutcomeUnknown);
    }
    let finalization = outcomes
        .last()
        .and_then(|outcome| outcome.finalization.as_ref());
    match exit.exit_kind {
        TurnExitKind::Final if exit.activities.is_empty() => {
            if finalization.is_some() {
                return Err(SessionV2Error::RecoveryOutcomeUnknown);
            }
        }
        TurnExitKind::Final => {
            let finalization = finalization.ok_or(SessionV2Error::RecoveryOutcomeUnknown)?;
            if finalization.activities.len() != exit.activities.len()
                || finalization.activities.iter().zip(&exit.activities).any(
                    |(finalized, exited)| {
                        finalized.activity_id != exited.activity_id
                            || finalized.deep_status != exited.deep_status
                    },
                )
            {
                return Err(SessionV2Error::RecoveryOutcomeUnknown);
            }
        }
        _ if finalization.is_some() => {
            return Err(SessionV2Error::RecoveryOutcomeUnknown);
        }
        _ => {}
    }
    Ok(())
}

fn valid_progressive_exit(exit: &ProgressiveExitStateV1) -> bool {
    if exit.exit_version != 1 || exit.activities.len() > 4 {
        return false;
    }
    let expected_code = match exit.exit_kind {
        TurnExitKind::Final => None,
        TurnExitKind::PendingConfirmation => Some("pending_confirmation"),
        TurnExitKind::ProviderError => Some("provider_error"),
        TurnExitKind::Cancelled => Some("cancelled"),
        TurnExitKind::OutcomeUnknown => Some("turn_outcome_unknown"),
        TurnExitKind::StepLimit => Some("turn_step_limit"),
    };
    if exit.terminal_code.as_deref() != expected_code {
        return false;
    }
    let mut ids = BTreeSet::new();
    exit.activities.iter().all(|activity| {
        valid_activity_id(&activity.activity_id)
            && ids.insert(activity.activity_id.as_str())
            && terminal_stage_status(activity.names_status)
            && terminal_stage_status(activity.bodies_status)
            && terminal_stage_status(activity.deep_status)
    })
}

fn valid_progressive_finalization(finalization: &ProgressiveFinalizationV1) -> bool {
    if finalization.finalization_version != 1
        || finalization.activities.is_empty()
        || finalization.activities.len() > 4
        || !valid_hex_digest(&finalization.finalized_text_sha256)
    {
        return false;
    }
    let mut ids = BTreeSet::new();
    if finalization.activities.iter().any(|activity| {
        !valid_activity_id(&activity.activity_id)
            || !ids.insert(activity.activity_id.as_str())
            || !terminal_stage_status(activity.deep_status)
            || activity.continuation_available
    }) {
        return false;
    }
    let expected_reason = finalization
        .activities
        .iter()
        .any(|activity| activity.budget_reached)
        .then_some(CoverageNoteReason::BudgetReached)
        .or_else(|| {
            finalization
                .activities
                .iter()
                .any(|activity| !activity.coverage_complete)
                .then_some(CoverageNoteReason::Incomplete)
        });
    finalization
        .coverage_note
        .as_ref()
        .map(|note| (note.version, note.reason))
        == expected_reason.map(|reason| (1, reason))
}

fn valid_activity_id(value: &str) -> bool {
    value.len() == ACTIVITY_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn terminal_stage_status(status: StageStatus) -> bool {
    !matches!(status, StageStatus::Queued | StageStatus::Running)
}

fn semantic_digest<T: Serialize>(value: &T) -> Result<String, SessionV2Error> {
    let bytes = serde_json::to_vec(value).map_err(|_| SessionV2Error::InvalidJournal)?;
    Ok(request_object_digest(&bytes))
}

fn valid_sources(sources: &[SourceRef]) -> bool {
    if sources.len() > MAX_SOURCE_REFS {
        return false;
    }
    let mut unique = BTreeSet::new();
    sources.iter().all(|source| {
        serde_json::to_vec(source).is_ok_and(|bytes| {
            bytes.len() <= MAX_SOURCE_REF_BYTES
                && unique.insert(String::from_utf8_lossy(&bytes).into_owned())
        })
    })
}

fn valid_tool_use_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_TOOL_USE_ID_BYTES
}

fn valid_closed_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn valid_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_base64url_digest(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_ulid(value: &str) -> bool {
    value.len() == 26
        && value.bytes().all(|byte| {
            matches!(
                byte,
                b'0'..=b'9'
                    | b'A'..=b'H'
                    | b'J'..=b'K'
                    | b'M'..=b'N'
                    | b'P'..=b'T'
                    | b'V'..=b'Z'
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::{ProgressiveActivityExitV1, ProgressiveActivityFinalizationV1};
    use ring::digest::{digest, SHA256};
    use std::collections::BTreeMap;

    fn fixture_hex_sha256(bytes: &[u8]) -> String {
        digest(&SHA256, bytes)
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn legacy_fixture_crypto() -> crate::SessionObjectCrypto {
        crate::SessionObjectCrypto::new(
            b"issue-643-fixed-fixture-key-v1!",
            crate::SessionCryptoConfig::new(crate::KdfProfile::production([43; 16])).unwrap(),
        )
        .unwrap()
    }

    fn recovery_test_journal(exit: ProgressiveExitStateV1) -> RequestJournalV2 {
        RequestJournalV2 {
            journal_version: REQUEST_JOURNAL_V2_VERSION,
            session_id: "01J00000000000000000000000".into(),
            request_id: "123e4567-e89b-42d3-a456-426614174000".into(),
            turn_id: "01J00000000000000000000001".into(),
            provider_binding: ProviderAttemptBindingV1 {
                provider: ProductProviderId::Claude,
                model: "model".into(),
                reasoning_effort: None,
                credential_generation: "generation".into(),
                oauth_policy_fingerprint: "policy".into(),
                harness_contract_version: 2,
                origin_installation_digest: "installation".into(),
            },
            phase: RequestPhase::ProviderStepCompleted,
            next_step_seq: 0,
            completed_steps: Vec::new(),
            read_checkpoints: Vec::new(),
            progressive_exit: Some(exit),
        }
    }

    fn recovery_test_outcome(
        blocks: Vec<PersistedNormalizedAssistantBlockV2>,
        finalization: Option<ProgressiveFinalizationV1>,
    ) -> RequestStepOutcomeV2 {
        RequestStepOutcomeV2 {
            outcome_version: REQUEST_OUTCOME_V2_VERSION,
            outcome_id: "01J00000000000000000000002".into(),
            step_seq: 0,
            previous_outcome_id: None,
            provider: ProductProviderId::Claude,
            model: "model".into(),
            normalized_blocks: blocks,
            final_text: Some("answer".into()),
            assistant_sources: Vec::new(),
            sanitized_usage: None,
            terminal_validation_error: None,
            finalization,
            outcome_digest: String::new(),
        }
    }

    fn progressive_final_exit() -> ProgressiveExitStateV1 {
        ProgressiveExitStateV1 {
            exit_version: 1,
            exit_kind: TurnExitKind::Final,
            activities: vec![ProgressiveActivityExitV1 {
                activity_id: "abcdefghijklmnopqrstuv".into(),
                names_status: StageStatus::Complete,
                bodies_status: StageStatus::Complete,
                deep_status: StageStatus::Complete,
            }],
            terminal_code: None,
        }
    }

    #[test]
    fn non_progressive_final_recovery_needs_no_progressive_marker() {
        let journal = recovery_test_journal(ProgressiveExitStateV1 {
            exit_version: 1,
            exit_kind: TurnExitKind::Final,
            activities: Vec::new(),
            terminal_code: None,
        });
        let outcome = recovery_test_outcome(
            vec![PersistedNormalizedAssistantBlockV2::Text {
                text: "answer".into(),
            }],
            None,
        );
        assert_eq!(validate_exit_consistency(&journal, &[outcome]), Ok(()));
    }

    #[test]
    fn progressive_final_recovery_without_marker_is_outcome_unknown() {
        let journal = recovery_test_journal(progressive_final_exit());
        let outcome = recovery_test_outcome(
            vec![PersistedNormalizedAssistantBlockV2::Text {
                text: "answer".into(),
            }],
            None,
        );
        assert_eq!(
            validate_exit_consistency(&journal, &[outcome]),
            Err(SessionV2Error::RecoveryOutcomeUnknown)
        );
    }

    #[test]
    fn progressive_finalization_on_tool_outcome_is_outcome_unknown() {
        let marker = ProgressiveFinalizationV1 {
            finalization_version: 1,
            activities: vec![ProgressiveActivityFinalizationV1 {
                activity_id: "abcdefghijklmnopqrstuv".into(),
                deep_status: StageStatus::Complete,
                coverage_complete: true,
                budget_reached: false,
                continuation_available: false,
            }],
            coverage_note: None,
            finalized_text_sha256: fixture_hex_sha256(b"answer"),
        };
        let outcome = recovery_test_outcome(
            vec![PersistedNormalizedAssistantBlockV2::ToolUse {
                tool_use_id: "tool".into(),
                action: PersistedToolActionV2 {
                    action_version: 2,
                    action: PersistedToolActionKindV2::Read {
                        account: "me".into(),
                        service: "mail".into(),
                        id: "item".into(),
                        max_bytes: None,
                    },
                },
            }],
            Some(marker),
        );
        assert_eq!(
            outcome.validate(&recovery_test_journal(progressive_final_exit()).provider_binding),
            Err(SessionV2Error::RecoveryOutcomeUnknown)
        );
    }

    #[test]
    fn progressive_exit_and_finalization_mismatch_is_outcome_unknown() {
        let journal = recovery_test_journal(progressive_final_exit());
        let outcome = recovery_test_outcome(
            vec![PersistedNormalizedAssistantBlockV2::Text {
                text: "answer".into(),
            }],
            Some(ProgressiveFinalizationV1 {
                finalization_version: 1,
                activities: vec![ProgressiveActivityFinalizationV1 {
                    activity_id: "differentabcdefghijklm".into(),
                    deep_status: StageStatus::Complete,
                    coverage_complete: true,
                    budget_reached: false,
                    continuation_available: false,
                }],
                coverage_note: None,
                finalized_text_sha256: fixture_hex_sha256(b"answer"),
            }),
        );
        assert_eq!(
            validate_exit_consistency(&journal, &[outcome]),
            Err(SessionV2Error::RecoveryOutcomeUnknown)
        );
    }

    #[test]
    fn legacy_v1_encrypted_fixture_verifies_original_object_and_semantic_digests() {
        let meta: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/progressive-search-v1/fixture-meta.json"
        )))
        .unwrap();
        let sealed_objects = BTreeMap::from([
            (
                "request-journal-v1",
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/progressive-search-v1/request-journal-v1.sealed"
                ))
                .as_slice(),
            ),
            (
                "search-outcome-v1",
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/progressive-search-v1/search-outcome-v1.sealed"
                ))
                .as_slice(),
            ),
            (
                "deep-search-outcome-v1",
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/progressive-search-v1/deep-search-outcome-v1.sealed"
                ))
                .as_slice(),
            ),
            (
                "read-checkpoint-v1",
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/progressive-search-v1/read-checkpoint-v1.sealed"
                ))
                .as_slice(),
            ),
        ]);
        let crypto = legacy_fixture_crypto();
        let mut plaintext = BTreeMap::new();
        for object in meta["objects"].as_array().unwrap() {
            let object_id = object["object_id"].as_str().unwrap();
            let sealed = sealed_objects[object_id];
            assert_eq!(
                fixture_hex_sha256(sealed),
                object["ciphertext_sha256"].as_str().unwrap()
            );
            let opened = crypto
                .open(
                    meta["session_id"].as_str().unwrap(),
                    crate::SessionObjectClass::RequestState,
                    object_id,
                    sealed,
                )
                .unwrap();
            assert_eq!(
                request_object_digest(&opened),
                object["plaintext_object_digest"].as_str().unwrap()
            );
            plaintext.insert(object_id.to_owned(), opened);
        }

        let journal: LegacyRequestJournalV1 =
            serde_json::from_slice(&plaintext["request-journal-v1"]).unwrap();
        let outcomes = journal
            .validate_chain_with(|outcome_id| match outcome_id {
                "00000000000000000000000644" => Ok(plaintext["search-outcome-v1"].clone()),
                "00000000000000000000000645" => Ok(plaintext["deep-search-outcome-v1"].clone()),
                _ => Err(SessionV2Error::InvalidJournal),
            })
            .unwrap();
        assert_eq!(outcomes.len(), 2);
        assert_eq!(
            outcomes[0].outcome_digest,
            meta["search_semantic_digest"].as_str().unwrap()
        );
        assert_eq!(
            outcomes[1].outcome_digest,
            meta["deep_search_semantic_digest"].as_str().unwrap()
        );
        assert!(matches!(
            &outcomes[1].normalized_blocks[0],
            LegacyNormalizedAssistantBlockV1::ToolUse {
                action: LegacyToolActionV1::DeepSearch {
                    cursor: Some(17),
                    max_reads: Some(3),
                    ..
                },
                ..
            }
        ));

        let help = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/progressive-search-v1/rejected-help-v1.txt"
        ));
        assert_eq!(
            crate::tool::render_rejected_tool_help(1, crate::tool::INVALID_TOOL_ARGUMENTS_CODE)
                .as_deref(),
            Some(help)
        );
        assert_eq!(
            fixture_hex_sha256(help.as_bytes()),
            meta["rejected_help_sha256"].as_str().unwrap()
        );
    }

    #[test]
    fn persisted_tool_action_v2_round_trips_all_runtime_variants() {
        let actions = vec![
            ToolAction::Search {
                account: "me".into(),
                services: vec!["mail".into()],
                query: "invoice".into(),
                limit: None,
            },
            ToolAction::DeepSearch {
                activity_id: "abcdefghijklmnopqrstuv".into(),
                continuation: "opaque".into(),
                candidates: vec!["candidate".into()],
            },
            ToolAction::Read {
                account: "me".into(),
                service: "mail".into(),
                id: "id".into(),
                max_bytes: Some(42),
            },
            ToolAction::List {
                account: "me".into(),
                service: "mail".into(),
                parent: None,
                limit: None,
                offset: None,
            },
            ToolAction::Export {
                account: "me".into(),
                service: "mail".into(),
                id: "id".into(),
            },
            ToolAction::RestoreLocal {
                account: "me".into(),
                service: "mail".into(),
                id: "id".into(),
            },
            ToolAction::Backup {
                account: "me".into(),
                services: vec![],
            },
            ToolAction::RestoreCloud {
                account: "me".into(),
                service: "mail".into(),
                id: "id".into(),
            },
            ToolAction::LiveWrite {
                account: "me".into(),
                service: "mail".into(),
                target: Some("id".into()),
                change: serde_json::json!({"z": 1, "a": true}),
            },
            ToolAction::Share {
                account: "me".into(),
                service: "mail".into(),
                id: "id".into(),
                mode: None,
                link_type: None,
                scope: None,
                recipients: vec![],
                role: None,
                recipient: None,
            },
        ];
        for action in actions {
            let persisted = PersistedToolActionV2::from_runtime(&action).unwrap();
            assert_eq!(persisted.to_runtime().unwrap(), action);
        }
    }

    #[test]
    fn canonical_json_rejects_float_and_sorts_object_keys() {
        assert!(CanonicalJsonValueV1::try_from_value(serde_json::json!(1.25)).is_err());
        let value =
            CanonicalJsonValueV1::try_from_value(serde_json::json!({"z": 1, "a": 2})).unwrap();
        assert_eq!(serde_json::to_string(&value).unwrap(), r#"{"a":2,"z":1}"#);
    }
}
