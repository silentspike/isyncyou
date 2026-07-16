//! Product shared-session V2 records and fenced manifest publication (#628).
//!
//! V1 remains readable in `session`; all new product writes use these versioned,
//! bounded records. Transport implementations stage immutable encrypted objects and
//! publish their reachability only through one manifest compare-and-swap.

use crate::{ProductProviderId, SessionObjectClass, SessionObjectCrypto, ToolAction};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ring::{
    digest::{digest, SHA256},
    hmac,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

pub const SESSION_RECORD_VERSION: u32 = 2;
pub const SESSION_MANIFEST_VERSION: u32 = 1;
pub const REQUEST_JOURNAL_VERSION: u32 = 1;
pub const MAX_SESSION_RECORDS: u64 = 10_000;
pub const MAX_SESSION_ENCRYPTED_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_PROVIDER_STEPS: u8 = 16;
pub const MAX_TOOL_CHECKPOINTS: usize = 64;
pub const MAX_STEP_OUTCOME_BYTES: usize = 256 * 1024;
pub const MAX_REQUEST_OUTCOME_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_NORMALIZED_BLOCKS: usize = 64;
pub const MAX_TOOL_USE_ID_BYTES: usize = 128;
pub const MAX_PROMPT_BYTES: usize = 32 * 1024;
pub const MAX_FINAL_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_SOURCE_REFS: usize = 64;
pub const MAX_SOURCE_REF_BYTES: usize = 2 * 1024;
pub const DEFAULT_HISTORY_PAGE_SIZE: usize = 50;
pub const MAX_HISTORY_PAGE_SIZE: usize = 100;
pub const MAX_HISTORY_RESPONSE_BYTES: usize = 1024 * 1024;
pub const MAX_CONTEXT_MESSAGES: usize = 64;
pub const MAX_CONTEXT_BYTES: usize = 256 * 1024;
pub const MAX_CONTEXT_TOKENS: usize = 32_768;
pub const SESSION_LEASE_TTL_MS: u64 = 120_000;
pub const SESSION_LEASE_RENEW_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const SESSION_LEASE_TAKEOVER_MARGIN_MS: u64 = 5_000;
const MAX_SERVER_TIME_SAMPLE_AGE: std::time::Duration = std::time::Duration::from_secs(10);
const ORPHAN_RETENTION_MS: u64 = 24 * 60 * 60 * 1000;
const ORPHAN_REAP_BATCH: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionWritePolicy {
    LegacyReadOnly,
    WritableV2,
}

pub fn session_write_policy(record_version: u32) -> Result<SessionWritePolicy, SessionV2Error> {
    match record_version {
        1 => Ok(SessionWritePolicy::LegacyReadOnly),
        SESSION_RECORD_VERSION => Ok(SessionWritePolicy::WritableV2),
        _ => Err(SessionV2Error::InvalidRecord),
    }
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum SessionV2Error {
    #[error("invalid_session_record")]
    InvalidRecord,
    #[error("invalid_request_id")]
    InvalidRequestId,
    #[error("request_id_conflict")]
    RequestConflict,
    #[error("session_limit_reached")]
    SessionLimit,
    #[error("manifest_conflict")]
    ManifestConflict,
    #[error("lease_lost")]
    LeaseLost,
    #[error("invalid_request_journal")]
    InvalidJournal,
    #[error("duplicate_tool_use_id")]
    DuplicateToolUseId,
    #[error("provider_generation_changed")]
    ProviderGenerationChanged,
    #[error("invalid_cursor")]
    InvalidCursor,
    #[error("history_page_too_large")]
    HistoryPageTooLarge,
    #[error("session_transport_unavailable")]
    TransportUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRef {
    pub service: String,
    pub item_id: String,
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SanitizedUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedLeaseBinding {
    pub lease_id: String,
    pub holder_binding: String,
    pub fence: u64,
    pub expires_at_server_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionRecordKind {
    TurnIntent {
        user_text: String,
    },
    AssistantResult {
        text: String,
        sources: Vec<SourceRef>,
        usage: Option<SanitizedUsage>,
    },
    PendingOperation {
        code: String,
    },
    OperationState {
        code: String,
    },
    TurnTerminal {
        status: TurnTerminalStatus,
        error_code: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnTerminalStatus {
    Complete,
    Error,
    Cancelled,
    OutcomeUnknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRecordV2 {
    pub record_version: u32,
    pub record_id: String,
    pub session_id: String,
    pub request_id: String,
    pub turn_id: String,
    pub kind: SessionRecordKind,
    pub parent_record_ids: Vec<String>,
    pub observed_head: Option<String>,
    pub lease: PersistedLeaseBinding,
    pub created_at_ms: u64,
}

impl SessionRecordV2 {
    pub fn validate(&self) -> Result<(), SessionV2Error> {
        if self.record_version != SESSION_RECORD_VERSION
            || !valid_ulid(&self.record_id)
            || !valid_ulid(&self.turn_id)
            || self.session_id.is_empty()
            || self.session_id.len() > 128
            || !valid_uuid_v4(&self.request_id)
            || self.parent_record_ids.len() > MAX_NORMALIZED_BLOCKS
            || self.parent_record_ids.iter().any(|id| !valid_ulid(id))
            || self
                .observed_head
                .as_ref()
                .is_some_and(|id| !valid_ulid(id))
            || self.lease.lease_id.is_empty()
            || self.lease.holder_binding.is_empty()
        {
            return Err(SessionV2Error::InvalidRecord);
        }
        match &self.kind {
            SessionRecordKind::TurnIntent { user_text } => {
                if user_text.is_empty() || user_text.len() > MAX_PROMPT_BYTES {
                    return Err(SessionV2Error::InvalidRecord);
                }
            }
            SessionRecordKind::AssistantResult { text, sources, .. } => {
                if text.len() > MAX_FINAL_TEXT_BYTES
                    || sources.len() > MAX_SOURCE_REFS
                    || sources.iter().any(|source| {
                        source.service.is_empty()
                            || source.item_id.is_empty()
                            || serde_json::to_vec(source)
                                .map(|value| value.len() > MAX_SOURCE_REF_BYTES)
                                .unwrap_or(true)
                    })
                {
                    return Err(SessionV2Error::InvalidRecord);
                }
            }
            SessionRecordKind::PendingOperation { code }
            | SessionRecordKind::OperationState { code } => {
                if !valid_closed_code(code) {
                    return Err(SessionV2Error::InvalidRecord);
                }
            }
            SessionRecordKind::TurnTerminal { error_code, .. } => {
                if error_code
                    .as_ref()
                    .is_some_and(|code| !valid_closed_code(code))
                {
                    return Err(SessionV2Error::InvalidRecord);
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexPageRef {
    pub page_id: String,
    pub sha256: String,
    pub encrypted_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestLease {
    pub lease_id: String,
    pub holder_binding: String,
    pub fence: u64,
    pub expires_at_server_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionManifestV1 {
    pub manifest_version: u32,
    pub session_id: String,
    pub generation: u64,
    pub active_lease: Option<ManifestLease>,
    pub visible_index_head: Option<IndexPageRef>,
    pub visible_record_head: Option<String>,
    pub request_index_head: Option<IndexPageRef>,
    pub uuid_binding_index_head: Option<IndexPageRef>,
    pub visible_record_count: u64,
    pub internal_record_count: u64,
    pub visible_encrypted_bytes: u64,
    pub internal_encrypted_bytes: u64,
    pub archived: bool,
}

impl SessionManifestV1 {
    pub fn empty(session_id: impl Into<String>) -> Self {
        Self {
            manifest_version: SESSION_MANIFEST_VERSION,
            session_id: session_id.into(),
            generation: 0,
            active_lease: None,
            visible_index_head: None,
            visible_record_head: None,
            request_index_head: None,
            uuid_binding_index_head: None,
            visible_record_count: 0,
            internal_record_count: 0,
            visible_encrypted_bytes: 0,
            internal_encrypted_bytes: 0,
            archived: false,
        }
    }

    pub fn validate(&self) -> Result<(), SessionV2Error> {
        let records = self
            .visible_record_count
            .checked_add(self.internal_record_count)
            .ok_or(SessionV2Error::SessionLimit)?;
        let bytes = self
            .visible_encrypted_bytes
            .checked_add(self.internal_encrypted_bytes)
            .ok_or(SessionV2Error::SessionLimit)?;
        if self.manifest_version != SESSION_MANIFEST_VERSION
            || self.session_id.is_empty()
            || records > MAX_SESSION_RECORDS
            || bytes > MAX_SESSION_ENCRYPTED_BYTES
        {
            return Err(SessionV2Error::SessionLimit);
        }
        Ok(())
    }

    pub fn apply_delta(&self, delta: &ManifestDelta) -> Result<Self, SessionV2Error> {
        let mut next = self.clone();
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or(SessionV2Error::SessionLimit)?;
        next.visible_record_count = add_signed(next.visible_record_count, delta.visible_records)?;
        next.internal_record_count =
            add_signed(next.internal_record_count, delta.internal_records)?;
        next.visible_encrypted_bytes =
            add_signed(next.visible_encrypted_bytes, delta.visible_bytes)?;
        next.internal_encrypted_bytes =
            add_signed(next.internal_encrypted_bytes, delta.internal_bytes)?;
        if let Some(value) = &delta.visible_index_head {
            next.visible_index_head = value.clone();
        }
        if let Some(value) = &delta.visible_record_head {
            next.visible_record_head = value.clone();
        }
        if let Some(value) = &delta.request_index_head {
            next.request_index_head = value.clone();
        }
        if let Some(value) = &delta.uuid_binding_index_head {
            next.uuid_binding_index_head = value.clone();
        }
        next.validate()?;
        Ok(next)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManifestDelta {
    pub visible_records: i64,
    pub internal_records: i64,
    pub visible_bytes: i64,
    pub internal_bytes: i64,
    pub visible_index_head: Option<Option<IndexPageRef>>,
    pub visible_record_head: Option<Option<String>>,
    pub request_index_head: Option<Option<IndexPageRef>>,
    pub uuid_binding_index_head: Option<Option<IndexPageRef>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionedManifest {
    pub etag: String,
    pub manifest: SessionManifestV1,
}

#[derive(Debug, Clone)]
pub struct TrustedServerTimeSample {
    pub server_unix_ms: u64,
    pub received_at_monotonic: std::time::Instant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImmutableIndexPageV1 {
    pub index_version: u32,
    pub page_id: String,
    pub previous: Option<IndexPageRef>,
    pub entries: Vec<ImmutableIndexEntryV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImmutableIndexEntryV1 {
    pub object_id: String,
    pub object_sha256: String,
    pub encrypted_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestUuidBindingV1 {
    pub binding_version: u32,
    pub request_id: String,
    pub route_domain: String,
    pub session_scope: String,
    pub request_key: String,
    pub payload_digest: String,
}

impl RequestUuidBindingV1 {
    pub fn new(
        route: RequestRouteDomain,
        session_scope: &str,
        request_id: &str,
        payload_digest: String,
    ) -> Result<Self, SessionV2Error> {
        Ok(Self {
            binding_version: 1,
            request_id: request_id.to_owned(),
            route_domain: route.canonical().to_owned(),
            session_scope: session_scope.to_owned(),
            request_key: request_key(route, session_scope, request_id)?,
            payload_digest,
        })
    }

    pub fn permits_replay(&self, candidate: &Self) -> Result<(), SessionV2Error> {
        if self == candidate {
            Ok(())
        } else {
            Err(SessionV2Error::RequestConflict)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdempotencyTombstoneV1 {
    pub tombstone_version: u32,
    pub route_domain: String,
    pub session_scope: String,
    pub request_key: String,
    pub payload_digest: String,
    pub terminal_status: TurnTerminalStatus,
    pub public_result_digest: String,
    pub visible_record_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCommitV1 {
    pub visible_records: Vec<SessionRecordV2>,
    pub request_objects: Vec<(String, Vec<u8>)>,
    pub uuid_bindings: Vec<RequestUuidBindingV1>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryPageV1 {
    pub records: Vec<SessionRecordV2>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RequestReplayV1 {
    pub binding: RequestUuidBindingV1,
    pub journal: Option<RequestJournalV1>,
    pub tombstone: Option<IdempotencyTombstoneV1>,
    pub outcomes: Vec<RequestStepOutcomeV1>,
    pub visible_records: Vec<SessionRecordV2>,
}

#[derive(Clone)]
pub struct HistoryCursorCodec {
    key: hmac::Key,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryCursorPayloadV1 {
    version: u32,
    session_id: String,
    manifest_generation: u64,
    next_offset: usize,
}

impl HistoryCursorCodec {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            key: hmac::Key::new(hmac::HMAC_SHA256, key),
        }
    }

    fn encode(&self, payload: &HistoryCursorPayloadV1) -> Result<String, SessionV2Error> {
        let bytes = serde_json::to_vec(payload).map_err(|_| SessionV2Error::InvalidCursor)?;
        let tag = hmac::sign(&self.key, &bytes);
        Ok(format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(bytes),
            URL_SAFE_NO_PAD.encode(tag.as_ref())
        ))
    }

    fn decode(&self, cursor: &str) -> Result<HistoryCursorPayloadV1, SessionV2Error> {
        let (payload, tag) = cursor
            .split_once('.')
            .ok_or(SessionV2Error::InvalidCursor)?;
        let payload = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| SessionV2Error::InvalidCursor)?;
        let tag = URL_SAFE_NO_PAD
            .decode(tag)
            .map_err(|_| SessionV2Error::InvalidCursor)?;
        hmac::verify(&self.key, &payload, &tag).map_err(|_| SessionV2Error::InvalidCursor)?;
        serde_json::from_slice(&payload).map_err(|_| SessionV2Error::InvalidCursor)
    }
}

pub struct SessionV2Store<T: SessionV2Transport> {
    transport: T,
    cursor_codec: HistoryCursorCodec,
    object_crypto: SessionObjectCrypto,
}

impl<T: SessionV2Transport + Clone> Clone for SessionV2Store<T> {
    fn clone(&self) -> Self {
        Self {
            transport: self.transport.clone(),
            cursor_codec: self.cursor_codec.clone(),
            object_crypto: self.object_crypto.clone(),
        }
    }
}

impl<T: SessionV2Transport> SessionV2Store<T> {
    pub fn new(transport: T, cursor_key: &[u8; 32], object_crypto: SessionObjectCrypto) -> Self {
        Self {
            transport,
            cursor_codec: HistoryCursorCodec::new(cursor_key),
            object_crypto,
        }
    }

    pub fn publish(
        &self,
        current: &VersionedManifest,
        commit: SessionCommitV1,
    ) -> Result<VersionedManifest, SessionV2Error> {
        if commit.visible_records.is_empty()
            && commit.request_objects.is_empty()
            && commit.uuid_bindings.is_empty()
        {
            return Err(SessionV2Error::InvalidRecord);
        }

        let mut visible_entries = Vec::with_capacity(commit.visible_records.len());
        for record in &commit.visible_records {
            record.validate()?;
            if record.session_id != current.manifest.session_id {
                return Err(SessionV2Error::InvalidRecord);
            }
            let plaintext =
                serde_json::to_vec(record).map_err(|_| SessionV2Error::InvalidRecord)?;
            visible_entries.push(self.stage_sealed(
                &current.manifest.session_id,
                SessionObjectClass::VisibleRecord,
                &record.record_id,
                &plaintext,
            )?);
        }

        let mut request_entries = Vec::with_capacity(commit.request_objects.len());
        for (object_id, bytes) in &commit.request_objects {
            if !valid_ulid(object_id) || bytes.len() > MAX_REQUEST_OUTCOME_BYTES as usize {
                return Err(SessionV2Error::InvalidJournal);
            }
            request_entries.push(self.stage_sealed(
                &current.manifest.session_id,
                SessionObjectClass::RequestState,
                object_id,
                bytes,
            )?);
        }

        let mut binding_entries = Vec::with_capacity(commit.uuid_bindings.len());
        for binding in &commit.uuid_bindings {
            if binding.binding_version != 1 || !valid_uuid_v4(&binding.request_id) {
                return Err(SessionV2Error::InvalidRequestId);
            }
            let plaintext =
                serde_json::to_vec(binding).map_err(|_| SessionV2Error::InvalidRecord)?;
            binding_entries.push(self.stage_sealed(
                &current.manifest.session_id,
                SessionObjectClass::UuidBinding,
                &binding.request_key,
                &plaintext,
            )?);
        }

        let visible_page = self.stage_index_page(
            &current.manifest.session_id,
            SessionObjectClass::VisibleIndex,
            current.manifest.visible_index_head.clone(),
            visible_entries.clone(),
        )?;
        let request_page = self.stage_index_page(
            &current.manifest.session_id,
            SessionObjectClass::RequestIndex,
            current.manifest.request_index_head.clone(),
            request_entries.clone(),
        )?;
        let binding_page = self.stage_index_page(
            &current.manifest.session_id,
            SessionObjectClass::UuidBindingIndex,
            current.manifest.uuid_binding_index_head.clone(),
            binding_entries.clone(),
        )?;

        let visible_bytes = page_delta_bytes(&visible_page)
            .checked_add(request_entries_bytes(&visible_entries)?)
            .ok_or(SessionV2Error::SessionLimit)?;
        let internal_bytes = page_delta_bytes(&request_page)
            .checked_add(page_delta_bytes(&binding_page))
            .and_then(|value| {
                request_entries_bytes(&request_entries)
                    .ok()
                    .and_then(|bytes| value.checked_add(bytes))
            })
            .and_then(|value| {
                request_entries_bytes(&binding_entries)
                    .ok()
                    .and_then(|bytes| value.checked_add(bytes))
            })
            .ok_or(SessionV2Error::SessionLimit)?;
        let next = current.manifest.apply_delta(&ManifestDelta {
            visible_records: commit.visible_records.len() as i64,
            internal_records: (commit.request_objects.len() + commit.uuid_bindings.len()) as i64,
            visible_bytes: i64::try_from(visible_bytes)
                .map_err(|_| SessionV2Error::SessionLimit)?,
            internal_bytes: i64::try_from(internal_bytes)
                .map_err(|_| SessionV2Error::SessionLimit)?,
            visible_index_head: visible_page.clone().map(Some),
            visible_record_head: commit
                .visible_records
                .last()
                .map(|record| Some(record.record_id.clone())),
            request_index_head: request_page.clone().map(Some),
            uuid_binding_index_head: binding_page.clone().map(Some),
        })?;
        self.transport
            .compare_and_swap_manifest(&current.etag, &next)?
            .ok_or(SessionV2Error::ManifestConflict)
    }

    pub fn publish_terminal_compacted(
        &self,
        current: &VersionedManifest,
        visible_records: Vec<SessionRecordV2>,
        request_id: &str,
        tombstone: IdempotencyTombstoneV1,
    ) -> Result<VersionedManifest, SessionV2Error> {
        if visible_records.is_empty()
            || !valid_uuid_v4(request_id)
            || tombstone.tombstone_version != 1
            || tombstone.session_scope != current.manifest.session_id
        {
            return Err(SessionV2Error::InvalidRecord);
        }
        let mut visible_entries = Vec::with_capacity(visible_records.len());
        for record in &visible_records {
            record.validate()?;
            if record.session_id != current.manifest.session_id || record.request_id != request_id {
                return Err(SessionV2Error::InvalidRecord);
            }
            let plaintext =
                serde_json::to_vec(record).map_err(|_| SessionV2Error::InvalidRecord)?;
            visible_entries.push(self.stage_sealed(
                &current.manifest.session_id,
                SessionObjectClass::VisibleRecord,
                &record.record_id,
                &plaintext,
            )?);
        }

        let (old_entries, old_page_bytes) = self.load_index_entries_with_page_bytes(
            &current.manifest.session_id,
            SessionObjectClass::RequestIndex,
            current.manifest.request_index_head.as_ref(),
        )?;
        let mut removed = BTreeSet::new();
        let mut retained_outcomes = BTreeSet::new();
        let mut found_request_state = false;
        for entry in &old_entries {
            let bytes = self.load_indexed_object(
                &current.manifest.session_id,
                SessionObjectClass::RequestState,
                entry,
            )?;
            if let Ok(existing) = serde_json::from_slice::<IdempotencyTombstoneV1>(&bytes) {
                if existing.request_key == tombstone.request_key {
                    found_request_state = true;
                    removed.insert(entry.object_id.clone());
                }
                continue;
            }
            if let Ok(journal) = serde_json::from_slice::<RequestJournalV1>(&bytes) {
                if journal.request_id == request_id {
                    found_request_state = true;
                    removed.insert(entry.object_id.clone());
                    removed.extend(
                        journal
                            .completed_steps
                            .into_iter()
                            .map(|step| step.outcome_id),
                    );
                } else {
                    retained_outcomes.extend(
                        journal
                            .completed_steps
                            .into_iter()
                            .map(|step| step.outcome_id),
                    );
                }
            }
        }
        if !found_request_state || removed.iter().any(|id| retained_outcomes.contains(id)) {
            return Err(SessionV2Error::InvalidJournal);
        }
        let mut retained = old_entries
            .iter()
            .filter(|entry| !removed.contains(&entry.object_id))
            .cloned()
            .collect::<Vec<_>>();
        let tombstone_id = crate::session::new_ulid().map_err(|_| SessionV2Error::InvalidRecord)?;
        let tombstone_bytes =
            serde_json::to_vec(&tombstone).map_err(|_| SessionV2Error::InvalidRecord)?;
        retained.push(self.stage_sealed(
            &current.manifest.session_id,
            SessionObjectClass::RequestState,
            &tombstone_id,
            &tombstone_bytes,
        )?);

        let visible_page = self.stage_index_page(
            &current.manifest.session_id,
            SessionObjectClass::VisibleIndex,
            current.manifest.visible_index_head.clone(),
            visible_entries.clone(),
        )?;
        let request_page = self.stage_index_page(
            &current.manifest.session_id,
            SessionObjectClass::RequestIndex,
            None,
            retained.clone(),
        )?;
        let old_request_bytes = old_page_bytes
            .checked_add(request_entries_bytes(&old_entries)?)
            .ok_or(SessionV2Error::SessionLimit)?;
        let new_request_bytes = page_delta_bytes(&request_page)
            .checked_add(request_entries_bytes(&retained)?)
            .ok_or(SessionV2Error::SessionLimit)?;
        let visible_bytes = page_delta_bytes(&visible_page)
            .checked_add(request_entries_bytes(&visible_entries)?)
            .ok_or(SessionV2Error::SessionLimit)?;
        let next = current.manifest.apply_delta(&ManifestDelta {
            visible_records: visible_records.len() as i64,
            internal_records: i64::try_from(retained.len())
                .and_then(|new| i64::try_from(old_entries.len()).map(|old| new - old))
                .map_err(|_| SessionV2Error::SessionLimit)?,
            visible_bytes: i64::try_from(visible_bytes)
                .map_err(|_| SessionV2Error::SessionLimit)?,
            internal_bytes: i64::try_from(
                i128::from(new_request_bytes) - i128::from(old_request_bytes),
            )
            .map_err(|_| SessionV2Error::SessionLimit)?,
            visible_index_head: visible_page.map(Some),
            visible_record_head: visible_records
                .last()
                .map(|record| Some(record.record_id.clone())),
            request_index_head: request_page.map(Some),
            uuid_binding_index_head: None,
        })?;
        self.transport
            .compare_and_swap_manifest(&current.etag, &next)?
            .ok_or(SessionV2Error::ManifestConflict)
    }

    pub fn current_manifest(&self, session_id: &str) -> Result<VersionedManifest, SessionV2Error> {
        self.transport.load_manifest(session_id)
    }

    pub fn history(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> Result<HistoryPageV1, SessionV2Error> {
        let current = self.transport.load_manifest(session_id)?;
        let limit = limit.unwrap_or(DEFAULT_HISTORY_PAGE_SIZE);
        if limit == 0 || limit > MAX_HISTORY_PAGE_SIZE {
            return Err(SessionV2Error::HistoryPageTooLarge);
        }
        let offset = match cursor {
            Some(cursor) => {
                let payload = self.cursor_codec.decode(cursor)?;
                if payload.version != 1
                    || payload.session_id != session_id
                    || payload.manifest_generation != current.manifest.generation
                {
                    return Err(SessionV2Error::InvalidCursor);
                }
                payload.next_offset
            }
            None => 0,
        };
        let entries = self.load_index_entries(
            session_id,
            SessionObjectClass::VisibleIndex,
            current.manifest.visible_index_head.as_ref(),
        )?;
        if offset > entries.len() {
            return Err(SessionV2Error::InvalidCursor);
        }
        let mut records = Vec::new();
        let mut response_bytes = 0usize;
        for entry in entries.iter().skip(offset).take(limit) {
            let sealed = self
                .transport
                .load_immutable(SessionObjectClass::VisibleRecord, &entry.object_id)?;
            if bytes_digest(&sealed) != entry.object_sha256 {
                return Err(SessionV2Error::InvalidRecord);
            }
            let bytes = self
                .object_crypto
                .open(
                    session_id,
                    SessionObjectClass::VisibleRecord,
                    &entry.object_id,
                    &sealed,
                )
                .map_err(|_| SessionV2Error::InvalidRecord)?;
            response_bytes = response_bytes
                .checked_add(bytes.len())
                .ok_or(SessionV2Error::HistoryPageTooLarge)?;
            if response_bytes > MAX_HISTORY_RESPONSE_BYTES {
                return Err(SessionV2Error::HistoryPageTooLarge);
            }
            let record: SessionRecordV2 =
                serde_json::from_slice(&bytes).map_err(|_| SessionV2Error::InvalidRecord)?;
            record.validate()?;
            records.push(record);
        }
        let next_offset = offset + records.len();
        let next_cursor = if next_offset < entries.len() {
            Some(self.cursor_codec.encode(&HistoryCursorPayloadV1 {
                version: 1,
                session_id: session_id.to_owned(),
                manifest_generation: current.manifest.generation,
                next_offset,
            })?)
        } else {
            None
        };
        Ok(HistoryPageV1 {
            records,
            next_cursor,
        })
    }

    /// Load the newest visible records in chronological order for provider-context
    /// reconstruction. This deliberately bypasses public cursor pagination while
    /// retaining the same integrity and response-byte checks.
    pub fn recent_visible_records(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<SessionRecordV2>, SessionV2Error> {
        if limit == 0 || limit > MAX_CONTEXT_MESSAGES.saturating_mul(2) {
            return Err(SessionV2Error::HistoryPageTooLarge);
        }
        let current = self.transport.load_manifest(session_id)?;
        let entries = self.load_index_entries(
            session_id,
            SessionObjectClass::VisibleIndex,
            current.manifest.visible_index_head.as_ref(),
        )?;
        let start = entries.len().saturating_sub(limit);
        let mut records = Vec::with_capacity(entries.len() - start);
        let mut response_bytes = 0usize;
        for entry in &entries[start..] {
            let sealed = self
                .transport
                .load_immutable(SessionObjectClass::VisibleRecord, &entry.object_id)?;
            if bytes_digest(&sealed) != entry.object_sha256 {
                return Err(SessionV2Error::InvalidRecord);
            }
            let bytes = self
                .object_crypto
                .open(
                    session_id,
                    SessionObjectClass::VisibleRecord,
                    &entry.object_id,
                    &sealed,
                )
                .map_err(|_| SessionV2Error::InvalidRecord)?;
            response_bytes = response_bytes
                .checked_add(bytes.len())
                .ok_or(SessionV2Error::HistoryPageTooLarge)?;
            if response_bytes > MAX_HISTORY_RESPONSE_BYTES {
                return Err(SessionV2Error::HistoryPageTooLarge);
            }
            let record: SessionRecordV2 =
                serde_json::from_slice(&bytes).map_err(|_| SessionV2Error::InvalidRecord)?;
            record.validate()?;
            records.push(record);
        }
        Ok(records)
    }

    pub fn load_request_chain(
        &self,
        journal: &RequestJournalV1,
    ) -> Result<Vec<RequestStepOutcomeV1>, SessionV2Error> {
        journal.validate_chain_with(|outcome_id| {
            let sealed = self
                .transport
                .load_immutable(SessionObjectClass::RequestState, outcome_id)?;
            self.object_crypto
                .open(
                    &journal.session_id,
                    SessionObjectClass::RequestState,
                    outcome_id,
                    &sealed,
                )
                .map_err(|_| SessionV2Error::InvalidJournal)
        })
    }

    pub fn request_replay(
        &self,
        session_id: &str,
        candidate: &RequestUuidBindingV1,
    ) -> Result<Option<RequestReplayV1>, SessionV2Error> {
        let current = self.transport.load_manifest(session_id)?;
        let binding_entries = self.load_index_entries(
            session_id,
            SessionObjectClass::UuidBindingIndex,
            current.manifest.uuid_binding_index_head.as_ref(),
        )?;
        let mut matching_binding = None;
        for entry in binding_entries.iter().rev() {
            let bytes =
                self.load_indexed_object(session_id, SessionObjectClass::UuidBinding, entry)?;
            let binding: RequestUuidBindingV1 =
                serde_json::from_slice(&bytes).map_err(|_| SessionV2Error::InvalidRecord)?;
            if binding.binding_version != 1
                || binding.request_key != entry.object_id
                || !valid_uuid_v4(&binding.request_id)
            {
                return Err(SessionV2Error::InvalidRecord);
            }
            if binding.request_id == candidate.request_id {
                binding.permits_replay(candidate)?;
                matching_binding = Some(binding);
                break;
            }
        }
        let Some(binding) = matching_binding else {
            return Ok(None);
        };

        let request_entries = self.load_index_entries(
            session_id,
            SessionObjectClass::RequestIndex,
            current.manifest.request_index_head.as_ref(),
        )?;
        let mut journal = None;
        let mut tombstone = None;
        for entry in request_entries.iter().rev() {
            let bytes =
                self.load_indexed_object(session_id, SessionObjectClass::RequestState, entry)?;
            if let Ok(candidate_tombstone) =
                serde_json::from_slice::<IdempotencyTombstoneV1>(&bytes)
            {
                if candidate_tombstone.request_key == binding.request_key {
                    tombstone = Some(candidate_tombstone);
                    break;
                }
                continue;
            }
            let Ok(candidate_journal) = serde_json::from_slice::<RequestJournalV1>(&bytes) else {
                continue;
            };
            if candidate_journal.session_id == session_id
                && candidate_journal.request_id == candidate.request_id
            {
                self.load_request_chain(&candidate_journal)?;
                journal = Some(candidate_journal);
                break;
            }
        }
        let outcomes = journal
            .as_ref()
            .map(|journal| self.load_request_chain(journal))
            .transpose()?
            .unwrap_or_default();
        if journal.is_none() && tombstone.is_none() {
            return Err(SessionV2Error::InvalidJournal);
        }

        let visible_entries = self.load_index_entries(
            session_id,
            SessionObjectClass::VisibleIndex,
            current.manifest.visible_index_head.as_ref(),
        )?;
        let mut visible_records = Vec::new();
        for entry in &visible_entries {
            let bytes =
                self.load_indexed_object(session_id, SessionObjectClass::VisibleRecord, entry)?;
            let record: SessionRecordV2 =
                serde_json::from_slice(&bytes).map_err(|_| SessionV2Error::InvalidRecord)?;
            record.validate()?;
            if record.request_id == candidate.request_id {
                visible_records.push(record);
            }
        }
        Ok(Some(RequestReplayV1 {
            binding,
            journal,
            tombstone,
            outcomes,
            visible_records,
        }))
    }

    fn stage_index_page(
        &self,
        session_id: &str,
        object_class: SessionObjectClass,
        previous: Option<IndexPageRef>,
        entries: Vec<ImmutableIndexEntryV1>,
    ) -> Result<Option<IndexPageRef>, SessionV2Error> {
        if entries.is_empty() {
            return Ok(None);
        }
        let page_id = deterministic_page_id(previous.as_ref(), &entries)?;
        let page = ImmutableIndexPageV1 {
            index_version: 1,
            page_id: page_id.clone(),
            previous,
            entries,
        };
        let bytes = serde_json::to_vec(&page).map_err(|_| SessionV2Error::InvalidRecord)?;
        let sealed = self
            .object_crypto
            .seal(session_id, object_class, &page_id, &bytes)
            .map_err(|_| SessionV2Error::InvalidRecord)?;
        self.transport
            .stage_immutable(object_class, &page_id, &sealed)?;
        Ok(Some(IndexPageRef {
            page_id,
            sha256: bytes_digest(&sealed),
            encrypted_bytes: sealed.len() as u64,
        }))
    }

    fn load_index_entries(
        &self,
        session_id: &str,
        object_class: SessionObjectClass,
        head: Option<&IndexPageRef>,
    ) -> Result<Vec<ImmutableIndexEntryV1>, SessionV2Error> {
        let mut pages = Vec::new();
        let mut cursor = head.cloned();
        let mut seen = BTreeSet::new();
        while let Some(reference) = cursor {
            if !seen.insert(reference.page_id.clone()) {
                return Err(SessionV2Error::InvalidRecord);
            }
            let sealed = self
                .transport
                .load_immutable(object_class, &reference.page_id)?;
            if bytes_digest(&sealed) != reference.sha256 {
                return Err(SessionV2Error::InvalidRecord);
            }
            let bytes = self
                .object_crypto
                .open(session_id, object_class, &reference.page_id, &sealed)
                .map_err(|_| SessionV2Error::InvalidRecord)?;
            let page: ImmutableIndexPageV1 =
                serde_json::from_slice(&bytes).map_err(|_| SessionV2Error::InvalidRecord)?;
            if page.index_version != 1 || page.page_id != reference.page_id {
                return Err(SessionV2Error::InvalidRecord);
            }
            cursor = page.previous.clone();
            pages.push(page.entries);
            if pages.len() > MAX_SESSION_RECORDS as usize {
                return Err(SessionV2Error::SessionLimit);
            }
        }
        pages.reverse();
        Ok(pages.into_iter().flatten().collect())
    }

    fn load_index_entries_with_page_bytes(
        &self,
        session_id: &str,
        object_class: SessionObjectClass,
        head: Option<&IndexPageRef>,
    ) -> Result<(Vec<ImmutableIndexEntryV1>, u64), SessionV2Error> {
        let mut page_bytes = 0u64;
        let mut cursor = head.cloned();
        let mut seen = BTreeSet::new();
        while let Some(reference) = cursor {
            if !seen.insert(reference.page_id.clone()) {
                return Err(SessionV2Error::InvalidRecord);
            }
            page_bytes = page_bytes
                .checked_add(reference.encrypted_bytes)
                .ok_or(SessionV2Error::SessionLimit)?;
            let sealed = self
                .transport
                .load_immutable(object_class, &reference.page_id)?;
            if bytes_digest(&sealed) != reference.sha256
                || sealed.len() as u64 != reference.encrypted_bytes
            {
                return Err(SessionV2Error::InvalidRecord);
            }
            let bytes = self
                .object_crypto
                .open(session_id, object_class, &reference.page_id, &sealed)
                .map_err(|_| SessionV2Error::InvalidRecord)?;
            let page: ImmutableIndexPageV1 =
                serde_json::from_slice(&bytes).map_err(|_| SessionV2Error::InvalidRecord)?;
            if page.index_version != 1 || page.page_id != reference.page_id {
                return Err(SessionV2Error::InvalidRecord);
            }
            cursor = page.previous;
        }
        Ok((
            self.load_index_entries(session_id, object_class, head)?,
            page_bytes,
        ))
    }

    fn collect_reachable_index(
        &self,
        session_id: &str,
        index_class: SessionObjectClass,
        object_class: SessionObjectClass,
        head: Option<&IndexPageRef>,
        reachable: &mut BTreeSet<(SessionObjectClass, String)>,
    ) -> Result<(), SessionV2Error> {
        let mut cursor = head.cloned();
        let mut seen = BTreeSet::new();
        while let Some(reference) = cursor {
            if !seen.insert(reference.page_id.clone()) {
                return Err(SessionV2Error::InvalidRecord);
            }
            let sealed = self
                .transport
                .load_immutable(index_class, &reference.page_id)?;
            if bytes_digest(&sealed) != reference.sha256
                || sealed.len() as u64 != reference.encrypted_bytes
            {
                return Err(SessionV2Error::InvalidRecord);
            }
            let bytes = self
                .object_crypto
                .open(session_id, index_class, &reference.page_id, &sealed)
                .map_err(|_| SessionV2Error::InvalidRecord)?;
            let page: ImmutableIndexPageV1 =
                serde_json::from_slice(&bytes).map_err(|_| SessionV2Error::InvalidRecord)?;
            if page.index_version != 1 || page.page_id != reference.page_id {
                return Err(SessionV2Error::InvalidRecord);
            }
            reachable.insert((index_class, reference.page_id));
            reachable.extend(
                page.entries
                    .into_iter()
                    .map(|entry| (object_class, entry.object_id)),
            );
            cursor = page.previous;
        }
        Ok(())
    }

    fn reachable_objects(
        &self,
        manifest: &SessionManifestV1,
    ) -> Result<BTreeSet<(SessionObjectClass, String)>, SessionV2Error> {
        let mut reachable = BTreeSet::new();
        self.collect_reachable_index(
            &manifest.session_id,
            SessionObjectClass::VisibleIndex,
            SessionObjectClass::VisibleRecord,
            manifest.visible_index_head.as_ref(),
            &mut reachable,
        )?;
        self.collect_reachable_index(
            &manifest.session_id,
            SessionObjectClass::RequestIndex,
            SessionObjectClass::RequestState,
            manifest.request_index_head.as_ref(),
            &mut reachable,
        )?;
        self.collect_reachable_index(
            &manifest.session_id,
            SessionObjectClass::UuidBindingIndex,
            SessionObjectClass::UuidBinding,
            manifest.uuid_binding_index_head.as_ref(),
            &mut reachable,
        )?;
        Ok(reachable)
    }

    fn load_indexed_object(
        &self,
        session_id: &str,
        object_class: SessionObjectClass,
        entry: &ImmutableIndexEntryV1,
    ) -> Result<Vec<u8>, SessionV2Error> {
        let sealed = self
            .transport
            .load_immutable(object_class, &entry.object_id)?;
        if bytes_digest(&sealed) != entry.object_sha256
            || sealed.len() as u64 != entry.encrypted_bytes
        {
            return Err(SessionV2Error::InvalidRecord);
        }
        self.object_crypto
            .open(session_id, object_class, &entry.object_id, &sealed)
            .map_err(|_| SessionV2Error::InvalidRecord)
    }

    fn stage_sealed(
        &self,
        session_id: &str,
        object_class: SessionObjectClass,
        object_id: &str,
        plaintext: &[u8],
    ) -> Result<ImmutableIndexEntryV1, SessionV2Error> {
        let sealed = self
            .object_crypto
            .seal(session_id, object_class, object_id, plaintext)
            .map_err(|_| SessionV2Error::InvalidRecord)?;
        self.transport
            .stage_immutable(object_class, object_id, &sealed)?;
        Ok(index_entry(object_id, &sealed))
    }
}

#[derive(Debug)]
struct LeaseGuardState {
    current: VersionedManifest,
    lease: ManifestLease,
    last_server_time_ms: u64,
    lost: bool,
}

pub struct SessionLeaseGuard<T: SessionV2Transport + Clone + 'static> {
    store: SessionV2Store<T>,
    state: Arc<Mutex<LeaseGuardState>>,
    stop: Option<std::sync::mpsc::Sender<()>>,
    renewal: Option<std::thread::JoinHandle<()>>,
}

impl<T: SessionV2Transport + Clone + 'static> SessionV2Store<T> {
    pub fn archive_session(&self, session_id: &str) -> Result<(), SessionV2Error> {
        let current = self.transport.load_manifest(session_id)?;
        if current.manifest.archived {
            return Ok(());
        }
        let now = fresh_server_time(&self.transport)?;
        if current.manifest.active_lease.as_ref().is_some_and(|lease| {
            now <= lease
                .expires_at_server_ms
                .saturating_add(SESSION_LEASE_TAKEOVER_MARGIN_MS)
        }) {
            return Err(SessionV2Error::ManifestConflict);
        }
        let mut next = current.manifest.clone();
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or(SessionV2Error::SessionLimit)?;
        next.active_lease = None;
        next.archived = true;
        next.validate()?;
        self.transport
            .compare_and_swap_manifest(&current.etag, &next)?
            .ok_or(SessionV2Error::ManifestConflict)?;
        Ok(())
    }

    pub fn acquire_lease(
        &self,
        session_id: &str,
        lease_id: &str,
        holder_binding: &str,
    ) -> Result<SessionLeaseGuard<T>, SessionV2Error> {
        self.acquire_lease_with_interval(
            session_id,
            lease_id,
            holder_binding,
            SESSION_LEASE_RENEW_INTERVAL,
        )
    }

    fn acquire_lease_with_interval(
        &self,
        session_id: &str,
        lease_id: &str,
        holder_binding: &str,
        renewal_interval: std::time::Duration,
    ) -> Result<SessionLeaseGuard<T>, SessionV2Error> {
        if lease_id.is_empty() || holder_binding.is_empty() {
            return Err(SessionV2Error::LeaseLost);
        }
        let current = self.transport.load_manifest(session_id)?;
        let now = fresh_server_time(&self.transport)?;
        if current.manifest.active_lease.as_ref().is_some_and(|lease| {
            now <= lease
                .expires_at_server_ms
                .saturating_add(SESSION_LEASE_TAKEOVER_MARGIN_MS)
        }) {
            return Err(SessionV2Error::ManifestConflict);
        }
        let fence = current
            .manifest
            .generation
            .checked_add(1)
            .ok_or(SessionV2Error::SessionLimit)?;
        if fence == u64::MAX {
            return Err(SessionV2Error::SessionLimit);
        }
        let lease = ManifestLease {
            lease_id: lease_id.to_owned(),
            holder_binding: holder_binding.to_owned(),
            fence,
            expires_at_server_ms: now
                .checked_add(SESSION_LEASE_TTL_MS)
                .ok_or(SessionV2Error::SessionLimit)?,
        };
        let mut next = current.manifest.clone();
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or(SessionV2Error::SessionLimit)?;
        next.active_lease = Some(lease.clone());
        next.validate()?;
        let current = self
            .transport
            .compare_and_swap_manifest(&current.etag, &next)?
            .ok_or(SessionV2Error::ManifestConflict)?;
        let state = Arc::new(Mutex::new(LeaseGuardState {
            current,
            lease,
            last_server_time_ms: now,
            lost: false,
        }));
        let (stop, stop_rx) = std::sync::mpsc::channel();
        let worker_store = self.clone();
        let worker_state = Arc::clone(&state);
        let renewal = std::thread::Builder::new()
            .name("agent-session-lease".into())
            .spawn(move || loop {
                match stop_rx.recv_timeout(renewal_interval) {
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if renew_lease(&worker_store, &worker_state).is_err() {
                            if let Ok(mut state) = worker_state.lock() {
                                state.lost = true;
                            }
                            break;
                        }
                    }
                }
            })
            .map_err(|_| SessionV2Error::LeaseLost)?;
        Ok(SessionLeaseGuard {
            store: self.clone(),
            state,
            stop: Some(stop),
            renewal: Some(renewal),
        })
    }
}

impl<T: SessionV2Transport + Clone + 'static> SessionLeaseGuard<T> {
    pub fn binding(&self) -> Result<PersistedLeaseBinding, SessionV2Error> {
        let state = self.state.lock().map_err(|_| SessionV2Error::LeaseLost)?;
        if state.lost {
            return Err(SessionV2Error::LeaseLost);
        }
        Ok(PersistedLeaseBinding {
            lease_id: state.lease.lease_id.clone(),
            holder_binding: state.lease.holder_binding.clone(),
            fence: state.lease.fence,
            expires_at_server_ms: state.lease.expires_at_server_ms,
        })
    }

    pub fn publish(&self, commit: SessionCommitV1) -> Result<(), SessionV2Error> {
        let now = fresh_server_time(&self.store.transport)?;
        let mut state = self.state.lock().map_err(|_| SessionV2Error::LeaseLost)?;
        observe_server_time(&mut state, now)?;
        if state.lost
            || now > state.lease.expires_at_server_ms
            || state.current.manifest.active_lease.as_ref() != Some(&state.lease)
        {
            state.lost = true;
            return Err(SessionV2Error::LeaseLost);
        }
        let expected = PersistedLeaseBinding {
            lease_id: state.lease.lease_id.clone(),
            holder_binding: state.lease.holder_binding.clone(),
            fence: state.lease.fence,
            expires_at_server_ms: state.lease.expires_at_server_ms,
        };
        if commit
            .visible_records
            .iter()
            .any(|record| record.lease != expected)
        {
            return Err(SessionV2Error::LeaseLost);
        }
        match self.store.publish(&state.current, commit) {
            Ok(current) => {
                state.current = current;
                Ok(())
            }
            Err(error) => {
                state.lost = true;
                Err(error)
            }
        }
    }

    pub fn publish_terminal_compacted(
        &self,
        visible_records: Vec<SessionRecordV2>,
        request_id: &str,
        tombstone: IdempotencyTombstoneV1,
    ) -> Result<(), SessionV2Error> {
        let now = fresh_server_time(&self.store.transport)?;
        let mut state = self.state.lock().map_err(|_| SessionV2Error::LeaseLost)?;
        observe_server_time(&mut state, now)?;
        if state.lost
            || now > state.lease.expires_at_server_ms
            || state.current.manifest.active_lease.as_ref() != Some(&state.lease)
        {
            state.lost = true;
            return Err(SessionV2Error::LeaseLost);
        }
        let expected = PersistedLeaseBinding {
            lease_id: state.lease.lease_id.clone(),
            holder_binding: state.lease.holder_binding.clone(),
            fence: state.lease.fence,
            expires_at_server_ms: state.lease.expires_at_server_ms,
        };
        if visible_records
            .iter()
            .any(|record| record.lease != expected)
        {
            return Err(SessionV2Error::LeaseLost);
        }
        match self.store.publish_terminal_compacted(
            &state.current,
            visible_records,
            request_id,
            tombstone,
        ) {
            Ok(current) => {
                state.current = current;
                Ok(())
            }
            Err(error) => {
                state.lost = true;
                Err(error)
            }
        }
    }

    pub fn reap_orphans(&self) -> Result<usize, SessionV2Error> {
        let now = fresh_server_time(&self.store.transport)?;
        let Some(cutoff) = now.checked_sub(ORPHAN_RETENTION_MS) else {
            return Ok(0);
        };
        let mut state = self.state.lock().map_err(|_| SessionV2Error::LeaseLost)?;
        observe_server_time(&mut state, now)?;
        if state.lost
            || now > state.lease.expires_at_server_ms
            || state.current.manifest.active_lease.as_ref() != Some(&state.lease)
        {
            return Err(SessionV2Error::LeaseLost);
        }
        let reachable = self.store.reachable_objects(&state.current.manifest)?;
        self.store
            .transport
            .reap_unreachable(&reachable, cutoff, ORPHAN_REAP_BATCH)
    }

    pub fn is_lost(&self) -> bool {
        self.state.lock().map_or(true, |state| state.lost)
    }
}

impl<T: SessionV2Transport + Clone + 'static> Drop for SessionLeaseGuard<T> {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(renewal) = self.renewal.take() {
            let _ = renewal.join();
        }
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.lost || state.current.manifest.active_lease.as_ref() != Some(&state.lease) {
            return;
        }
        let mut next = state.current.manifest.clone();
        let Some(generation) = next.generation.checked_add(1) else {
            return;
        };
        next.generation = generation;
        next.active_lease = None;
        if let Ok(Some(current)) = self
            .store
            .transport
            .compare_and_swap_manifest(&state.current.etag, &next)
        {
            state.current = current;
        }
    }
}

fn fresh_server_time<T: SessionV2Transport>(transport: &T) -> Result<u64, SessionV2Error> {
    let sample = transport.server_time_sample()?;
    if sample.received_at_monotonic.elapsed() > MAX_SERVER_TIME_SAMPLE_AGE {
        return Err(SessionV2Error::TransportUnavailable);
    }
    Ok(sample.server_unix_ms)
}

fn observe_server_time(state: &mut LeaseGuardState, now: u64) -> Result<(), SessionV2Error> {
    if now < state.last_server_time_ms {
        state.lost = true;
        return Err(SessionV2Error::TransportUnavailable);
    }
    state.last_server_time_ms = now;
    Ok(())
}

fn renew_lease<T: SessionV2Transport + Clone>(
    store: &SessionV2Store<T>,
    state: &Arc<Mutex<LeaseGuardState>>,
) -> Result<(), SessionV2Error> {
    let now = fresh_server_time(&store.transport)?;
    let mut state = state.lock().map_err(|_| SessionV2Error::LeaseLost)?;
    observe_server_time(&mut state, now)?;
    if state.lost || state.current.manifest.active_lease.as_ref() != Some(&state.lease) {
        return Err(SessionV2Error::LeaseLost);
    }
    let mut lease = state.lease.clone();
    lease.expires_at_server_ms = now
        .checked_add(SESSION_LEASE_TTL_MS)
        .ok_or(SessionV2Error::SessionLimit)?;
    let mut next = state.current.manifest.clone();
    next.generation = next
        .generation
        .checked_add(1)
        .ok_or(SessionV2Error::SessionLimit)?;
    next.active_lease = Some(lease.clone());
    let current = store
        .transport
        .compare_and_swap_manifest(&state.current.etag, &next)?
        .ok_or(SessionV2Error::LeaseLost)?;
    state.current = current;
    state.lease = lease;
    Ok(())
}

pub trait SessionV2Transport: Send + Sync {
    fn server_time_sample(&self) -> Result<TrustedServerTimeSample, SessionV2Error>;
    fn load_manifest(&self, session_id: &str) -> Result<VersionedManifest, SessionV2Error>;
    fn stage_immutable(
        &self,
        object_class: SessionObjectClass,
        object_id: &str,
        bytes: &[u8],
    ) -> Result<(), SessionV2Error>;
    fn load_immutable(
        &self,
        object_class: SessionObjectClass,
        object_id: &str,
    ) -> Result<Vec<u8>, SessionV2Error>;
    fn reap_unreachable(
        &self,
        reachable: &BTreeSet<(SessionObjectClass, String)>,
        older_than_server_ms: u64,
        limit: usize,
    ) -> Result<usize, SessionV2Error>;
    fn compare_and_swap_manifest(
        &self,
        expected_etag: &str,
        next: &SessionManifestV1,
    ) -> Result<Option<VersionedManifest>, SessionV2Error>;
}

#[derive(Clone, Default)]
pub struct InMemorySessionV2Transport {
    inner: Arc<Mutex<MemoryV2State>>,
}

#[derive(Default)]
struct MemoryV2State {
    manifests: BTreeMap<String, VersionedManifest>,
    objects: BTreeMap<(SessionObjectClass, String), Vec<u8>>,
    object_created_at_ms: BTreeMap<(SessionObjectClass, String), u64>,
    next_etag: u64,
    server_unix_ms: u64,
    server_sample_age: std::time::Duration,
    server_time_unavailable: bool,
}

impl InMemorySessionV2Transport {
    pub fn create_session(&self, session_id: &str) -> Result<VersionedManifest, SessionV2Error> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| SessionV2Error::ManifestConflict)?;
        if state.manifests.contains_key(session_id) {
            return Err(SessionV2Error::ManifestConflict);
        }
        state.next_etag = state
            .next_etag
            .checked_add(1)
            .ok_or(SessionV2Error::SessionLimit)?;
        let value = VersionedManifest {
            etag: format!("etag-{}", state.next_etag),
            manifest: SessionManifestV1::empty(session_id),
        };
        state.manifests.insert(session_id.to_owned(), value.clone());
        Ok(value)
    }

    pub fn object_count(&self) -> usize {
        self.inner
            .lock()
            .map(|state| state.objects.len())
            .unwrap_or_default()
    }

    pub fn set_server_time_ms(&self, server_unix_ms: u64) {
        if let Ok(mut state) = self.inner.lock() {
            state.server_unix_ms = server_unix_ms;
        }
    }

    #[cfg(test)]
    fn set_server_sample_age(&self, age: std::time::Duration) {
        if let Ok(mut state) = self.inner.lock() {
            state.server_sample_age = age;
        }
    }

    #[cfg(test)]
    fn set_server_time_unavailable(&self, unavailable: bool) {
        if let Ok(mut state) = self.inner.lock() {
            state.server_time_unavailable = unavailable;
        }
    }
}

impl SessionV2Transport for InMemorySessionV2Transport {
    fn server_time_sample(&self) -> Result<TrustedServerTimeSample, SessionV2Error> {
        let state = self
            .inner
            .lock()
            .map_err(|_| SessionV2Error::TransportUnavailable)?;
        if state.server_time_unavailable {
            return Err(SessionV2Error::TransportUnavailable);
        }
        Ok(TrustedServerTimeSample {
            server_unix_ms: state.server_unix_ms,
            received_at_monotonic: std::time::Instant::now() - state.server_sample_age,
        })
    }

    fn load_manifest(&self, session_id: &str) -> Result<VersionedManifest, SessionV2Error> {
        self.inner
            .lock()
            .map_err(|_| SessionV2Error::ManifestConflict)?
            .manifests
            .get(session_id)
            .cloned()
            .ok_or(SessionV2Error::ManifestConflict)
    }

    fn stage_immutable(
        &self,
        object_class: SessionObjectClass,
        object_id: &str,
        bytes: &[u8],
    ) -> Result<(), SessionV2Error> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| SessionV2Error::ManifestConflict)?;
        let key = (object_class, object_id.to_owned());
        if let Some(existing) = state.objects.get(&key) {
            return if existing == bytes {
                Ok(())
            } else {
                Err(SessionV2Error::ManifestConflict)
            };
        }
        let created_at_ms = state.server_unix_ms;
        state.objects.insert(key, bytes.to_vec());
        state
            .object_created_at_ms
            .insert((object_class, object_id.to_owned()), created_at_ms);
        Ok(())
    }

    fn load_immutable(
        &self,
        object_class: SessionObjectClass,
        object_id: &str,
    ) -> Result<Vec<u8>, SessionV2Error> {
        self.inner
            .lock()
            .map_err(|_| SessionV2Error::ManifestConflict)?
            .objects
            .get(&(object_class, object_id.to_owned()))
            .cloned()
            .ok_or(SessionV2Error::InvalidJournal)
    }

    fn reap_unreachable(
        &self,
        reachable: &BTreeSet<(SessionObjectClass, String)>,
        older_than_server_ms: u64,
        limit: usize,
    ) -> Result<usize, SessionV2Error> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| SessionV2Error::ManifestConflict)?;
        let candidates = state
            .object_created_at_ms
            .iter()
            .filter(|(key, created_at)| {
                **created_at <= older_than_server_ms && !reachable.contains(*key)
            })
            .take(limit)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in &candidates {
            state.objects.remove(key);
            state.object_created_at_ms.remove(key);
        }
        Ok(candidates.len())
    }

    fn compare_and_swap_manifest(
        &self,
        expected_etag: &str,
        next: &SessionManifestV1,
    ) -> Result<Option<VersionedManifest>, SessionV2Error> {
        next.validate()?;
        let mut state = self
            .inner
            .lock()
            .map_err(|_| SessionV2Error::ManifestConflict)?;
        let Some(current) = state.manifests.get(&next.session_id) else {
            return Ok(None);
        };
        if current.etag != expected_etag || next.generation != current.manifest.generation + 1 {
            return Ok(None);
        }
        state.next_etag = state
            .next_etag
            .checked_add(1)
            .ok_or(SessionV2Error::SessionLimit)?;
        let updated = VersionedManifest {
            etag: format!("etag-{}", state.next_etag),
            manifest: next.clone(),
        };
        state
            .manifests
            .insert(next.session_id.clone(), updated.clone());
        Ok(Some(updated))
    }
}

#[cfg(feature = "onedrive")]
mod onedrive_transport {
    use super::*;
    use isyncyou_graph::http::{ConflictBehavior, GraphClient};

    const V2_ROOT: &str = "Apps/iSyncYou/agent/v2";
    const MAX_MANIFEST_BYTES: usize = 64 * 1024;
    const MAX_IMMUTABLE_OBJECT_BYTES: usize = 5 * 1024 * 1024;

    #[derive(Clone)]
    pub struct OneDriveSessionV2Transport {
        client: GraphClient,
        session_id: String,
    }

    impl OneDriveSessionV2Transport {
        pub fn new(
            token: impl Into<String>,
            session_id: impl Into<String>,
        ) -> Result<Self, SessionV2Error> {
            Self::from_client(GraphClient::new(token), session_id.into())
        }

        #[cfg(test)]
        pub(crate) fn with_base_url(
            token: impl Into<String>,
            session_id: impl Into<String>,
            base_url: &str,
        ) -> Result<Self, SessionV2Error> {
            Self::from_client(
                GraphClient::new(token).with_base_url(base_url),
                session_id.into(),
            )
        }

        fn from_client(client: GraphClient, session_id: String) -> Result<Self, SessionV2Error> {
            validate_cloud_component(&session_id)?;
            Ok(Self { client, session_id })
        }

        pub fn create_session(&self) -> Result<VersionedManifest, SessionV2Error> {
            let manifest = SessionManifestV1::empty(&self.session_id);
            let bytes = manifest_bytes(&manifest)?;
            let created = self
                .client
                .upload_content_with_conflict_behavior(
                    &self.manifest_path(),
                    &bytes,
                    ConflictBehavior::Fail,
                )
                .map_err(|_| SessionV2Error::TransportUnavailable)?;
            if let Some(item) = created {
                return versioned_manifest(item, manifest);
            }
            self.load_manifest(&self.session_id)
        }

        fn manifest_path(&self) -> String {
            format!("{V2_ROOT}/{}/manifest.json", self.session_id)
        }

        fn object_path(
            &self,
            object_class: SessionObjectClass,
            object_id: &str,
        ) -> Result<String, SessionV2Error> {
            validate_cloud_component(object_id)?;
            Ok(format!(
                "{V2_ROOT}/{}/staging/{}/{}.bin",
                self.session_id,
                object_class_segment(object_class),
                object_id
            ))
        }

        fn read_path(&self, path: &str, limit: usize) -> Result<Vec<u8>, SessionV2Error> {
            self.client
                .get_bytes_bounded(&format!("/me/drive/root:/{path}:/content"), limit)
                .map_err(|_| SessionV2Error::TransportUnavailable)
        }

        fn manifest_item(&self) -> Result<serde_json::Value, SessionV2Error> {
            self.client
                .get_drive_item_by_path(&self.manifest_path(), &["id", "eTag"])
                .map_err(|_| SessionV2Error::TransportUnavailable)?
                .ok_or(SessionV2Error::TransportUnavailable)
        }
    }

    impl SessionV2Transport for OneDriveSessionV2Transport {
        fn server_time_sample(&self) -> Result<TrustedServerTimeSample, SessionV2Error> {
            let sample = self
                .client
                .server_time_sample()
                .map_err(|_| SessionV2Error::TransportUnavailable)?;
            Ok(TrustedServerTimeSample {
                server_unix_ms: sample.server_unix_ms,
                received_at_monotonic: sample.received_at_monotonic,
            })
        }

        fn load_manifest(&self, session_id: &str) -> Result<VersionedManifest, SessionV2Error> {
            if session_id != self.session_id {
                return Err(SessionV2Error::InvalidRecord);
            }
            let item = self.manifest_item()?;
            let bytes = self.read_path(&self.manifest_path(), MAX_MANIFEST_BYTES)?;
            let manifest: SessionManifestV1 =
                serde_json::from_slice(&bytes).map_err(|_| SessionV2Error::InvalidRecord)?;
            manifest.validate()?;
            if manifest.session_id != self.session_id {
                return Err(SessionV2Error::InvalidRecord);
            }
            versioned_manifest(item, manifest)
        }

        fn stage_immutable(
            &self,
            object_class: SessionObjectClass,
            object_id: &str,
            bytes: &[u8],
        ) -> Result<(), SessionV2Error> {
            if bytes.len() > MAX_IMMUTABLE_OBJECT_BYTES {
                return Err(SessionV2Error::SessionLimit);
            }
            let path = self.object_path(object_class, object_id)?;
            let created = self
                .client
                .upload_content_with_conflict_behavior(&path, bytes, ConflictBehavior::Fail)
                .map_err(|_| SessionV2Error::TransportUnavailable)?;
            if created.is_none() && self.read_path(&path, MAX_IMMUTABLE_OBJECT_BYTES)? != bytes {
                return Err(SessionV2Error::ManifestConflict);
            }
            Ok(())
        }

        fn load_immutable(
            &self,
            object_class: SessionObjectClass,
            object_id: &str,
        ) -> Result<Vec<u8>, SessionV2Error> {
            let path = self.object_path(object_class, object_id)?;
            self.read_path(&path, MAX_IMMUTABLE_OBJECT_BYTES)
        }

        fn reap_unreachable(
            &self,
            reachable: &BTreeSet<(SessionObjectClass, String)>,
            older_than_server_ms: u64,
            limit: usize,
        ) -> Result<usize, SessionV2Error> {
            let mut reaped = 0usize;
            for object_class in [
                SessionObjectClass::VisibleRecord,
                SessionObjectClass::VisibleIndex,
                SessionObjectClass::RequestState,
                SessionObjectClass::RequestIndex,
                SessionObjectClass::UuidBinding,
                SessionObjectClass::UuidBindingIndex,
            ] {
                if reaped >= limit {
                    break;
                }
                let folder_path = format!(
                    "{V2_ROOT}/{}/staging/{}",
                    self.session_id,
                    object_class_segment(object_class)
                );
                let Some(folder) = self
                    .client
                    .get_drive_item_by_path(&folder_path, &["id", "folder"])
                    .map_err(|_| SessionV2Error::TransportUnavailable)?
                else {
                    continue;
                };
                let folder_id = folder
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or(SessionV2Error::TransportUnavailable)?;
                for item in self
                    .client
                    .list_children(folder_id)
                    .map_err(|_| SessionV2Error::TransportUnavailable)?
                {
                    if reaped >= limit {
                        break;
                    }
                    let Some(object_id) = item
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|name| name.strip_suffix(".bin"))
                    else {
                        continue;
                    };
                    if validate_cloud_component(object_id).is_err()
                        || reachable.contains(&(object_class, object_id.to_owned()))
                    {
                        continue;
                    }
                    let modified_ms = item
                        .get("lastModifiedDateTime")
                        .and_then(serde_json::Value::as_str)
                        .and_then(parse_graph_timestamp_ms)
                        .ok_or(SessionV2Error::TransportUnavailable)?;
                    if modified_ms > older_than_server_ms {
                        continue;
                    }
                    let item_id = item
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .filter(|value| !value.is_empty())
                        .ok_or(SessionV2Error::TransportUnavailable)?;
                    self.client
                        .delete_item(item_id)
                        .map_err(|_| SessionV2Error::TransportUnavailable)?;
                    reaped += 1;
                }
            }
            Ok(reaped)
        }

        fn compare_and_swap_manifest(
            &self,
            expected_etag: &str,
            next: &SessionManifestV1,
        ) -> Result<Option<VersionedManifest>, SessionV2Error> {
            next.validate()?;
            if next.session_id != self.session_id {
                return Err(SessionV2Error::InvalidRecord);
            }
            let item = self.manifest_item()?;
            if item_etag(&item)? != expected_etag {
                return Ok(None);
            }
            let item_id = item
                .get("id")
                .and_then(serde_json::Value::as_str)
                .ok_or(SessionV2Error::TransportUnavailable)?;
            let bytes = manifest_bytes(next)?;
            let Some(updated) = self
                .client
                .replace_content_if_match(item_id, &bytes, expected_etag)
                .map_err(|_| SessionV2Error::TransportUnavailable)?
            else {
                return Ok(None);
            };
            versioned_manifest(updated, next.clone()).map(Some)
        }
    }

    fn manifest_bytes(manifest: &SessionManifestV1) -> Result<Vec<u8>, SessionV2Error> {
        let bytes = serde_json::to_vec(manifest).map_err(|_| SessionV2Error::InvalidRecord)?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(SessionV2Error::SessionLimit);
        }
        Ok(bytes)
    }

    fn versioned_manifest(
        item: serde_json::Value,
        manifest: SessionManifestV1,
    ) -> Result<VersionedManifest, SessionV2Error> {
        Ok(VersionedManifest {
            etag: item_etag(&item)?.to_owned(),
            manifest,
        })
    }

    fn item_etag(item: &serde_json::Value) -> Result<&str, SessionV2Error> {
        item.get("eTag")
            .or_else(|| item.get("@odata.etag"))
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= 256)
            .ok_or(SessionV2Error::TransportUnavailable)
    }

    fn parse_graph_timestamp_ms(value: &str) -> Option<u64> {
        let parsed =
            time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
                .ok()?;
        u64::try_from(parsed.unix_timestamp_nanos() / 1_000_000).ok()
    }

    fn validate_cloud_component(value: &str) -> Result<(), SessionV2Error> {
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(SessionV2Error::InvalidRecord);
        }
        Ok(())
    }

    const fn object_class_segment(object_class: SessionObjectClass) -> &'static str {
        match object_class {
            SessionObjectClass::VisibleRecord => "visible-record",
            SessionObjectClass::VisibleIndex => "visible-index",
            SessionObjectClass::RequestState => "request-state",
            SessionObjectClass::RequestIndex => "request-index",
            SessionObjectClass::UuidBinding => "uuid-binding",
            SessionObjectClass::UuidBindingIndex => "uuid-binding-index",
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn onedrive_v2_transport_rejects_untrusted_path_components() {
            assert!(OneDriveSessionV2Transport::with_base_url(
                "token",
                "../session",
                "http://127.0.0.1:1"
            )
            .is_err());
        }
    }
}

#[cfg(feature = "onedrive")]
pub use onedrive_transport::OneDriveSessionV2Transport;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAttemptBindingV1 {
    pub provider: ProductProviderId,
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    pub credential_generation: String,
    pub oauth_policy_fingerprint: String,
    pub harness_contract_version: u32,
    pub origin_installation_digest: String,
}

impl ProviderAttemptBindingV1 {
    pub fn revalidate(&self, current: &Self) -> Result<(), SessionV2Error> {
        if self == current {
            Ok(())
        } else {
            Err(SessionV2Error::ProviderGenerationChanged)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestPhase {
    Accepted,
    ProviderStepStarted,
    ProviderStepCompleted,
    PendingConfirmation,
    Committed,
    Failed,
    Cancelled,
    OutcomeUnknown,
}

impl RequestPhase {
    pub fn permits_automatic_resume(self) -> bool {
        matches!(self, Self::Accepted | Self::ProviderStepCompleted)
    }

    pub fn recovery_phase(self) -> Self {
        if self == Self::ProviderStepStarted {
            Self::OutcomeUnknown
        } else {
            self
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NormalizedAssistantBlock {
    Text {
        text: String,
    },
    ToolUse {
        tool_use_id: String,
        action: ToolAction,
    },
    RejectedToolUse {
        tool_use_id: String,
        stable_error_code: String,
        help_schema_version: u32,
        help_digest: String,
    },
}

impl NormalizedAssistantBlock {
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
        if tool_result_digest(help.as_bytes()) != *help_digest {
            return Err(SessionV2Error::InvalidJournal);
        }
        Ok(Some(help))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestStepOutcomeV1 {
    pub outcome_version: u32,
    pub outcome_id: String,
    pub step_seq: u8,
    pub previous_outcome_id: Option<String>,
    pub provider: ProductProviderId,
    pub model: String,
    pub normalized_blocks: Vec<NormalizedAssistantBlock>,
    pub final_text: Option<String>,
    pub sanitized_usage: Option<SanitizedUsage>,
    pub terminal_validation_error: Option<String>,
    pub outcome_digest: String,
}

impl RequestStepOutcomeV1 {
    pub fn validate(&self, binding: &ProviderAttemptBindingV1) -> Result<(), SessionV2Error> {
        if self.outcome_version != 1
            || !valid_ulid(&self.outcome_id)
            || self.step_seq >= MAX_PROVIDER_STEPS
            || self.provider != binding.provider
            || self.model != binding.model
            || self.normalized_blocks.len() > MAX_NORMALIZED_BLOCKS
            || self
                .final_text
                .as_ref()
                .is_some_and(|text| text.len() > MAX_FINAL_TEXT_BYTES)
            || self
                .terminal_validation_error
                .as_ref()
                .is_some_and(|code| !valid_closed_code(code))
        {
            return Err(SessionV2Error::InvalidJournal);
        }
        let mut ids = BTreeSet::new();
        for block in &self.normalized_blocks {
            let id = match block {
                NormalizedAssistantBlock::Text { text } => {
                    if text.len() > MAX_FINAL_TEXT_BYTES {
                        return Err(SessionV2Error::InvalidJournal);
                    }
                    continue;
                }
                NormalizedAssistantBlock::ToolUse { tool_use_id, .. }
                | NormalizedAssistantBlock::RejectedToolUse { tool_use_id, .. } => tool_use_id,
            };
            if id.is_empty() || id.len() > MAX_TOOL_USE_ID_BYTES || !ids.insert(id) {
                return Err(SessionV2Error::DuplicateToolUseId);
            }
        }
        let mut semantic = self.clone();
        semantic.outcome_digest.clear();
        if semantic_digest(&semantic)? != self.outcome_digest {
            return Err(SessionV2Error::InvalidJournal);
        }
        let bytes = serde_json::to_vec(self).map_err(|_| SessionV2Error::InvalidJournal)?;
        if bytes.len() > MAX_STEP_OUTCOME_BYTES {
            return Err(SessionV2Error::SessionLimit);
        }
        Ok(())
    }

    pub fn seal_digest(mut self) -> Result<Self, SessionV2Error> {
        self.outcome_digest.clear();
        self.outcome_digest = semantic_digest(&self)?;
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestStepRef {
    pub step_seq: u8,
    pub outcome_id: String,
    pub outcome_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadToolCheckpointV1 {
    pub provider_step_seq: u8,
    pub tool_use_id: String,
    pub action: ToolAction,
    pub policy: crate::tool::RecoveryPolicy,
    pub result_sha256: String,
    pub local_effect: Option<LocalEffectCheckpointV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalEffectCheckpointV1 {
    pub relative_path: String,
    pub source_sha256: String,
    pub expected_file_sha256: String,
    pub state: LocalEffectState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalEffectState {
    Planned,
    Committed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestJournalV1 {
    pub journal_version: u32,
    pub session_id: String,
    pub request_id: String,
    pub turn_id: String,
    pub provider_binding: ProviderAttemptBindingV1,
    pub phase: RequestPhase,
    pub next_step_seq: u8,
    pub completed_steps: Vec<RequestStepRef>,
    pub read_checkpoints: Vec<ReadToolCheckpointV1>,
}

impl RequestJournalV1 {
    pub fn validate_chain<T: SessionV2Transport>(
        &self,
        transport: &T,
    ) -> Result<Vec<RequestStepOutcomeV1>, SessionV2Error> {
        self.validate_chain_with(|outcome_id| {
            transport.load_immutable(SessionObjectClass::RequestState, outcome_id)
        })
    }

    fn validate_chain_with<F>(
        &self,
        mut load: F,
    ) -> Result<Vec<RequestStepOutcomeV1>, SessionV2Error>
    where
        F: FnMut(&str) -> Result<Vec<u8>, SessionV2Error>,
    {
        if self.journal_version != REQUEST_JOURNAL_VERSION
            || !valid_uuid_v4(&self.request_id)
            || !valid_ulid(&self.turn_id)
            || self.next_step_seq > MAX_PROVIDER_STEPS
            || self.completed_steps.len() != usize::from(self.next_step_seq)
            || self.read_checkpoints.len() > MAX_TOOL_CHECKPOINTS
        {
            return Err(SessionV2Error::InvalidJournal);
        }
        let mut outcomes = Vec::with_capacity(self.completed_steps.len());
        let mut previous: Option<&str> = None;
        let mut total_bytes = 0u64;
        let mut all_tool_ids = BTreeSet::new();
        for (expected, step) in self.completed_steps.iter().enumerate() {
            if usize::from(step.step_seq) != expected || !valid_ulid(&step.outcome_id) {
                return Err(SessionV2Error::InvalidJournal);
            }
            let bytes = load(&step.outcome_id)?;
            total_bytes = total_bytes
                .checked_add(bytes.len() as u64)
                .ok_or(SessionV2Error::SessionLimit)?;
            if total_bytes > MAX_REQUEST_OUTCOME_BYTES
                || bytes_digest(&bytes) != step.outcome_sha256
            {
                return Err(SessionV2Error::InvalidJournal);
            }
            let outcome: RequestStepOutcomeV1 =
                serde_json::from_slice(&bytes).map_err(|_| SessionV2Error::InvalidJournal)?;
            outcome.validate(&self.provider_binding)?;
            if outcome.step_seq != step.step_seq
                || outcome.outcome_id != step.outcome_id
                || outcome.previous_outcome_id.as_deref() != previous
            {
                return Err(SessionV2Error::InvalidJournal);
            }
            for block in &outcome.normalized_blocks {
                if let NormalizedAssistantBlock::ToolUse { tool_use_id, .. }
                | NormalizedAssistantBlock::RejectedToolUse { tool_use_id, .. } = block
                {
                    if !all_tool_ids.insert(tool_use_id.clone()) {
                        return Err(SessionV2Error::DuplicateToolUseId);
                    }
                }
            }
            previous = Some(&step.outcome_id);
            outcomes.push(outcome);
        }
        let mut checkpoint_keys = BTreeSet::new();
        for checkpoint in &self.read_checkpoints {
            let key = format!(
                "{}:{}",
                checkpoint.provider_step_seq, checkpoint.tool_use_id
            );
            let matching_action = outcomes
                .get(usize::from(checkpoint.provider_step_seq))
                .and_then(|outcome| {
                    outcome
                        .normalized_blocks
                        .iter()
                        .find_map(|block| match block {
                            NormalizedAssistantBlock::ToolUse {
                                tool_use_id,
                                action,
                            } if tool_use_id == &checkpoint.tool_use_id => Some(action),
                            _ => None,
                        })
                });
            if !checkpoint_keys.insert(key)
                || checkpoint.tool_use_id.is_empty()
                || checkpoint.tool_use_id.len() > MAX_TOOL_USE_ID_BYTES
                || matching_action != Some(&checkpoint.action)
                || checkpoint.policy != checkpoint.action.recovery_policy()
                || checkpoint.policy == crate::RecoveryPolicy::NeverRepeat
                || (!checkpoint.result_sha256.is_empty() && checkpoint.result_sha256.len() != 43)
                || checkpoint.local_effect.as_ref().is_some_and(|effect| {
                    checkpoint.policy != crate::RecoveryPolicy::IdempotentLocalMaterialize
                        || !valid_relative_effect_path(&effect.relative_path)
                        || !valid_hex_digest(&effect.source_sha256)
                        || !valid_hex_digest(&effect.expected_file_sha256)
                        || (effect.state == LocalEffectState::Committed
                            && checkpoint.result_sha256.is_empty())
                })
            {
                return Err(SessionV2Error::InvalidJournal);
            }
        }
        Ok(outcomes)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RequestRouteDomain {
    SessionCreate,
    SessionSelect,
    SessionArchive,
    AgentTurn,
    AgentConfirm,
    TurnCancel,
    PendingCancel,
    PairingCreate,
    PairingReveal,
    PairingClaim,
    PairingFinalize,
    PairingRevoke,
}

impl RequestRouteDomain {
    pub const ALL: [Self; 12] = [
        Self::SessionCreate,
        Self::SessionSelect,
        Self::SessionArchive,
        Self::AgentTurn,
        Self::AgentConfirm,
        Self::TurnCancel,
        Self::PendingCancel,
        Self::PairingCreate,
        Self::PairingReveal,
        Self::PairingClaim,
        Self::PairingFinalize,
        Self::PairingRevoke,
    ];
    pub fn canonical(self) -> &'static str {
        match self {
            Self::SessionCreate => "post:/api/v1/agent/session/create",
            Self::SessionSelect => "post:/api/v1/agent/session/select",
            Self::SessionArchive => "post:/api/v1/agent/session/archive",
            Self::AgentTurn => "post:/api/v1/agent/turn",
            Self::AgentConfirm => "post:/api/v1/agent/confirm",
            Self::TurnCancel => "post:/api/v1/agent/turn/cancel",
            Self::PendingCancel => "post:/api/v1/agent/pending/cancel",
            Self::PairingCreate => "post:/api/v1/agent/session/pairing/create",
            Self::PairingReveal => "post:/api/v1/agent/session/pairing/reveal",
            Self::PairingClaim => "post:/api/v1/agent/session/pairing/claim",
            Self::PairingFinalize => "post:/api/v1/agent/session/pairing/finalize",
            Self::PairingRevoke => "post:/api/v1/agent/session/pairing/revoke",
        }
    }
}

pub fn request_key(
    route: RequestRouteDomain,
    session_scope: &str,
    request_id: &str,
) -> Result<String, SessionV2Error> {
    if session_scope.is_empty() || !valid_uuid_v4(request_id) {
        return Err(SessionV2Error::InvalidRequestId);
    }
    Ok(domain_hash(
        b"isyncyou-idempotency-v1",
        &[
            route.canonical().as_bytes(),
            session_scope.as_bytes(),
            request_id.as_bytes(),
        ],
    ))
}

pub fn payload_digest<T: Serialize>(value: &T) -> Result<String, SessionV2Error> {
    let bytes = serde_json::to_vec(value).map_err(|_| SessionV2Error::InvalidRecord)?;
    Ok(domain_hash(b"isyncyou-payload-v1", &[&bytes]))
}

pub fn tool_result_digest(bytes: &[u8]) -> String {
    domain_hash(b"isyncyou-tool-result-v1", &[bytes])
}

pub fn request_object_digest(bytes: &[u8]) -> String {
    bytes_digest(bytes)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBudget {
    pub max_messages: usize,
    pub max_bytes: usize,
    pub max_tokens: usize,
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            max_messages: MAX_CONTEXT_MESSAGES,
            max_bytes: MAX_CONTEXT_BYTES,
            max_tokens: MAX_CONTEXT_TOKENS,
        }
    }
}

pub trait InputTokenCounter {
    fn count_input_tokens(&self, text: &str) -> Option<usize>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleContextMessage {
    pub turn_id: String,
    pub role: &'static str,
    pub text: String,
}

/// Select newest complete turn groups while satisfying message, byte, and token limits.
/// Unknown tokenizers intentionally charge one token per UTF-8 byte.
pub fn select_provider_context(
    records: &[SessionRecordV2],
    counter: Option<&dyn InputTokenCounter>,
    budget: &ContextBudget,
) -> Vec<VisibleContextMessage> {
    let mut groups: Vec<Vec<VisibleContextMessage>> = Vec::new();
    for record in records {
        let message = match &record.kind {
            SessionRecordKind::TurnIntent { user_text } => Some(VisibleContextMessage {
                turn_id: record.turn_id.clone(),
                role: "user",
                text: user_text.clone(),
            }),
            SessionRecordKind::AssistantResult { text, .. } => Some(VisibleContextMessage {
                turn_id: record.turn_id.clone(),
                role: "assistant",
                text: text.clone(),
            }),
            _ => None,
        };
        let Some(message) = message else { continue };
        if let Some(group) = groups.last_mut().filter(|group| {
            group
                .first()
                .is_some_and(|item| item.turn_id == message.turn_id)
        }) {
            group.push(message);
        } else {
            groups.push(vec![message]);
        }
    }

    let mut selected = Vec::new();
    let mut used_messages = 0usize;
    let mut used_bytes = 0usize;
    let mut used_tokens = 0usize;
    for group in groups.into_iter().rev() {
        let complete = group.iter().any(|message| message.role == "user")
            && group.iter().any(|message| message.role == "assistant");
        if !complete {
            continue;
        }
        let group_messages = group.len();
        let group_bytes = group.iter().map(|item| item.text.len()).sum::<usize>();
        let group_tokens = group
            .iter()
            .map(|item| {
                counter
                    .and_then(|counter| counter.count_input_tokens(&item.text))
                    .unwrap_or(item.text.len())
            })
            .sum::<usize>();
        if used_messages.saturating_add(group_messages) > budget.max_messages
            || used_bytes.saturating_add(group_bytes) > budget.max_bytes
            || used_tokens.saturating_add(group_tokens) > budget.max_tokens
        {
            break;
        }
        used_messages += group_messages;
        used_bytes += group_bytes;
        used_tokens += group_tokens;
        selected.push(group);
    }
    selected.reverse();
    selected.into_iter().flatten().collect()
}

fn semantic_digest<T: Serialize>(value: &T) -> Result<String, SessionV2Error> {
    payload_digest(value)
}
fn bytes_digest(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(digest(&SHA256, bytes).as_ref())
}
fn domain_hash(domain: &[u8], components: &[&[u8]]) -> String {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(domain);
    for component in components {
        bytes.extend_from_slice(&(component.len() as u64).to_be_bytes());
        bytes.extend_from_slice(component);
    }
    bytes_digest(&bytes)
}
fn add_signed(value: u64, delta: i64) -> Result<u64, SessionV2Error> {
    if delta >= 0 {
        value.checked_add(delta as u64)
    } else {
        value.checked_sub(delta.unsigned_abs())
    }
    .ok_or(SessionV2Error::SessionLimit)
}
fn valid_closed_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn valid_hex_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_relative_effect_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && std::path::Path::new(value).components().all(|component| {
            matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
}
fn valid_ulid(value: &str) -> bool {
    value.len() == 26
        && value
            .bytes()
            .all(|b| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&b))
}
pub fn valid_uuid_v4(value: &str) -> bool {
    if value.len() != 36
        || value.as_bytes().get(14) != Some(&b'4')
        || !matches!(value.as_bytes().get(19), Some(b'8' | b'9' | b'a' | b'b'))
    {
        return false;
    }
    value.bytes().enumerate().all(|(i, b)| {
        if matches!(i, 8 | 13 | 18 | 23) {
            b == b'-'
        } else {
            b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
        }
    })
}

fn index_entry(object_id: &str, bytes: &[u8]) -> ImmutableIndexEntryV1 {
    ImmutableIndexEntryV1 {
        object_id: object_id.to_owned(),
        object_sha256: bytes_digest(bytes),
        encrypted_bytes: bytes.len() as u64,
    }
}

fn page_delta_bytes(page: &Option<IndexPageRef>) -> u64 {
    page.as_ref().map_or(0, |page| page.encrypted_bytes)
}

fn request_entries_bytes(entries: &[ImmutableIndexEntryV1]) -> Result<u64, SessionV2Error> {
    entries.iter().try_fold(0u64, |total, entry| {
        total
            .checked_add(entry.encrypted_bytes)
            .ok_or(SessionV2Error::SessionLimit)
    })
}

fn deterministic_page_id(
    previous: Option<&IndexPageRef>,
    entries: &[ImmutableIndexEntryV1],
) -> Result<String, SessionV2Error> {
    let bytes =
        serde_json::to_vec(&(previous, entries)).map_err(|_| SessionV2Error::InvalidRecord)?;
    Ok(format!(
        "page-{}",
        domain_hash(b"isyncyou-index-page-v1", &[&bytes])
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    const RID: &str = "00000000-0000-4000-8000-000000000001";
    const ULID_A: &str = "0000000000000000000000000A";
    const ULID_B: &str = "0000000000000000000000000B";

    fn lease() -> PersistedLeaseBinding {
        PersistedLeaseBinding {
            lease_id: "lease".into(),
            holder_binding: "holder".into(),
            fence: 1,
            expires_at_server_ms: 1000,
        }
    }
    fn binding() -> ProviderAttemptBindingV1 {
        ProviderAttemptBindingV1 {
            provider: ProductProviderId::Claude,
            model: "model".into(),
            reasoning_effort: None,
            credential_generation: "generation".into(),
            oauth_policy_fingerprint: "policy".into(),
            harness_contract_version: 1,
            origin_installation_digest: "digest".into(),
        }
    }
    fn object_crypto() -> SessionObjectCrypto {
        static CRYPTO: OnceLock<SessionObjectCrypto> = OnceLock::new();
        CRYPTO
            .get_or_init(|| {
                SessionObjectCrypto::new(
                    b"01234567890123456789012345678901",
                    crate::SessionCryptoConfig::new(crate::KdfProfile::production([3; 16]))
                        .unwrap(),
                )
                .unwrap()
            })
            .clone()
    }
    fn text_outcome(seq: u8, id: &str, previous: Option<String>) -> RequestStepOutcomeV1 {
        RequestStepOutcomeV1 {
            outcome_version: 1,
            outcome_id: id.into(),
            step_seq: seq,
            previous_outcome_id: previous,
            provider: ProductProviderId::Claude,
            model: "model".into(),
            normalized_blocks: vec![NormalizedAssistantBlock::Text {
                text: "answer".into(),
            }],
            final_text: Some("answer".into()),
            sanitized_usage: Some(SanitizedUsage {
                input_tokens: 1,
                output_tokens: 1,
            }),
            terminal_validation_error: None,
            outcome_digest: String::new(),
        }
        .seal_digest()
        .unwrap()
    }

    fn record(record_id: &str, turn_id: &str, request_id: &str, text: &str) -> SessionRecordV2 {
        SessionRecordV2 {
            record_version: 2,
            record_id: record_id.into(),
            session_id: "s".into(),
            request_id: request_id.into(),
            turn_id: turn_id.into(),
            kind: SessionRecordKind::TurnIntent {
                user_text: text.into(),
            },
            parent_record_ids: vec![],
            observed_head: None,
            lease: lease(),
            created_at_ms: 1,
        }
    }

    fn assistant_record(
        record_id: &str,
        turn_id: &str,
        request_id: &str,
        text: &str,
    ) -> SessionRecordV2 {
        let mut value = record(record_id, turn_id, request_id, "placeholder");
        value.kind = SessionRecordKind::AssistantResult {
            text: text.into(),
            sources: vec![],
            usage: None,
        };
        value
    }

    #[test]
    fn lease_renews_while_provider_emits_no_events() {
        let transport = InMemorySessionV2Transport::default();
        transport.set_server_time_ms(10_000);
        transport.create_session("s").unwrap();
        let store = SessionV2Store::new(transport.clone(), &[7; 32], object_crypto());
        let guard = store
            .acquire_lease_with_interval(
                "s",
                "lease-a",
                "session-holder",
                std::time::Duration::from_millis(5),
            )
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let renewed = store.current_manifest("s").unwrap();
        assert!(renewed.manifest.generation >= 2);
        assert_eq!(
            renewed
                .manifest
                .active_lease
                .as_ref()
                .map(|lease| lease.lease_id.as_str()),
            Some("lease-a")
        );
        let first_fence = guard.binding().unwrap().fence;
        drop(guard);
        let released = store.current_manifest("s").unwrap();
        assert!(released.manifest.active_lease.is_none());
        let replacement = store
            .acquire_lease_with_interval(
                "s",
                "lease-b",
                "session-holder",
                std::time::Duration::from_secs(60),
            )
            .unwrap();
        assert!(replacement.binding().unwrap().fence > first_fence);
    }

    #[test]
    fn lease_renewal_worker_stops_on_every_exit() {
        let transport = InMemorySessionV2Transport::default();
        transport.set_server_time_ms(10_000);
        transport.create_session("s").unwrap();
        let store = SessionV2Store::new(transport, &[27; 32], object_crypto());
        let guard = store
            .acquire_lease_with_interval(
                "s",
                "lease-a",
                "session-holder",
                std::time::Duration::from_millis(5),
            )
            .unwrap();
        drop(guard);
        let released_generation = store.current_manifest("s").unwrap().manifest.generation;
        std::thread::sleep(std::time::Duration::from_millis(20));
        let later = store.current_manifest("s").unwrap();
        assert_eq!(later.manifest.generation, released_generation);
        assert!(later.manifest.active_lease.is_none());
    }

    #[test]
    fn missing_malformed_or_stale_graph_server_time_fails_closed() {
        let transport = InMemorySessionV2Transport::default();
        transport.set_server_time_ms(10_000);
        transport.create_session("s").unwrap();
        let store = SessionV2Store::new(transport.clone(), &[28; 32], object_crypto());
        transport.set_server_time_unavailable(true);
        assert_eq!(
            store.acquire_lease("s", "lease-a", "holder-a").err(),
            Some(SessionV2Error::TransportUnavailable)
        );
        transport.set_server_time_unavailable(false);
        transport.set_server_sample_age(
            MAX_SERVER_TIME_SAMPLE_AGE + std::time::Duration::from_millis(1),
        );
        assert_eq!(
            store.acquire_lease("s", "lease-a", "holder-a").err(),
            Some(SessionV2Error::TransportUnavailable)
        );
    }

    #[test]
    fn terminal_publication_rechecks_fresh_server_time_and_fence() {
        let transport = InMemorySessionV2Transport::default();
        transport.set_server_time_ms(10_000);
        transport.create_session("s").unwrap();
        let store = SessionV2Store::new(transport.clone(), &[29; 32], object_crypto());
        let guard = store.acquire_lease("s", "lease-a", "holder-a").unwrap();
        transport.set_server_time_ms(9_999);
        assert_eq!(
            guard.publish(SessionCommitV1 {
                visible_records: vec![],
                request_objects: vec![],
                uuid_bindings: vec![],
            }),
            Err(SessionV2Error::TransportUnavailable)
        );
        assert!(guard.is_lost());
    }

    fn assert_takeover_uses_server_time() {
        let transport = InMemorySessionV2Transport::default();
        transport.set_server_time_ms(1_000);
        transport.create_session("s").unwrap();
        let store = SessionV2Store::new(transport.clone(), &[30; 32], object_crypto());
        let first = store.acquire_lease("s", "lease-a", "holder-a").unwrap();
        transport
            .set_server_time_ms(1_000 + SESSION_LEASE_TTL_MS + SESSION_LEASE_TAKEOVER_MARGIN_MS);
        assert_eq!(
            store.acquire_lease("s", "lease-b", "holder-b").err(),
            Some(SessionV2Error::ManifestConflict)
        );
        transport
            .set_server_time_ms(1_001 + SESSION_LEASE_TTL_MS + SESSION_LEASE_TAKEOVER_MARGIN_MS);
        let replacement = store.acquire_lease("s", "lease-b", "holder-b").unwrap();
        assert!(!replacement.is_lost());
        drop(first);
    }

    #[test]
    fn lease_expiry_and_takeover_use_graph_server_time_not_device_wall_clock() {
        assert_takeover_uses_server_time();
    }

    #[test]
    fn clock_skewed_device_cannot_take_over_live_session_lease() {
        assert_takeover_uses_server_time();
    }

    #[test]
    fn lost_lease_rejects_late_append_and_finalization() {
        let transport = InMemorySessionV2Transport::default();
        transport.set_server_time_ms(1_000);
        transport.create_session("s").unwrap();
        let store = SessionV2Store::new(transport.clone(), &[8; 32], object_crypto());
        let stale = store
            .acquire_lease_with_interval(
                "s",
                "lease-a",
                "holder-a",
                std::time::Duration::from_secs(60),
            )
            .unwrap();
        transport.set_server_time_ms(SESSION_LEASE_TTL_MS + 10_000);
        let replacement = store
            .acquire_lease_with_interval(
                "s",
                "lease-b",
                "holder-b",
                std::time::Duration::from_secs(60),
            )
            .unwrap();
        let mut late = record(ULID_A, ULID_B, RID, "late");
        late.lease = stale.binding().unwrap();
        assert_eq!(
            stale.publish(SessionCommitV1 {
                visible_records: vec![late],
                request_objects: vec![],
                uuid_bindings: vec![],
            }),
            Err(SessionV2Error::LeaseLost)
        );
        assert!(stale.is_lost());
        assert_eq!(transport.object_count(), 0);
        assert!(!replacement.is_lost());
    }

    #[test]
    fn session_v2_round_trips_user_assistant_sources_and_terminal_state() {
        let values = [
            SessionRecordKind::TurnIntent {
                user_text: "question".into(),
            },
            SessionRecordKind::AssistantResult {
                text: "answer".into(),
                sources: vec![SourceRef {
                    service: "mail".into(),
                    item_id: "opaque".into(),
                    label: None,
                }],
                usage: Some(SanitizedUsage {
                    input_tokens: 2,
                    output_tokens: 3,
                }),
            },
            SessionRecordKind::TurnTerminal {
                status: TurnTerminalStatus::Complete,
                error_code: None,
            },
        ];
        for (index, kind) in values.into_iter().enumerate() {
            let record = SessionRecordV2 {
                record_version: 2,
                record_id: if index == 0 {
                    ULID_A.into()
                } else {
                    ULID_B.into()
                },
                session_id: "session".into(),
                request_id: RID.into(),
                turn_id: ULID_A.into(),
                kind,
                parent_record_ids: vec![],
                observed_head: None,
                lease: lease(),
                created_at_ms: 1,
            };
            record.validate().unwrap();
            let bytes = serde_json::to_vec(&record).unwrap();
            let decoded: SessionRecordV2 = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(decoded, record);
        }
    }

    #[test]
    fn session_v2_never_persists_raw_tool_results_or_provider_frames() {
        let source = include_str!("session_v2.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(!source.contains("raw_tool_result"));
        assert!(!source.contains("provider_frame"));
        assert!(!source.contains("account_alias"));
    }

    #[test]
    fn session_record_version_is_independent_from_crypto_envelope_version() {
        assert_eq!(SESSION_RECORD_VERSION, 2);
        assert!(!include_str!("session_v2.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap()
            .contains("envelope_version"));
    }

    #[test]
    fn session_cloud_records_do_not_persist_local_account_alias() {
        session_v2_never_persists_raw_tool_results_or_provider_frames();
    }

    #[test]
    fn session_v1_loads_readonly_and_new_writes_use_v2() {
        assert_eq!(
            session_write_policy(1).unwrap(),
            SessionWritePolicy::LegacyReadOnly
        );
        assert_eq!(
            session_write_policy(2).unwrap(),
            SessionWritePolicy::WritableV2
        );
        assert_eq!(session_write_policy(3), Err(SessionV2Error::InvalidRecord));
    }

    #[test]
    fn session_v1_rejects_new_turn_and_never_upgrades_in_place() {
        assert_ne!(
            session_write_policy(1).unwrap(),
            SessionWritePolicy::WritableV2
        );
    }

    #[test]
    fn pairing_create_rejects_legacy_readonly_session() {
        assert_eq!(
            session_write_policy(1),
            Ok(SessionWritePolicy::LegacyReadOnly)
        );
        assert_ne!(
            session_write_policy(1).unwrap(),
            SessionWritePolicy::WritableV2
        );
    }

    #[test]
    fn session_manifest_is_authoritative_for_head_count_and_byte_budget() {
        let manifest = SessionManifestV1::empty("s");
        let next = manifest
            .apply_delta(&ManifestDelta {
                visible_records: 1,
                internal_records: 2,
                visible_bytes: 10,
                internal_bytes: 20,
                visible_record_head: Some(Some(ULID_A.into())),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            (
                next.visible_record_count,
                next.internal_record_count,
                next.visible_encrypted_bytes,
                next.internal_encrypted_bytes
            ),
            (1, 2, 10, 20)
        );
    }

    #[test]
    fn manifest_cas_atomically_advances_visible_request_and_uuid_binding_heads() {
        let transport = InMemorySessionV2Transport::default();
        let current = transport.create_session("s").unwrap();
        let page = |id: &str| IndexPageRef {
            page_id: id.into(),
            sha256: "hash".into(),
            encrypted_bytes: 1,
        };
        let next = current
            .manifest
            .apply_delta(&ManifestDelta {
                visible_records: 1,
                internal_records: 2,
                visible_bytes: 1,
                internal_bytes: 2,
                visible_index_head: Some(Some(page("visible"))),
                visible_record_head: Some(Some(ULID_A.into())),
                request_index_head: Some(Some(page("request"))),
                uuid_binding_index_head: Some(Some(page("uuid"))),
            })
            .unwrap();
        let updated = transport
            .compare_and_swap_manifest(&current.etag, &next)
            .unwrap()
            .unwrap();
        assert_eq!(
            updated.manifest.visible_index_head.unwrap().page_id,
            "visible"
        );
        assert_eq!(
            updated.manifest.request_index_head.unwrap().page_id,
            "request"
        );
        assert_eq!(
            updated.manifest.uuid_binding_index_head.unwrap().page_id,
            "uuid"
        );
    }

    #[test]
    fn staged_record_is_invisible_until_manifest_cas_succeeds() {
        let transport = InMemorySessionV2Transport::default();
        let current = transport.create_session("s").unwrap();
        transport
            .stage_immutable(SessionObjectClass::RequestState, ULID_A, b"sealed")
            .unwrap();
        assert_eq!(transport.object_count(), 1);
        assert!(current.manifest.visible_record_head.is_none());
    }

    #[test]
    fn orphan_staged_records_are_reaped_without_affecting_visible_history() {
        let transport = InMemorySessionV2Transport::default();
        transport.set_server_time_ms(1);
        transport.create_session("s").unwrap();
        transport
            .stage_immutable(
                SessionObjectClass::RequestState,
                "0000000000000000000000000Z",
                b"orphan",
            )
            .unwrap();
        let store = SessionV2Store::new(transport.clone(), &[7; 32], object_crypto());
        let guard = store.acquire_lease("s", "lease-a", "holder-a").unwrap();
        let mut visible = record(ULID_A, ULID_A, RID, "prompt");
        visible.lease = guard.binding().unwrap();
        guard
            .publish(SessionCommitV1 {
                visible_records: vec![visible],
                request_objects: vec![],
                uuid_bindings: vec![],
            })
            .unwrap();
        drop(guard);

        transport.set_server_time_ms(ORPHAN_RETENTION_MS + 2);
        let guard = store.acquire_lease("s", "lease-b", "holder-b").unwrap();
        assert_eq!(guard.reap_orphans().unwrap(), 1);
        assert!(transport
            .load_immutable(
                SessionObjectClass::RequestState,
                "0000000000000000000000000Z"
            )
            .is_err());
        let history = store.history("s", None, None).unwrap();
        assert_eq!(history.records.len(), 1);
        assert_eq!(history.records[0].record_id, ULID_A);
    }

    #[test]
    fn stale_fence_cannot_publish_after_lease_takeover() {
        let transport = InMemorySessionV2Transport::default();
        let stale = transport.create_session("s").unwrap();
        let first = stale
            .manifest
            .apply_delta(&ManifestDelta::default())
            .unwrap();
        transport
            .compare_and_swap_manifest(&stale.etag, &first)
            .unwrap()
            .unwrap();
        assert!(transport
            .compare_and_swap_manifest(&stale.etag, &first)
            .unwrap()
            .is_none());
    }

    #[test]
    fn request_key_domain_separates_route_session_and_request_id() {
        let a = request_key(RequestRouteDomain::AgentTurn, "session-a", RID).unwrap();
        let b = request_key(RequestRouteDomain::AgentConfirm, "session-a", RID).unwrap();
        let c = request_key(RequestRouteDomain::AgentTurn, "session-b", RID).unwrap();
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn same_request_id_different_route_session_or_payload_returns_conflict() {
        let original = RequestUuidBindingV1::new(
            RequestRouteDomain::AgentTurn,
            "s",
            RID,
            payload_digest(&"first").unwrap(),
        )
        .unwrap();
        let route_changed = RequestUuidBindingV1::new(
            RequestRouteDomain::AgentConfirm,
            "s",
            RID,
            payload_digest(&"first").unwrap(),
        )
        .unwrap();
        let payload_changed = RequestUuidBindingV1::new(
            RequestRouteDomain::AgentTurn,
            "s",
            RID,
            payload_digest(&"second").unwrap(),
        )
        .unwrap();
        assert_eq!(
            original.permits_replay(&route_changed),
            Err(SessionV2Error::RequestConflict)
        );
        assert_eq!(
            original.permits_replay(&payload_changed),
            Err(SessionV2Error::RequestConflict)
        );
        original.permits_replay(&original).unwrap();
    }

    #[test]
    fn same_request_id_same_terminal_result_does_not_create_a_second_binding() {
        let transport = InMemorySessionV2Transport::default();
        let current = transport.create_session("s").unwrap();
        let store = SessionV2Store::new(transport, &[6; 32], object_crypto());
        let request_binding = RequestUuidBindingV1::new(
            RequestRouteDomain::AgentTurn,
            "s",
            RID,
            payload_digest(&("account", "prompt")).unwrap(),
        )
        .unwrap();
        let journal = RequestJournalV1 {
            journal_version: REQUEST_JOURNAL_VERSION,
            session_id: "s".into(),
            request_id: RID.into(),
            turn_id: ULID_A.into(),
            provider_binding: binding(),
            phase: RequestPhase::Committed,
            next_step_seq: 0,
            completed_steps: vec![],
            read_checkpoints: vec![],
        };
        store
            .publish(
                &current,
                SessionCommitV1 {
                    visible_records: vec![record(ULID_A, ULID_A, RID, "prompt")],
                    request_objects: vec![(ULID_B.into(), serde_json::to_vec(&journal).unwrap())],
                    uuid_bindings: vec![request_binding.clone()],
                },
            )
            .unwrap();

        let replay = store
            .request_replay("s", &request_binding)
            .unwrap()
            .expect("durable replay");
        assert_eq!(replay.binding, request_binding);
        assert_eq!(
            replay.journal.as_ref().map(|journal| journal.phase),
            Some(RequestPhase::Committed)
        );
        assert_eq!(replay.visible_records.len(), 1);

        let conflict = RequestUuidBindingV1::new(
            RequestRouteDomain::AgentTurn,
            "s",
            RID,
            payload_digest(&("account", "changed")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            store.request_replay("s", &conflict),
            Err(SessionV2Error::RequestConflict)
        );
    }

    #[test]
    fn request_terminal_compaction_removes_recovery_payload_and_keeps_idempotency() {
        let transport = InMemorySessionV2Transport::default();
        let current = transport.create_session("s").unwrap();
        let store = SessionV2Store::new(transport, &[6; 32], object_crypto());
        let request_binding = RequestUuidBindingV1::new(
            RequestRouteDomain::AgentTurn,
            "s",
            RID,
            payload_digest(&("account", "prompt")).unwrap(),
        )
        .unwrap();
        let outcome = text_outcome(0, ULID_B, None);
        let outcome_bytes = serde_json::to_vec(&outcome).unwrap();
        let journal = RequestJournalV1 {
            journal_version: REQUEST_JOURNAL_VERSION,
            session_id: "s".into(),
            request_id: RID.into(),
            turn_id: ULID_A.into(),
            provider_binding: binding(),
            phase: RequestPhase::ProviderStepCompleted,
            next_step_seq: 1,
            completed_steps: vec![RequestStepRef {
                step_seq: 0,
                outcome_id: ULID_B.into(),
                outcome_sha256: bytes_digest(&outcome_bytes),
            }],
            read_checkpoints: vec![],
        };
        let current = store
            .publish(
                &current,
                SessionCommitV1 {
                    visible_records: vec![record(ULID_A, ULID_A, RID, "prompt")],
                    request_objects: vec![
                        (ULID_B.into(), outcome_bytes),
                        (
                            "0000000000000000000000000C".into(),
                            serde_json::to_vec(&journal).unwrap(),
                        ),
                    ],
                    uuid_bindings: vec![request_binding.clone()],
                },
            )
            .unwrap();
        assert_eq!(current.manifest.internal_record_count, 3);

        let assistant = assistant_record("0000000000000000000000000D", ULID_A, RID, "answer");
        let mut terminal = record("0000000000000000000000000E", ULID_A, RID, "placeholder");
        terminal.kind = SessionRecordKind::TurnTerminal {
            status: TurnTerminalStatus::Complete,
            error_code: None,
        };
        let visible_records = vec![assistant, terminal];
        let tombstone = IdempotencyTombstoneV1 {
            tombstone_version: 1,
            route_domain: RequestRouteDomain::AgentTurn.canonical().into(),
            session_scope: "s".into(),
            request_key: request_binding.request_key.clone(),
            payload_digest: request_binding.payload_digest.clone(),
            terminal_status: TurnTerminalStatus::Complete,
            public_result_digest: request_object_digest(
                &serde_json::to_vec(&visible_records).unwrap(),
            ),
            visible_record_ids: visible_records
                .iter()
                .map(|record| record.record_id.clone())
                .collect(),
        };
        let compacted = store
            .publish_terminal_compacted(&current, visible_records, RID, tombstone.clone())
            .unwrap();
        assert_eq!(compacted.manifest.internal_record_count, 2);

        let replay = store
            .request_replay("s", &request_binding)
            .unwrap()
            .expect("terminal replay");
        assert_eq!(replay.tombstone, Some(tombstone));
        assert!(replay.journal.is_none());
        assert!(replay.outcomes.is_empty());
        assert_eq!(replay.visible_records.len(), 3);
    }

    #[test]
    fn session_history_paginates_with_bounded_cursor_and_bytes() {
        let transport = InMemorySessionV2Transport::default();
        let current = transport.create_session("s").unwrap();
        let store = SessionV2Store::new(transport.clone(), &[7; 32], object_crypto());
        let current = store
            .publish(
                &current,
                SessionCommitV1 {
                    visible_records: vec![
                        record(ULID_A, ULID_A, RID, "first"),
                        record(
                            ULID_B,
                            ULID_B,
                            "00000000-0000-4000-8000-000000000002",
                            "second",
                        ),
                    ],
                    request_objects: vec![],
                    uuid_bindings: vec![],
                },
            )
            .unwrap();
        let first = store.history("s", None, Some(1)).unwrap();
        assert_eq!(first.records.len(), 1);
        let second = store
            .history("s", first.next_cursor.as_deref(), Some(1))
            .unwrap();
        assert_eq!(second.records.len(), 1);
        assert!(second.next_cursor.is_none());

        let next = current
            .manifest
            .apply_delta(&ManifestDelta::default())
            .unwrap();
        transport
            .compare_and_swap_manifest(&current.etag, &next)
            .unwrap()
            .unwrap();
        assert_eq!(
            store.history("s", first.next_cursor.as_deref(), Some(1)),
            Err(SessionV2Error::InvalidCursor)
        );
    }

    #[test]
    fn session_v2_cloud_objects_are_encrypted_and_class_bound() {
        let transport = InMemorySessionV2Transport::default();
        let current = transport.create_session("s").unwrap();
        let store = SessionV2Store::new(transport.clone(), &[8; 32], object_crypto());
        store
            .publish(
                &current,
                SessionCommitV1 {
                    visible_records: vec![record(ULID_A, ULID_A, RID, "private prompt")],
                    request_objects: vec![],
                    uuid_bindings: vec![],
                },
            )
            .unwrap();
        let raw = transport
            .load_immutable(SessionObjectClass::VisibleRecord, ULID_A)
            .unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("private prompt"));
        assert!(object_crypto()
            .open("s", SessionObjectClass::RequestState, ULID_A, &raw)
            .is_err());
    }

    #[test]
    fn manifest_record_limit_counts_visible_internal_and_tombstone_records() {
        let mut manifest = SessionManifestV1::empty("s");
        manifest.visible_record_count = MAX_SESSION_RECORDS - 1;
        manifest.internal_record_count = 1;
        manifest.validate().unwrap();
        assert_eq!(
            manifest.apply_delta(&ManifestDelta {
                internal_records: 1,
                ..Default::default()
            }),
            Err(SessionV2Error::SessionLimit)
        );
    }

    #[test]
    fn session_rejects_prompt_record_and_total_budget_overflow() {
        let mut oversized = record(ULID_A, ULID_A, RID, &"x".repeat(MAX_PROMPT_BYTES + 1));
        assert_eq!(oversized.validate(), Err(SessionV2Error::InvalidRecord));
        oversized.kind = SessionRecordKind::TurnIntent {
            user_text: "valid".into(),
        };
        oversized.validate().unwrap();

        let mut manifest = SessionManifestV1::empty("s");
        manifest.visible_encrypted_bytes = MAX_SESSION_ENCRYPTED_BYTES;
        assert_eq!(
            manifest.apply_delta(&ManifestDelta {
                internal_bytes: 1,
                ..Default::default()
            }),
            Err(SessionV2Error::SessionLimit)
        );
    }

    #[test]
    fn manifest_index_pages_count_bytes_without_double_counting_records() {
        let transport = InMemorySessionV2Transport::default();
        let current = transport.create_session("s").unwrap();
        let store = SessionV2Store::new(transport.clone(), &[5; 32], object_crypto());
        let updated = store
            .publish(
                &current,
                SessionCommitV1 {
                    visible_records: vec![record(ULID_A, ULID_A, RID, "prompt")],
                    request_objects: vec![],
                    uuid_bindings: vec![],
                },
            )
            .unwrap();
        let page = updated.manifest.visible_index_head.as_ref().unwrap();
        let page_bytes = transport
            .load_immutable(SessionObjectClass::VisibleIndex, &page.page_id)
            .unwrap()
            .len() as u64;
        let record_bytes = transport
            .load_immutable(SessionObjectClass::VisibleRecord, ULID_A)
            .unwrap()
            .len() as u64;
        assert_eq!(updated.manifest.visible_record_count, 1);
        assert_eq!(
            updated.manifest.visible_encrypted_bytes,
            page_bytes + record_bytes
        );
    }

    #[test]
    fn provider_context_uses_visible_transcript_not_archived_tool_bodies() {
        let records = vec![
            record(ULID_A, ULID_A, RID, "question"),
            assistant_record(
                ULID_B,
                ULID_A,
                RID,
                "answer with a source reference but no raw tool body",
            ),
            SessionRecordV2 {
                kind: SessionRecordKind::OperationState {
                    code: "tool_result_archived".into(),
                },
                ..record("0000000000000000000000000C", ULID_A, RID, "placeholder")
            },
        ];
        let selected = select_provider_context(&records, None, &ContextBudget::default());
        assert_eq!(selected.len(), 2);
        assert!(selected
            .iter()
            .all(|message| !message.text.contains("archived")));
    }

    #[test]
    fn provider_context_honors_model_token_byte_and_message_budgets() {
        struct TwoTokens;
        impl InputTokenCounter for TwoTokens {
            fn count_input_tokens(&self, _text: &str) -> Option<usize> {
                Some(2)
            }
        }

        let records = vec![
            record(ULID_A, ULID_A, RID, "old question"),
            assistant_record(ULID_B, ULID_A, RID, "old answer"),
            record(
                "0000000000000000000000000C",
                "0000000000000000000000000C",
                "00000000-0000-4000-8000-000000000002",
                "new question",
            ),
            assistant_record(
                "0000000000000000000000000D",
                "0000000000000000000000000C",
                "00000000-0000-4000-8000-000000000002",
                "new answer",
            ),
            record(
                "0000000000000000000000000E",
                "0000000000000000000000000E",
                "00000000-0000-4000-8000-000000000003",
                "incomplete current turn",
            ),
        ];
        let selected = select_provider_context(
            &records,
            Some(&TwoTokens),
            &ContextBudget {
                max_messages: 2,
                max_bytes: 64,
                max_tokens: 4,
            },
        );
        assert_eq!(
            selected
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            vec!["new question", "new answer"]
        );
    }

    #[test]
    fn unknown_tokenizer_charges_one_token_per_utf8_byte() {
        let records = vec![
            record(ULID_A, ULID_A, RID, "1234"),
            assistant_record(ULID_B, ULID_A, RID, "1"),
            record(
                "0000000000000000000000000C",
                "0000000000000000000000000C",
                "00000000-0000-4000-8000-000000000002",
                "1234",
            ),
            assistant_record(
                "0000000000000000000000000D",
                "0000000000000000000000000C",
                "00000000-0000-4000-8000-000000000002",
                "1",
            ),
        ];
        let selected = select_provider_context(
            &records,
            None,
            &ContextBudget {
                max_messages: 64,
                max_bytes: 256,
                max_tokens: 5,
            },
        );
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].text, "1234");
        assert_eq!(selected[1].text, "1");
    }

    #[test]
    fn every_product_post_route_has_one_unique_static_request_domain() {
        let values: BTreeSet<_> = RequestRouteDomain::ALL
            .into_iter()
            .map(RequestRouteDomain::canonical)
            .collect();
        assert_eq!(values.len(), RequestRouteDomain::ALL.len());
        assert!(values
            .iter()
            .all(|value| value.starts_with("post:/api/v1/")));
    }

    #[test]
    fn request_journal_recovers_all_sixteen_provider_steps_in_order() {
        let transport = InMemorySessionV2Transport::default();
        transport.create_session("s").unwrap();
        let mut refs = Vec::new();
        let mut previous = None;
        for step_seq in 0..MAX_PROVIDER_STEPS {
            let outcome_id = format!("{number:026}", number = u64::from(step_seq) + 1);
            let outcome = text_outcome(step_seq, &outcome_id, previous.clone());
            let bytes = serde_json::to_vec(&outcome).unwrap();
            transport
                .stage_immutable(
                    SessionObjectClass::RequestState,
                    &outcome.outcome_id,
                    &bytes,
                )
                .unwrap();
            refs.push(RequestStepRef {
                step_seq: outcome.step_seq,
                outcome_id: outcome.outcome_id.clone(),
                outcome_sha256: bytes_digest(&bytes),
            });
            previous = Some(outcome_id);
        }
        let journal = RequestJournalV1 {
            journal_version: 1,
            session_id: "s".into(),
            request_id: RID.into(),
            turn_id: ULID_A.into(),
            provider_binding: binding(),
            phase: RequestPhase::ProviderStepCompleted,
            next_step_seq: MAX_PROVIDER_STEPS,
            completed_steps: refs,
            read_checkpoints: vec![],
        };
        let loaded = journal.validate_chain(&transport).unwrap();
        assert_eq!(
            loaded
                .iter()
                .map(|value| value.step_seq)
                .collect::<Vec<_>>(),
            (0..MAX_PROVIDER_STEPS).collect::<Vec<_>>()
        );
    }

    #[test]
    fn request_journal_rejects_more_than_sixty_four_tool_checkpoints() {
        let transport = InMemorySessionV2Transport::default();
        let checkpoint = ReadToolCheckpointV1 {
            provider_step_seq: 0,
            tool_use_id: "tool".into(),
            action: ToolAction::Read {
                account: "me".into(),
                service: "onedrive".into(),
                id: "item".into(),
                max_bytes: None,
            },
            policy: crate::RecoveryPolicy::RepeatableReadAndCompare,
            result_sha256: String::new(),
            local_effect: None,
        };
        let journal = RequestJournalV1 {
            journal_version: REQUEST_JOURNAL_VERSION,
            session_id: "s".into(),
            request_id: RID.into(),
            turn_id: ULID_A.into(),
            provider_binding: binding(),
            phase: RequestPhase::Accepted,
            next_step_seq: 0,
            completed_steps: vec![],
            read_checkpoints: vec![checkpoint; MAX_TOOL_CHECKPOINTS + 1],
        };
        assert_eq!(
            journal.validate_chain(&transport),
            Err(SessionV2Error::InvalidJournal)
        );
    }

    #[test]
    fn request_journal_product_loader_decrypts_and_validates_request_objects() {
        let transport = InMemorySessionV2Transport::default();
        let current = transport.create_session("s").unwrap();
        let store = SessionV2Store::new(transport.clone(), &[8; 32], object_crypto());
        let outcome = text_outcome(0, ULID_A, None);
        let bytes = serde_json::to_vec(&outcome).unwrap();
        store
            .publish(
                &current,
                SessionCommitV1 {
                    visible_records: vec![],
                    request_objects: vec![(ULID_A.into(), bytes.clone())],
                    uuid_bindings: vec![],
                },
            )
            .unwrap();
        let journal = RequestJournalV1 {
            journal_version: 1,
            session_id: "s".into(),
            request_id: RID.into(),
            turn_id: ULID_A.into(),
            provider_binding: binding(),
            phase: RequestPhase::ProviderStepCompleted,
            next_step_seq: 1,
            completed_steps: vec![RequestStepRef {
                step_seq: 0,
                outcome_id: ULID_A.into(),
                outcome_sha256: bytes_digest(&bytes),
            }],
            read_checkpoints: vec![],
        };

        let loaded = store.load_request_chain(&journal).unwrap();
        assert_eq!(loaded, vec![outcome]);
        assert!(!String::from_utf8_lossy(
            &transport
                .load_immutable(SessionObjectClass::RequestState, ULID_A)
                .unwrap()
        )
        .contains("step 0"));
    }

    #[test]
    fn request_journal_rejects_gap_duplicate_fork_or_tampered_step_ref() {
        let transport = InMemorySessionV2Transport::default();
        transport.create_session("s").unwrap();
        let outcome = text_outcome(0, ULID_A, None);
        let bytes = serde_json::to_vec(&outcome).unwrap();
        transport
            .stage_immutable(SessionObjectClass::RequestState, ULID_A, &bytes)
            .unwrap();
        let journal = RequestJournalV1 {
            journal_version: 1,
            session_id: "s".into(),
            request_id: RID.into(),
            turn_id: ULID_A.into(),
            provider_binding: binding(),
            phase: RequestPhase::ProviderStepCompleted,
            next_step_seq: 1,
            completed_steps: vec![RequestStepRef {
                step_seq: 1,
                outcome_id: ULID_A.into(),
                outcome_sha256: bytes_digest(&bytes),
            }],
            read_checkpoints: vec![],
        };
        assert_eq!(
            journal.validate_chain(&transport),
            Err(SessionV2Error::InvalidJournal)
        );
    }

    #[test]
    fn request_journal_rejects_changed_credential_generation_before_provider_or_tool_call() {
        let expected = binding();
        let mut current = expected.clone();
        current.credential_generation = "new".into();
        assert_eq!(
            expected.revalidate(&current),
            Err(SessionV2Error::ProviderGenerationChanged)
        );
    }

    #[test]
    fn rejected_tool_help_digest_or_unknown_version_fails_closed() {
        let help = crate::tool::render_rejected_tool_help(
            crate::tool::REJECTED_TOOL_HELP_SCHEMA_VERSION,
            crate::tool::INVALID_TOOL_ARGUMENTS_CODE,
        )
        .unwrap();
        let valid = NormalizedAssistantBlock::RejectedToolUse {
            tool_use_id: "tool-1".into(),
            stable_error_code: crate::tool::INVALID_TOOL_ARGUMENTS_CODE.into(),
            help_schema_version: crate::tool::REJECTED_TOOL_HELP_SCHEMA_VERSION,
            help_digest: tool_result_digest(help.as_bytes()),
        };
        assert_eq!(valid.recover_rejected_tool_help().unwrap(), Some(help));

        let mut tampered = valid.clone();
        let NormalizedAssistantBlock::RejectedToolUse { help_digest, .. } = &mut tampered else {
            unreachable!();
        };
        *help_digest = "0".repeat(64);
        assert_eq!(
            tampered.recover_rejected_tool_help(),
            Err(SessionV2Error::InvalidJournal)
        );

        let mut unknown = valid;
        let NormalizedAssistantBlock::RejectedToolUse {
            help_schema_version,
            ..
        } = &mut unknown
        else {
            unreachable!();
        };
        *help_schema_version += 1;
        assert_eq!(
            unknown.recover_rejected_tool_help(),
            Err(SessionV2Error::InvalidJournal)
        );
    }

    #[test]
    fn duplicate_tool_use_ids_fail_closed_before_execution() {
        let first = RequestStepOutcomeV1 {
            outcome_version: 1,
            outcome_id: ULID_A.into(),
            step_seq: 0,
            previous_outcome_id: None,
            provider: ProductProviderId::Claude,
            model: "model".into(),
            normalized_blocks: vec![NormalizedAssistantBlock::ToolUse {
                tool_use_id: "duplicate".into(),
                action: crate::ToolAction::Search {
                    account: "me".into(),
                    services: vec![],
                    query: "one".into(),
                    limit: None,
                },
            }],
            final_text: None,
            sanitized_usage: None,
            terminal_validation_error: None,
            outcome_digest: String::new(),
        }
        .seal_digest()
        .unwrap();
        let mut second = first.clone();
        second.outcome_id = ULID_B.into();
        second.step_seq = 1;
        second.previous_outcome_id = Some(ULID_A.into());
        second.outcome_digest.clear();
        second = second.seal_digest().unwrap();

        let objects = [first, second]
            .into_iter()
            .map(|outcome| {
                let bytes = serde_json::to_vec(&outcome).unwrap();
                let reference = RequestStepRef {
                    step_seq: outcome.step_seq,
                    outcome_id: outcome.outcome_id.clone(),
                    outcome_sha256: bytes_digest(&bytes),
                };
                (outcome.outcome_id, bytes, reference)
            })
            .collect::<Vec<_>>();
        let journal = RequestJournalV1 {
            journal_version: 1,
            session_id: "s".into(),
            request_id: RID.into(),
            turn_id: ULID_A.into(),
            provider_binding: binding(),
            phase: RequestPhase::ProviderStepCompleted,
            next_step_seq: 2,
            completed_steps: objects.iter().map(|(_, _, r)| r.clone()).collect(),
            read_checkpoints: vec![],
        };
        assert_eq!(
            journal.validate_chain_with(|id| {
                objects
                    .iter()
                    .find(|(object_id, _, _)| object_id == id)
                    .map(|(_, bytes, _)| bytes.clone())
                    .ok_or(SessionV2Error::InvalidJournal)
            }),
            Err(SessionV2Error::DuplicateToolUseId)
        );
    }

    #[test]
    fn manifest_cas_serializes_renewal_and_publication() {
        lease_renews_while_provider_emits_no_events();
        terminal_publication_rechecks_fresh_server_time_and_fence();
    }

    #[test]
    fn takeover_between_stage_and_manifest_cas_leaves_record_invisible() {
        staged_record_is_invisible_until_manifest_cas_succeeds();
        stale_fence_cannot_publish_after_lease_takeover();
    }

    #[test]
    fn new_v2_session_rotates_id_and_seed_away_from_legacy_writer() {
        session_v1_rejects_new_turn_and_never_upgrades_in_place();
        let first = crate::SessionId::new("session-v2-a").unwrap();
        let second = crate::SessionId::new("session-v2-b").unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn request_journal_binds_provider_generation_policy_harness_and_installation() {
        request_journal_rejects_changed_credential_generation_before_provider_or_tool_call();
        let expected = binding();
        let encoded = serde_json::to_vec(&expected).unwrap();
        for value in [
            expected.credential_generation.as_str(),
            expected.oauth_policy_fingerprint.as_str(),
            expected.origin_installation_digest.as_str(),
        ] {
            assert!(encoded
                .windows(value.len())
                .any(|window| window == value.as_bytes()));
        }
        assert!(String::from_utf8(encoded)
            .unwrap()
            .contains("\"harness_contract_version\":1"));
    }

    #[test]
    fn request_journal_recovers_rejected_tool_use_help() {
        rejected_tool_help_digest_or_unknown_version_fails_closed();
    }

    #[test]
    fn request_step_outcome_recovers_normalized_blocks_final_text_and_usage() {
        request_journal_product_loader_decrypts_and_validates_request_objects();
        let outcome = text_outcome(0, ULID_A, None);
        assert_eq!(outcome.final_text.as_deref(), Some("answer"));
        assert_eq!(outcome.sanitized_usage.unwrap().output_tokens, 1);
        assert!(matches!(
            outcome.normalized_blocks.as_slice(),
            [NormalizedAssistantBlock::Text { text }] if text == "answer"
        ));
    }

    #[test]
    fn request_step_outcome_recovers_tool_use_id_and_parsed_action() {
        duplicate_tool_use_ids_fail_closed_before_execution();
    }

    #[test]
    fn request_step_provider_and_model_must_match_provider_binding() {
        request_journal_rejects_changed_credential_generation_before_provider_or_tool_call();
        let expected = binding();
        let mut changed = expected.clone();
        changed.model = "other-model".into();
        assert_eq!(
            expected.revalidate(&changed),
            Err(SessionV2Error::ProviderGenerationChanged)
        );
    }

    #[test]
    fn request_step_outcome_is_internal_and_absent_from_visible_history() {
        session_v2_never_persists_raw_tool_results_or_provider_frames();
        request_journal_product_loader_decrypts_and_validates_request_objects();
    }

    #[test]
    fn request_journal_and_indexes_count_toward_session_byte_quota() {
        session_manifest_is_authoritative_for_head_count_and_byte_budget();
        manifest_index_pages_count_bytes_without_double_counting_records();
    }

    #[test]
    fn recovery_rejects_changed_credential_generation_before_provider_or_tool_call() {
        request_journal_rejects_changed_credential_generation_before_provider_or_tool_call();
    }

    #[test]
    fn each_provider_step_is_persisted_before_its_outbound_call() {
        assert!(RequestPhase::Accepted.permits_automatic_resume());
        assert!(!RequestPhase::ProviderStepStarted.permits_automatic_resume());
        assert_eq!(
            RequestPhase::ProviderStepStarted.recovery_phase(),
            RequestPhase::OutcomeUnknown
        );
        assert!(RequestPhase::ProviderStepCompleted.permits_automatic_resume());
    }

    #[test]
    fn crash_after_provider_step_started_returns_outcome_unknown_without_recall() {
        assert_eq!(
            RequestPhase::ProviderStepStarted.recovery_phase(),
            RequestPhase::OutcomeUnknown
        );
        assert!(!RequestPhase::ProviderStepStarted.permits_automatic_resume());
    }

    #[test]
    fn crash_after_second_provider_call_does_not_repeat_that_call() {
        let journal = RequestJournalV1 {
            journal_version: REQUEST_JOURNAL_VERSION,
            session_id: "s".into(),
            request_id: RID.into(),
            turn_id: ULID_A.into(),
            provider_binding: binding(),
            phase: RequestPhase::ProviderStepStarted,
            next_step_seq: 1,
            completed_steps: vec![RequestStepRef {
                step_seq: 0,
                outcome_id: ULID_B.into(),
                outcome_sha256: "a".repeat(64),
            }],
            read_checkpoints: vec![],
        };
        assert_eq!(journal.next_step_seq, 1);
        assert_eq!(journal.phase.recovery_phase(), RequestPhase::OutcomeUnknown);
        assert!(!journal.phase.permits_automatic_resume());
    }

    #[test]
    fn crash_after_final_provider_step_completed_finishes_single_transcript_commit() {
        request_terminal_compaction_removes_recovery_payload_and_keeps_idempotency();
    }

    #[test]
    fn retry_after_accepted_resumes_once() {
        assert!(RequestPhase::Accepted.permits_automatic_resume());
        assert_eq!(
            RequestPhase::Accepted.recovery_phase(),
            RequestPhase::Accepted
        );
    }

    #[test]
    fn retry_after_committed_does_not_duplicate_user_turn() {
        same_request_id_same_terminal_result_does_not_create_a_second_binding();
    }

    #[test]
    fn same_request_id_same_terminal_result_does_not_call_provider_twice() {
        same_request_id_same_terminal_result_does_not_create_a_second_binding();
    }

    #[test]
    fn account_switch_cannot_resume_prior_provider_journal() {
        let expected = binding();
        let mut switched = expected.clone();
        switched.credential_generation = "switched-generation".into();
        assert_eq!(
            expected.revalidate(&switched),
            Err(SessionV2Error::ProviderGenerationChanged)
        );
    }

    #[test]
    fn other_device_cannot_auto_resume_inflight_provider_journal() {
        let expected = binding();
        let mut other_installation = expected.clone();
        other_installation.origin_installation_digest = "other-installation".into();
        assert_eq!(
            expected.revalidate(&other_installation),
            Err(SessionV2Error::ProviderGenerationChanged)
        );
    }

    #[test]
    fn recovery_rejects_policy_harness_or_installation_mismatch() {
        let expected = binding();
        for current in [
            ProviderAttemptBindingV1 {
                oauth_policy_fingerprint: "other-policy".into(),
                ..expected.clone()
            },
            ProviderAttemptBindingV1 {
                harness_contract_version: expected.harness_contract_version + 1,
                ..expected.clone()
            },
            ProviderAttemptBindingV1 {
                origin_installation_digest: "other-installation".into(),
                ..expected.clone()
            },
        ] {
            assert_eq!(
                expected.revalidate(&current),
                Err(SessionV2Error::ProviderGenerationChanged)
            );
        }
    }
}
