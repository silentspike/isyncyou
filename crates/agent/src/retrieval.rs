//! The read-class tool executor, backed by an [`ArchiveSource`]. It implements
//! [`crate::ToolExecutor`] for `Search`/`Read`/`List`/`Export`, returning JSON whose
//! every result carries `{service, id, path}` source citations (REQ-AGENT-009) and
//! honours a `max_bytes` read budget. The logic is generic over `ArchiveSource`, so it
//! is tested with an in-memory fake (no store).

use crate::activity::{
    truncate_collapsed, ActivityKind, CoverageNoteReason, CoverageNoteV1, PartialResultV1,
    ProgressiveActivityExitV1, ProgressiveActivityFinalizationV1, ProgressiveExitStateV1,
    ProgressiveFinalizationV1, ResultChange, SearchResultPublicV1, SearchStage, StageProgressV1,
    StageStatus, TurnExitKind, ACTIVITY_SCHEMA_VERSION, MAX_PARTIAL_RESULT_ITEMS,
    MAX_PUBLIC_COUNTER, MAX_RESULT_NAME_BYTES,
};
use crate::archive::{
    ArchiveItemPrivateV1, ArchiveSearchSnapshot, ArchiveSource, BodyFtsHit, ItemRef,
    NormalizedSearchScope, StoreSearchDeadline,
};
use crate::progressive_search::{
    candidate_page_digest, CanonicalSearchScopeV1, DeepContinuationStateV1, IssuedCandidateV1,
    SearchActivityBindingV1, SearchCandidateMetadataV1, MAX_CANDIDATES_PER_PAGE,
    MAX_METADATA_SCANNED_PER_ACTIVITY, MAX_METADATA_SCANNED_PER_CALL,
    MAX_METADATA_SCANNED_PER_CANDIDATE_PAGE, MAX_SELECTED_CANDIDATES,
};
use crate::provider::{StreamEvent, TurnEventSink};
use crate::session_v2::SourceRef;
use crate::tool::{ToolAction, ToolClass};
use crate::turn::ToolExecutor;
use crate::AgentError;
use base64::Engine as _;
use ring::rand::SecureRandom as _;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const INCOMPLETE_COVERAGE_NOTE: &str =
    "Search coverage was limited; some archive items may not have been reviewed.";
const BUDGET_COVERAGE_NOTE: &str =
    "Search stopped at its safety limit; some archive items may not have been reviewed.";

/// Default read budget when the model does not set `max_bytes` (64 KiB).
pub const DEFAULT_READ_BUDGET: u64 = 64 * 1024;
/// Default search result cap when the model does not set `limit`.
pub const DEFAULT_SEARCH_LIMIT: u32 = 20;
/// Default flat list page size when the model does not set `limit`.
pub const DEFAULT_LIST_LIMIT: u32 = 50;
/// Hard cap for public list pages; deep-search uses its own candidate budget.
pub const MAX_LIST_LIMIT: u32 = 200;
/// Body preview length (chars) attached to each hit: enough for a real content preview in
/// the expanded card, not just the one-line header. Whitespace is collapsed first.
pub const PREVIEW_CHARS: usize = 1200;
/// Default number of candidate bodies a single deep-search pass reads (budget). Bounds
/// cost on a large mailbox; the model resumes via `next_cursor` to "search deeper".
pub const DEFAULT_DEEP_READS: u32 = 12;
/// Hard cap on a deep-search pass regardless of the model's `max_reads`.
pub const MAX_DEEP_READS: u32 = 40;
pub const MAX_DEEP_TOOL_DURATION: Duration = Duration::from_secs(10);
pub const MAX_METADATA_SCAN_DURATION: Duration = Duration::from_secs(2);
pub const METADATA_PROGRESS_RECORD_INTERVAL: u32 = 25;
pub const METADATA_PROGRESS_TIME_INTERVAL: Duration = Duration::from_millis(250);
pub const MAX_KEYWORD_PROVIDER_ITEMS: usize = 64;
pub const MAX_KEYWORD_PROVIDER_BYTES: usize = 96 * 1_024;
pub const MAX_ACTIVITY_PROVIDER_BYTES: usize = 192 * 1_024;
pub const MAX_TURN_PROGRESSIVE_PROVIDER_BYTES: usize = 256 * 1_024;
pub const MAX_SEARCH_ACTIVITIES_PER_TURN: usize = 4;
pub const MAX_STAGE_EVENTS_PER_ACTIVITY: u16 = 256;
pub const MAX_PARTIAL_EVENTS_PER_ACTIVITY: u16 = 64;
pub const MAX_PUBLIC_PARTIAL_BYTES_PER_ACTIVITY: usize = 512 * 1_024;
pub const MAX_CANDIDATE_PROVIDER_BYTES: usize = 24 * 1_024;
pub const MAX_DEEP_PROVIDER_BYTES: usize = 64 * 1_024;
pub const MAX_PROVIDER_PRIVATE_SNIPPET_BYTES: usize = 1_200;
pub const MAX_DEEP_EXCERPT_BYTES: usize = 1_200;
pub const MAX_VISIBLE_RESULTS: usize = 200;
pub const MAX_KEYWORD_RESULTS: usize = 160;
pub const DEEP_RESULT_RESERVE: usize = 40;
/// The M365 services a deep scan covers when the model names none.
pub const SCANNABLE_SERVICES: &[&str] = &[
    "mail", "onedrive", "calendar", "contacts", "todo", "onenote",
];

/// Executes read-class actions against an [`ArchiveSource`].
pub struct RetrievalExecutor<A: ArchiveSource> {
    source: A,
    progressive: Mutex<ProgressiveTurnState>,
    clock: Arc<dyn ProgressiveClock>,
}

pub trait ProgressiveClock: Send + Sync {
    fn now(&self) -> Instant;
}

struct SystemProgressiveClock;

impl ProgressiveClock for SystemProgressiveClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[derive(Default)]
struct MetadataScanState {
    active_since: Option<Instant>,
    elapsed: Duration,
    scanned_this_call: u32,
}

struct ProgressiveCallTiming {
    clock: Arc<dyn ProgressiveClock>,
    cancellation: crate::CancellationToken,
    overall_deadline: Instant,
    metadata: Arc<Mutex<MetadataScanState>>,
}

struct MetadataScanGuard<'a> {
    timing: &'a ProgressiveCallTiming,
}

impl Drop for MetadataScanGuard<'_> {
    fn drop(&mut self) {
        self.timing.pause_metadata_scan();
    }
}

impl ProgressiveCallTiming {
    fn new(clock: Arc<dyn ProgressiveClock>, cancellation: &crate::CancellationToken) -> Self {
        let overall_deadline = clock.now() + MAX_DEEP_TOOL_DURATION;
        Self {
            clock,
            cancellation: cancellation.clone(),
            overall_deadline,
            metadata: Arc::new(Mutex::new(MetadataScanState::default())),
        }
    }

    fn store_deadline(&self) -> StoreSearchDeadline {
        let clock = Arc::clone(&self.clock);
        let cancellation = self.cancellation.clone();
        let overall_deadline = self.overall_deadline;
        let metadata = Arc::clone(&self.metadata);
        StoreSearchDeadline::new(move || {
            if cancellation.is_cancelled() || clock.now() >= overall_deadline {
                return true;
            }
            let Ok(state) = metadata.lock() else {
                return true;
            };
            state.active_since.is_some_and(|started| {
                state
                    .elapsed
                    .saturating_add(clock.now().saturating_duration_since(started))
                    >= MAX_METADATA_SCAN_DURATION
            })
        })
    }

    fn ensure_overall_active(&self) -> Result<(), AgentError> {
        if self.cancellation.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        if self.clock.now() >= self.overall_deadline {
            return Err(AgentError::Provider(
                "progressive_search_deadline_reached".into(),
            ));
        }
        Ok(())
    }

    fn metadata_scan(&self) -> Result<MetadataScanGuard<'_>, AgentError> {
        self.ensure_overall_active()?;
        let mut state = self
            .metadata
            .lock()
            .map_err(|_| AgentError::Provider("progressive_clock_unavailable".into()))?;
        if state.active_since.is_none() {
            state.active_since = Some(self.clock.now());
        }
        drop(state);
        Ok(MetadataScanGuard { timing: self })
    }

    fn pause_metadata_scan(&self) {
        let Ok(mut state) = self.metadata.lock() else {
            return;
        };
        if let Some(started) = state.active_since.take() {
            state.elapsed = state
                .elapsed
                .saturating_add(self.clock.now().saturating_duration_since(started));
        }
    }

    fn metadata_exhausted(&self) -> Result<bool, AgentError> {
        let state = self
            .metadata
            .lock()
            .map_err(|_| AgentError::Provider("progressive_clock_unavailable".into()))?;
        let elapsed = state.elapsed.saturating_add(
            state
                .active_since
                .map(|started| self.clock.now().saturating_duration_since(started))
                .unwrap_or_default(),
        );
        Ok(state.scanned_this_call >= MAX_METADATA_SCANNED_PER_CALL
            || elapsed >= MAX_METADATA_SCAN_DURATION)
    }

    fn record_metadata(&self) -> Result<(), AgentError> {
        let mut state = self
            .metadata
            .lock()
            .map_err(|_| AgentError::Provider("progressive_clock_unavailable".into()))?;
        state.scanned_this_call = state
            .scanned_this_call
            .checked_add(1)
            .ok_or_else(|| AgentError::Provider("progressive_counter_overflow".into()))?;
        Ok(())
    }

    fn metadata_remaining(&self) -> Result<u32, AgentError> {
        let state = self
            .metadata
            .lock()
            .map_err(|_| AgentError::Provider("progressive_clock_unavailable".into()))?;
        Ok(MAX_METADATA_SCANNED_PER_CALL.saturating_sub(state.scanned_this_call))
    }

    fn now(&self) -> Instant {
        self.clock.now()
    }
}

#[derive(Default)]
struct ProgressiveTurnState {
    activities: BTreeMap<String, ProgressiveActivityState>,
    creation_order: Vec<String>,
    provider_bytes: usize,
}

struct ProgressiveActivityState {
    binding: SearchActivityBindingV1,
    scope: CanonicalSearchScopeV1,
    matched: BTreeSet<(String, String)>,
    consumed_pages: BTreeMap<u16, String>,
    next_sequence: u16,
    visible_keys: BTreeSet<(String, String)>,
    body_reads_used: u16,
    provider_bytes: usize,
    event_budget: ActivityEventBudget,
    names_status: StageStatus,
    bodies_status: StageStatus,
    deep_status: StageStatus,
    coverage_complete: bool,
    budget_reached: bool,
    continuation_available: bool,
}

#[derive(Clone, Copy, Default)]
struct StageCompletion {
    coverage_complete: Option<bool>,
    budget_reached: Option<bool>,
    continuation_available: Option<bool>,
}

#[derive(Debug, Clone, Default)]
struct ActivityEventBudget {
    public_partial_bytes: usize,
    stage_events: u16,
    partial_events: u16,
}

impl ActivityEventBudget {
    fn emit(
        &mut self,
        event: StreamEvent,
        sink: &mut dyn TurnEventSink,
        deliver: bool,
    ) -> Result<(), AgentError> {
        match &event {
            StreamEvent::StageProgress(progress) => {
                progress
                    .validate()
                    .map_err(|_| AgentError::Provider("progressive_public_event_invalid".into()))?;
                self.stage_events = self.stage_events.checked_add(1).ok_or_else(|| {
                    AgentError::Provider("progressive_event_budget_exhausted".into())
                })?;
                if self.stage_events > MAX_STAGE_EVENTS_PER_ACTIVITY {
                    return Err(AgentError::Provider(
                        "progressive_event_budget_exhausted".into(),
                    ));
                }
            }
            StreamEvent::PartialResult(result) => {
                result
                    .validate()
                    .map_err(|_| AgentError::Provider("progressive_public_event_invalid".into()))?;
                self.partial_events = self.partial_events.checked_add(1).ok_or_else(|| {
                    AgentError::Provider("progressive_event_budget_exhausted".into())
                })?;
                let bytes = serde_json::to_vec(&result.public_json())
                    .map_err(|_| AgentError::Provider("progressive_public_event_invalid".into()))?
                    .len();
                self.public_partial_bytes = self
                    .public_partial_bytes
                    .checked_add(bytes)
                    .ok_or_else(|| {
                        AgentError::Provider("progressive_event_budget_exhausted".into())
                    })?;
                if self.partial_events > MAX_PARTIAL_EVENTS_PER_ACTIVITY
                    || self.public_partial_bytes > MAX_PUBLIC_PARTIAL_BYTES_PER_ACTIVITY
                {
                    return Err(AgentError::Provider(
                        "progressive_event_budget_exhausted".into(),
                    ));
                }
            }
            _ => {
                return Err(AgentError::Provider(
                    "progressive_public_event_invalid".into(),
                ));
            }
        }
        if deliver {
            sink.emit(event)
        } else {
            Ok(())
        }
    }
}

struct CandidatePage {
    state: DeepContinuationStateV1,
    continuation: String,
    provider_candidates: Vec<SearchCandidateMetadataV1>,
    issued_candidates: Vec<IssuedCandidateV1>,
    private_items: Vec<ArchiveItemPrivateV1>,
    next_offset: u32,
    has_more: bool,
    budget_reached: bool,
    last_current_item: Option<String>,
}

struct StagedReadResult {
    provider_content: String,
    activity_id: String,
    assistant_sources: Vec<SourceRef>,
    visible_hits: u32,
    coverage_complete: bool,
    budget_reached: bool,
    continuation_available: bool,
}

impl std::ops::Deref for StagedReadResult {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.provider_content
    }
}

impl<A: ArchiveSource> RetrievalExecutor<A> {
    pub fn new(source: A) -> Self {
        Self::with_progressive_clock(source, Arc::new(SystemProgressiveClock))
    }

    pub fn with_progressive_clock(source: A, clock: Arc<dyn ProgressiveClock>) -> Self {
        Self {
            source,
            progressive: Mutex::new(ProgressiveTurnState::default()),
            clock,
        }
    }

    fn ensure_account(&self, action: &ToolAction) -> Result<(), AgentError> {
        let account = match action {
            ToolAction::Search { account, .. }
            | ToolAction::Read { account, .. }
            | ToolAction::List { account, .. }
            | ToolAction::Export { account, .. }
            | ToolAction::RestoreLocal { account, .. }
            | ToolAction::Backup { account, .. }
            | ToolAction::RestoreCloud { account, .. }
            | ToolAction::LiveWrite { account, .. }
            | ToolAction::Share { account, .. } => account,
            ToolAction::DeepSearch { .. } => return Ok(()),
        };
        if account != self.source.account() {
            return Err(AgentError::ToolArgs(format!(
                "account mismatch: tool requested {account}, executor is bound to {}",
                self.source.account()
            )));
        }
        Ok(())
    }

    fn citation_ref(it: &ItemRef) -> serde_json::Value {
        serde_json::json!({
            "service": it.service,
            "id": it.id,
            "path": it.path,
        })
    }

    fn assistant_source(it: &ItemRef) -> SourceRef {
        SourceRef {
            service: it.service.clone(),
            item_id: it.id.clone(),
            label: Some(truncate_collapsed(&it.name, MAX_RESULT_NAME_BYTES)),
        }
    }

    fn new_activity_id() -> Result<String, AgentError> {
        let mut bytes = [0_u8; 16];
        ring::rand::SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| AgentError::Provider("activity_id_unavailable".into()))?;
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
    }

    fn result_key(activity_id: &str, item: &ItemRef) -> String {
        let mut context = ring::digest::Context::new(&ring::digest::SHA256);
        context.update(b"isyncyou-search-result-key/v1");
        for value in [activity_id, item.service.as_str(), item.id.as_str()] {
            context.update(&(value.len() as u32).to_be_bytes());
            context.update(value.as_bytes());
        }
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&context.finish().as_ref()[..16])
    }

    fn public_result(
        activity_id: &str,
        item: &ItemRef,
        change: ResultChange,
    ) -> SearchResultPublicV1 {
        let name = truncate_collapsed(&item.name, MAX_RESULT_NAME_BYTES);
        SearchResultPublicV1 {
            result_key: Self::result_key(activity_id, item),
            change,
            service: item.service.clone(),
            item_id: item.id.clone(),
            name: name.clone(),
            item_type: truncate_collapsed(&item.item_type, 64),
            display_path: None,
            sender: None,
            body_available: item.path.is_some(),
            source: SourceRef {
                service: item.service.clone(),
                item_id: item.id.clone(),
                label: Some(name),
            },
        }
    }

    fn private_source(item: &ArchiveItemPrivateV1) -> SourceRef {
        SourceRef {
            service: item.service.clone(),
            item_id: item.item_id.clone(),
            label: Some(
                truncate_collapsed(&item.name, MAX_RESULT_NAME_BYTES)
                    .trim()
                    .to_string(),
            )
            .filter(|label| !label.is_empty()),
        }
    }

    fn private_public_result(
        activity_id: &str,
        item: &ArchiveItemPrivateV1,
        change: ResultChange,
    ) -> SearchResultPublicV1 {
        let name = {
            let value = truncate_collapsed(&item.name, MAX_RESULT_NAME_BYTES);
            if value.is_empty() {
                "Untitled".to_string()
            } else {
                value
            }
        };
        SearchResultPublicV1 {
            result_key: Self::private_result_key(activity_id, item),
            change,
            service: item.service.clone(),
            item_id: item.item_id.clone(),
            name: name.clone(),
            item_type: {
                let value = truncate_collapsed(&item.item_type, 64);
                if value.is_empty() {
                    "item".to_string()
                } else {
                    value
                }
            },
            display_path: item.display_path.clone(),
            sender: item
                .sender
                .as_deref()
                .map(|value| truncate_collapsed(value, crate::activity::MAX_SENDER_BYTES))
                .filter(|value| !value.is_empty()),
            body_available: item.body_rel_path.is_some(),
            source: SourceRef {
                service: item.service.clone(),
                item_id: item.item_id.clone(),
                label: Some(name),
            },
        }
    }

    fn private_result_key(activity_id: &str, item: &ArchiveItemPrivateV1) -> String {
        let mut context = ring::digest::Context::new(&ring::digest::SHA256);
        context.update(b"isyncyou-search-result-key/v1");
        for value in [
            activity_id.as_bytes(),
            item.service.as_bytes(),
            item.item_id.as_bytes(),
        ] {
            context.update(&(value.len() as u32).to_be_bytes());
            context.update(value);
        }
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&context.finish().as_ref()[..16])
    }

    fn provider_keyword_item(
        item: &ArchiveItemPrivateV1,
        snippet: Option<&str>,
    ) -> serde_json::Value {
        serde_json::json!({
            "service": item.service,
            "id": item.item_id,
            "name": truncate_collapsed(&item.name, MAX_RESULT_NAME_BYTES),
            "item_type": truncate_collapsed(&item.item_type, 64),
            "sender": item.sender.as_deref().map(|value| {
                truncate_collapsed(value, crate::activity::MAX_SENDER_BYTES)
            }),
            "snippet": snippet
                .map(|value| truncate_collapsed(value, MAX_PROVIDER_PRIVATE_SNIPPET_BYTES)),
            "body_available": item.body_rel_path.is_some(),
            "source": Self::private_source(item),
        })
    }

    fn search_provider_content(
        query: &str,
        provider_results: &[serde_json::Value],
        coverage_complete: bool,
        deep_context: Option<&serde_json::Value>,
    ) -> Result<String, AgentError> {
        serde_json::to_string(&serde_json::json!({
            "query": query,
            "returned": provider_results.len(),
            "coverage_complete": coverage_complete,
            "results": provider_results,
            "deep_context": deep_context,
        }))
        .map_err(|_| AgentError::Provider("search_result_encode_failed".into()))
    }

    fn search_provider_content_fits(
        query: &str,
        provider_results: &[serde_json::Value],
        input_budget: &crate::ProviderInputBudgetV1<'_>,
    ) -> Result<bool, AgentError> {
        let content = Self::search_provider_content(query, provider_results, false, None)?;
        Ok(content.len() <= MAX_ACTIVITY_PROVIDER_BYTES
            && input_budget.tokens_for(&content) <= input_budget.remaining_tokens)
    }

    fn candidate_metadata(item: &ArchiveItemPrivateV1) -> SearchCandidateMetadataV1 {
        SearchCandidateMetadataV1 {
            candidate_key: "AAAAAAAAAAAAAAAAAAAAAA".into(),
            service: item.service.clone(),
            name: {
                let value = truncate_collapsed(&item.name, MAX_RESULT_NAME_BYTES);
                if value.is_empty() {
                    "Untitled".into()
                } else {
                    value
                }
            },
            sender: item
                .sender
                .as_deref()
                .map(|value| truncate_collapsed(value, crate::activity::MAX_SENDER_BYTES))
                .filter(|value| !value.is_empty()),
            item_type: {
                let value = truncate_collapsed(&item.item_type, 64);
                if value.is_empty() {
                    "item".into()
                } else {
                    value
                }
            },
            remote_mtime: item
                .remote_mtime
                .as_deref()
                .map(|value| truncate_collapsed(value, 64))
                .filter(|value| !value.is_empty()),
            size: item.size,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn candidate_page(
        &self,
        snapshot: &dyn ArchiveSearchSnapshot,
        authority: &dyn crate::ProgressiveSearchAuthority,
        activity: &SearchActivityBindingV1,
        matched: &BTreeSet<(String, String)>,
        start_offset: u32,
        page_number: u16,
        metadata_scanned_before: u32,
        body_reads_used: u16,
        provider_step: u8,
        timing: &ProgressiveCallTiming,
        on_progress: &mut dyn FnMut(u32, &ArchiveItemPrivateV1) -> Result<(), AgentError>,
    ) -> Result<CandidatePage, AgentError> {
        if metadata_scanned_before != start_offset
            || metadata_scanned_before > MAX_METADATA_SCANNED_PER_ACTIVITY
        {
            return Err(AgentError::Provider(
                "progressive_candidate_offset_invalid".into(),
            ));
        }
        let _metadata_scan = timing.metadata_scan()?;
        let mut raw_offset = start_offset;
        let mut scanned = metadata_scanned_before;
        let mut page_scanned = 0u32;
        let mut items = Vec::new();
        let mut has_more = false;
        let mut budget_reached = false;
        let mut last_current_item = None;

        'pages: loop {
            timing.ensure_overall_active()?;
            if timing.metadata_exhausted()? {
                budget_reached = true;
                has_more = true;
                break;
            }
            if scanned >= MAX_METADATA_SCANNED_PER_ACTIVITY {
                budget_reached = true;
                has_more = true;
                break;
            }
            let remaining_scan = timing
                .metadata_remaining()?
                .min(MAX_METADATA_SCANNED_PER_CANDIDATE_PAGE - page_scanned)
                .min(MAX_METADATA_SCANNED_PER_ACTIVITY - scanned);
            let limit = remaining_scan.min(MAX_LIST_LIMIT);
            let page = match snapshot.metadata_page(limit, raw_offset) {
                Ok(page) => page,
                Err(error) => {
                    timing.ensure_overall_active()?;
                    if timing.metadata_exhausted()? {
                        budget_reached = true;
                        has_more = true;
                        break;
                    }
                    return Err(error);
                }
            };
            timing.ensure_overall_active()?;
            if page.items.is_empty() {
                has_more = false;
                break;
            }
            let page_len = page.items.len();
            for (index, item) in page.items.into_iter().enumerate() {
                timing.ensure_overall_active()?;
                if timing.metadata_exhausted()? {
                    budget_reached = true;
                    has_more = true;
                    break 'pages;
                }
                if scanned >= MAX_METADATA_SCANNED_PER_ACTIVITY {
                    budget_reached = true;
                    has_more = true;
                    break 'pages;
                }
                let candidate = if item.body_rel_path.is_some()
                    && !matched.contains(&(item.service.clone(), item.item_id.clone()))
                {
                    let candidate = Self::candidate_metadata(&item);
                    let mut projected = items
                        .iter()
                        .map(
                            |(_, candidate): &(ArchiveItemPrivateV1, SearchCandidateMetadataV1)| {
                                candidate
                            },
                        )
                        .cloned()
                        .collect::<Vec<_>>();
                    projected.push(candidate.clone());
                    let bytes = serde_json::to_vec(&projected)
                        .map_err(|_| AgentError::Provider("candidate_encode_failed".into()))?
                        .len();
                    if items.len() >= MAX_CANDIDATES_PER_PAGE
                        || bytes > MAX_CANDIDATE_PROVIDER_BYTES
                    {
                        if items.is_empty() {
                            return Err(AgentError::Provider(
                                "progressive_candidate_too_large".into(),
                            ));
                        }
                        has_more = true;
                        break 'pages;
                    }
                    Some(candidate)
                } else {
                    None
                };
                raw_offset = raw_offset
                    .checked_add(1)
                    .ok_or_else(|| AgentError::Provider("progressive_offset_overflow".into()))?;
                scanned = scanned
                    .checked_add(1)
                    .ok_or_else(|| AgentError::Provider("progressive_counter_overflow".into()))?;
                page_scanned = page_scanned
                    .checked_add(1)
                    .ok_or_else(|| AgentError::Provider("progressive_counter_overflow".into()))?;
                timing.record_metadata()?;
                last_current_item = Some(truncate_collapsed(
                    &item.name,
                    crate::activity::MAX_CURRENT_ITEM_BYTES,
                ));
                on_progress(scanned, &item)?;
                if let Some(candidate) = candidate {
                    items.push((item, candidate));
                    if items.len() == MAX_CANDIDATES_PER_PAGE {
                        has_more = index + 1 < page_len || page.has_more;
                        break 'pages;
                    }
                }
                if page_scanned >= MAX_METADATA_SCANNED_PER_CANDIDATE_PAGE {
                    has_more = index + 1 < page_len || page.has_more;
                    break 'pages;
                }
            }
            if !page.has_more {
                break;
            }
        }

        let authoritative_ids = items
            .iter()
            .map(|(item, _)| item.item_id.clone())
            .collect::<Vec<_>>();
        let metadata_without_keys = items
            .iter()
            .map(|(_, metadata)| {
                let mut metadata = metadata.clone();
                metadata.candidate_key.clear();
                metadata
            })
            .collect::<Vec<_>>();
        let digest = candidate_page_digest(&metadata_without_keys, &authoritative_ids)?;
        let state = DeepContinuationStateV1 {
            version: 1,
            activity_id: activity.activity_id.clone(),
            canonical_scope_digest: activity.canonical_scope_digest,
            service_index: 0,
            service_offset: start_offset,
            page: page_number,
            metadata_scanned: scanned,
            body_reads_used,
            candidate_page_digest: digest,
            issued_at_provider_step: provider_step,
        };
        let mut provider_candidates = Vec::with_capacity(items.len());
        let mut issued_candidates = Vec::with_capacity(items.len());
        let mut private_items = Vec::with_capacity(items.len());
        for (item, mut metadata) in items {
            let candidate_key = authority.candidate_key(&state, &item.service, &item.item_id)?;
            metadata.candidate_key = candidate_key.clone();
            let provider_metadata_digest = candidate_page_digest(
                &[SearchCandidateMetadataV1 {
                    candidate_key: String::new(),
                    ..metadata.clone()
                }],
                std::slice::from_ref(&item.item_id),
            )?;
            issued_candidates.push(IssuedCandidateV1 {
                candidate_key,
                service: item.service.clone(),
                item_id: item.item_id.clone(),
                provider_metadata_digest,
            });
            provider_candidates.push(metadata);
            private_items.push(item);
        }
        let continuation = authority.seal_continuation(activity, &state)?;
        Ok(CandidatePage {
            state,
            continuation,
            provider_candidates,
            issued_candidates,
            private_items,
            next_offset: raw_offset,
            has_more,
            budget_reached,
            last_current_item,
        })
    }

    fn public_count(value: usize) -> u32 {
        u32::try_from(value)
            .unwrap_or(MAX_PUBLIC_COUNTER)
            .min(MAX_PUBLIC_COUNTER)
    }

    fn stage_event(
        activity_id: &str,
        stage: SearchStage,
        status: StageStatus,
        scanned: usize,
        hits: usize,
        completion: StageCompletion,
    ) -> StreamEvent {
        Self::stage_event_with_current(activity_id, stage, status, scanned, hits, None, completion)
    }

    fn stage_event_with_current(
        activity_id: &str,
        stage: SearchStage,
        status: StageStatus,
        scanned: usize,
        hits: usize,
        current_item: Option<&str>,
        completion: StageCompletion,
    ) -> StreamEvent {
        StreamEvent::StageProgress(StageProgressV1 {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            activity_id: activity_id.to_string(),
            activity_kind: ActivityKind::ArchiveSearch,
            stage,
            status,
            scanned: Self::public_count(scanned),
            total: None,
            hits: Self::public_count(hits),
            current_item: current_item
                .map(|value| truncate_collapsed(value, crate::activity::MAX_CURRENT_ITEM_BYTES))
                .filter(|value| !value.is_empty()),
            coverage_complete: completion.coverage_complete,
            budget_reached: completion.budget_reached,
            continuation_available: completion.continuation_available,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_partial_batches(
        activity_id: &str,
        stage: SearchStage,
        sequence: &mut u16,
        items: Vec<SearchResultPublicV1>,
        budget: &mut ActivityEventBudget,
        emit: &mut dyn TurnEventSink,
        deliver: bool,
        ensure_active: &mut dyn FnMut() -> Result<(), AgentError>,
    ) -> Result<(), AgentError> {
        for batch in items.chunks(MAX_PARTIAL_RESULT_ITEMS) {
            ensure_active()?;
            budget.emit(
                StreamEvent::PartialResult(PartialResultV1 {
                    schema_version: ACTIVITY_SCHEMA_VERSION,
                    activity_id: activity_id.to_string(),
                    stage,
                    sequence: *sequence,
                    items: batch.to_vec(),
                }),
                emit,
                deliver,
            )?;
            ensure_active()?;
            *sequence = sequence.saturating_add(1);
        }
        Ok(())
    }

    fn source_ref(it: &ItemRef) -> serde_json::Value {
        serde_json::json!({
            "service": it.service,
            "id": it.id,
            "name": it.name,
            "item_type": it.item_type,
            "path": it.path,
            "source": Self::citation_ref(it),
        })
    }

    /// Like [`source_ref`], plus a body `preview` (best-effort) so the UI can render a real
    /// content preview per hit — the card header shows the first line, the expanded panel
    /// shows this whole preview — and the model can judge relevance without a second
    /// round-trip. Whitespace-collapsed and capped at [`PREVIEW_CHARS`]; empty when the
    /// item has no archived body or the read fails.
    fn hit_json(&self, it: &ItemRef) -> serde_json::Value {
        let mut v = Self::source_ref(it);
        let (preview, body_available) = match it.path.as_ref() {
            Some(_) => match self.source.read_body(&it.service, &it.id) {
                Ok(body) => (Self::body_preview(&it.service, &body), true),
                Err(_) => (String::new(), false),
            },
            None => (String::new(), false),
        };
        v["snippet"] = serde_json::Value::String(preview);
        v["body_available"] = serde_json::Value::Bool(body_available);
        v
    }

    /// Turn a raw archived body into a readable, whitespace-collapsed preview capped at
    /// [`PREVIEW_CHARS`]. Mail bodies are full `.eml` MIME, so extract the `text/plain`
    /// part (tag-stripped `text/html` fallback) — the same [`isyncyou_connectors::mime`]
    /// text the store indexes — instead of showing raw headers/boundaries. Other services
    /// archive already-readable bodies (ics/vCard/text), so pass them through.
    fn body_preview(service: &str, body: &[u8]) -> String {
        Self::body_model_text(service, body)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(PREVIEW_CHARS)
            .collect()
    }

    /// Convert archived bytes into the text exposed to the model. Counters and
    /// truncation are defined over this final UTF-8 string, not the raw archive bytes.
    fn body_model_text(service: &str, body: &[u8]) -> String {
        #[cfg(feature = "retrieval")]
        let text = if service == "mail" {
            // Real mail is `.eml` MIME → extract the readable text. A non-MIME/plain body
            // (or a message extract_text can't parse) falls back to raw so nothing is lost.
            let t = isyncyou_connectors::mime::extract_text(body);
            if t.trim().is_empty() {
                String::from_utf8_lossy(body).into_owned()
            } else {
                t
            }
        } else {
            String::from_utf8_lossy(body).into_owned()
        };
        #[cfg(not(feature = "retrieval"))]
        let text = {
            let _ = service;
            String::from_utf8_lossy(body).into_owned()
        };
        text
    }

    fn content_kind(service: &str) -> &'static str {
        match service {
            "mail" => "mail-text",
            "calendar" | "contacts" | "todo" => "json",
            "onedrive" | "onenote" => "text",
            _ => "text",
        }
    }

    fn utf8_budget_slice(text: &str, max_bytes: usize) -> (&str, bool) {
        if text.len() <= max_bytes {
            return (text, false);
        }
        let mut end = max_bytes;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        (&text[..end], true)
    }

    fn list_limit(limit: Option<u32>) -> u32 {
        limit.unwrap_or(DEFAULT_LIST_LIMIT).min(MAX_LIST_LIMIT)
    }

    fn page_items(items: Vec<ItemRef>, limit: u32, offset: u32) -> Vec<ItemRef> {
        items
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect()
    }

    fn search(
        &self,
        services: &[String],
        query: &str,
        limit: Option<u32>,
    ) -> Result<String, AgentError> {
        let cap = limit.unwrap_or(DEFAULT_SEARCH_LIMIT) as usize;
        let mut hits = self.source.search_names(query)?;
        let mut seen: std::collections::HashSet<(String, String)> = hits
            .iter()
            .map(|i| (i.service.clone(), i.id.clone()))
            .collect();
        for (service, id) in self.source.search_bodies(query)? {
            if seen.insert((service.clone(), id.clone())) {
                if let Some(it) = self.source.get(&service, &id)? {
                    hits.push(it);
                }
            }
        }
        if !services.is_empty() {
            hits.retain(|i| services.iter().any(|s| s == &i.service));
        }
        let total = hits.len();
        let results: Vec<serde_json::Value> =
            hits.iter().take(cap).map(|it| self.hit_json(it)).collect();
        Ok(serde_json::json!({
            "query": query,
            "returned": results.len(),
            "total_matches": total,
            "results": results,
        })
        .to_string())
    }

    /// Progressive search (S-AG.18/#643): the same merge as [`search`], but run as
    /// visible stages — **stage 1** fast name/subject match, **stage 2** full-text over
    /// bodies — emitting a `SearchStage` boundary + a `PartialResult` of the newly-added,
    /// deduped, source-tagged hits after each, so the UI grows the list live. The final
    /// JSON is identical to [`search`] (plus a `deep_search_hint`): the returned string is
    /// what the model answers from; **stage 3** is the [`deep_search`](Self::deep_search)
    /// op, which the model calls (guided by that hint) to surface matches whose wording the
    /// query never contains.
    fn search_staged(
        &self,
        services: &[String],
        query: &str,
        limit: Option<u32>,
        emit: &mut dyn TurnEventSink,
    ) -> Result<StagedReadResult, AgentError> {
        self.search_staged_for_activity(services, query, limit, &Self::new_activity_id()?, emit)
    }

    fn search_staged_for_activity(
        &self,
        services: &[String],
        query: &str,
        limit: Option<u32>,
        activity_id: &str,
        emit: &mut dyn TurnEventSink,
    ) -> Result<StagedReadResult, AgentError> {
        let mut sequence = 0_u16;
        let mut event_budget = ActivityEventBudget::default();
        let in_scope =
            |it: &ItemRef| services.is_empty() || services.iter().any(|s| s == &it.service);
        let mut seen: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        let mut hits: Vec<ItemRef> = Vec::new();

        // Stage 1 — fast name/subject match (indexed).
        event_budget.emit(
            Self::stage_event(
                activity_id,
                SearchStage::Names,
                StageStatus::Running,
                0,
                0,
                StageCompletion::default(),
            ),
            emit,
            true,
        )?;
        let mut stage1 = Vec::new();
        for it in self.source.search_names(query)? {
            if in_scope(&it) && seen.insert((it.service.clone(), it.id.clone())) {
                stage1.push(Self::public_result(activity_id, &it, ResultChange::Add));
                hits.push(it);
            }
        }
        Self::emit_partial_batches(
            activity_id,
            SearchStage::Names,
            &mut sequence,
            stage1,
            &mut event_budget,
            emit,
            true,
            &mut || Ok(()),
        )?;
        event_budget.emit(
            Self::stage_event(
                activity_id,
                SearchStage::Names,
                StageStatus::Complete,
                hits.len(),
                hits.len(),
                StageCompletion {
                    coverage_complete: Some(true),
                    budget_reached: Some(false),
                    continuation_available: Some(false),
                },
            ),
            emit,
            true,
        )?;

        // Stage 2 — full-text over indexed bodies (only items stage 1 didn't already have).
        event_budget.emit(
            Self::stage_event(
                activity_id,
                SearchStage::Bodies,
                StageStatus::Running,
                0,
                hits.len(),
                StageCompletion::default(),
            ),
            emit,
            true,
        )?;
        let mut stage2 = Vec::new();
        for (service, id) in self.source.search_bodies(query)? {
            if seen.insert((service.clone(), id.clone())) {
                if let Some(it) = self.source.get(&service, &id)? {
                    if in_scope(&it) {
                        stage2.push(Self::public_result(activity_id, &it, ResultChange::Add));
                        hits.push(it);
                    }
                }
            }
        }
        Self::emit_partial_batches(
            activity_id,
            SearchStage::Bodies,
            &mut sequence,
            stage2,
            &mut event_budget,
            emit,
            true,
            &mut || Ok(()),
        )?;
        event_budget.emit(
            Self::stage_event(
                activity_id,
                SearchStage::Bodies,
                StageStatus::Complete,
                hits.len(),
                hits.len(),
                StageCompletion {
                    coverage_complete: Some(true),
                    budget_reached: Some(false),
                    continuation_available: Some(true),
                },
            ),
            emit,
            true,
        )?;

        let cap = limit.unwrap_or(DEFAULT_SEARCH_LIMIT) as usize;
        let total = hits.len();
        let results: Vec<serde_json::Value> =
            hits.iter().take(cap).map(|it| self.hit_json(it)).collect();
        let provider_content = serde_json::json!({
            "query": query,
            "returned": results.len(),
            "total_matches": total,
            "results": results,
            "deep_search_hint": "Keyword passes (name + full-text) are done. If the user may mean something these missed (different wording/synonyms), call `deep-search` — it scans metadata and reads unmatched candidate bodies for you to judge; resume with its `next_cursor` to search deeper.",
        })
        .to_string();
        let assistant_sources = hits
            .iter()
            .take(cap)
            .take(crate::session_v2::MAX_SOURCE_REFS)
            .map(Self::assistant_source)
            .collect();
        Ok(StagedReadResult {
            provider_content,
            activity_id: activity_id.to_owned(),
            assistant_sources,
            visible_hits: Self::public_count(hits.len().min(cap)),
            coverage_complete: true,
            budget_reached: false,
            continuation_available: true,
        })
    }

    fn search_progressive(
        &self,
        services: &[String],
        query: &str,
        limit: Option<u32>,
        context: &mut crate::ReadExecutionContext<'_, '_>,
    ) -> Result<StagedReadResult, AgentError> {
        let authority = context.progressive_authority.ok_or_else(|| {
            AgentError::Provider("progressive_search_authority_unavailable".into())
        })?;
        let scope = CanonicalSearchScopeV1::new(
            context.binding.resolved_account_key.clone(),
            query.to_string(),
            services.to_vec(),
            limit,
        )?;
        let normalized_scope =
            NormalizedSearchScope::new(scope.account().to_string(), scope.services().to_vec())?;
        let activity_id = authority.activity_id(context.binding)?;
        let binding = SearchActivityBindingV1 {
            session_id: context.binding.session_id.clone(),
            request_id: context.binding.request_id.clone(),
            activity_id: activity_id.clone(),
            originating_search_tool_use_id: context.binding.tool_use_id.clone(),
            canonical_scope_digest: scope.digest(),
        };
        let timing = ProgressiveCallTiming::new(Arc::clone(&self.clock), context.cancellation);
        let deadline = timing.store_deadline();
        let keyword_limit = usize::try_from(scope.effective_keyword_limit())
            .unwrap_or(MAX_KEYWORD_RESULTS)
            .min(MAX_KEYWORD_RESULTS);
        let mut sequence = 0_u16;
        let mut event_budget = ActivityEventBudget::default();
        let mut matched = BTreeSet::new();
        let mut visible_keys = BTreeSet::new();
        let mut provider_results = Vec::<serde_json::Value>::new();
        let mut provider_indexes = BTreeMap::<(String, String), usize>::new();
        let mut assistant_sources = Vec::new();

        {
            let mut state = self
                .progressive
                .lock()
                .map_err(|_| AgentError::Provider("progressive_state_unavailable".into()))?;
            if state.activities.len() >= MAX_SEARCH_ACTIVITIES_PER_TURN {
                return Err(AgentError::Provider(
                    "progressive_activity_budget_exhausted".into(),
                ));
            }
            if state.activities.contains_key(&activity_id) {
                return Err(AgentError::Provider(
                    "progressive_activity_binding_conflict".into(),
                ));
            }
            state.creation_order.push(activity_id.clone());
            state.activities.insert(
                activity_id.clone(),
                ProgressiveActivityState {
                    binding: binding.clone(),
                    scope: scope.clone(),
                    matched: BTreeSet::new(),
                    consumed_pages: BTreeMap::new(),
                    next_sequence: 0,
                    visible_keys: BTreeSet::new(),
                    body_reads_used: 0,
                    provider_bytes: 0,
                    event_budget: ActivityEventBudget::default(),
                    names_status: StageStatus::Queued,
                    bodies_status: StageStatus::Queued,
                    deep_status: StageStatus::Queued,
                    coverage_complete: false,
                    budget_reached: false,
                    continuation_available: false,
                },
            );
        }

        for stage in [SearchStage::Names, SearchStage::Bodies, SearchStage::Deep] {
            event_budget.emit(
                Self::stage_event(
                    &activity_id,
                    stage,
                    StageStatus::Queued,
                    0,
                    0,
                    StageCompletion::default(),
                ),
                context.events,
                context.mode == crate::ReadExecutionMode::Live,
            )?;
        }
        self.progressive
            .lock()
            .map_err(|_| AgentError::Provider("progressive_state_unavailable".into()))?
            .activities
            .get_mut(&activity_id)
            .ok_or_else(|| AgentError::Provider("progressive_state_unavailable".into()))?
            .names_status = StageStatus::Running;
        event_budget.emit(
            Self::stage_event(
                &activity_id,
                SearchStage::Names,
                StageStatus::Running,
                0,
                0,
                StageCompletion::default(),
            ),
            context.events,
            context.mode == crate::ReadExecutionMode::Live,
        )?;
        timing.ensure_overall_active()?;
        let snapshot = self
            .source
            .begin_search_snapshot(&normalized_scope, &deadline)?;
        timing.ensure_overall_active()?;
        let names_limit = keyword_limit
            .saturating_sub(DEEP_RESULT_RESERVE.min(keyword_limit))
            .max(1);
        timing.ensure_overall_active()?;
        let names = match snapshot.search_names_page(query, names_limit as u32, 0) {
            Ok(page) => page,
            Err(error) => {
                timing.ensure_overall_active()?;
                return Err(error);
            }
        };
        timing.ensure_overall_active()?;
        let mut name_public = Vec::new();
        let mut names_scanned = 0_usize;
        let mut names_last_progress_at = timing.now();
        let mut names_last_current = None;
        for item in names.items {
            timing.ensure_overall_active()?;
            names_scanned = names_scanned.saturating_add(1);
            names_last_current = Some(item.name.clone());
            let now = timing.now();
            if names_scanned == 1
                || names_scanned.is_multiple_of(METADATA_PROGRESS_RECORD_INTERVAL as usize)
                || now.saturating_duration_since(names_last_progress_at)
                    >= METADATA_PROGRESS_TIME_INTERVAL
            {
                event_budget.emit(
                    Self::stage_event_with_current(
                        &activity_id,
                        SearchStage::Names,
                        StageStatus::Running,
                        names_scanned,
                        visible_keys.len(),
                        Some(&item.name),
                        StageCompletion::default(),
                    ),
                    context.events,
                    context.mode == crate::ReadExecutionMode::Live,
                )?;
                names_last_progress_at = now;
            }
            let key = (item.service.clone(), item.item_id.clone());
            if !matched.insert(key.clone()) {
                continue;
            }
            visible_keys.insert(key.clone());
            name_public.push(Self::private_public_result(
                &activity_id,
                &item,
                ResultChange::Add,
            ));
            let provider_item = Self::provider_keyword_item(&item, None);
            let mut projected = provider_results.clone();
            projected.push(provider_item.clone());
            let projected_bytes = serde_json::to_vec(&projected)
                .map_err(|_| AgentError::Provider("search_result_encode_failed".into()))?
                .len();
            if provider_results.len() < MAX_KEYWORD_PROVIDER_ITEMS
                && projected_bytes <= MAX_KEYWORD_PROVIDER_BYTES
                && Self::search_provider_content_fits(query, &projected, context.input_budget)?
            {
                provider_indexes.insert(key, provider_results.len());
                provider_results.push(provider_item);
                if assistant_sources.len() < crate::session_v2::MAX_SOURCE_REFS {
                    assistant_sources.push(Self::private_source(&item));
                }
            }
        }
        Self::emit_partial_batches(
            &activity_id,
            SearchStage::Names,
            &mut sequence,
            name_public,
            &mut event_budget,
            context.events,
            context.mode == crate::ReadExecutionMode::Live,
            &mut || timing.ensure_overall_active(),
        )?;
        event_budget.emit(
            Self::stage_event_with_current(
                &activity_id,
                SearchStage::Names,
                StageStatus::Complete,
                names_scanned,
                visible_keys.len(),
                names_last_current.as_deref(),
                StageCompletion {
                    coverage_complete: Some(!names.has_more),
                    budget_reached: Some(names.has_more),
                    continuation_available: Some(false),
                },
            ),
            context.events,
            context.mode == crate::ReadExecutionMode::Live,
        )?;
        timing.ensure_overall_active()?;
        {
            let mut state = self
                .progressive
                .lock()
                .map_err(|_| AgentError::Provider("progressive_state_unavailable".into()))?;
            let activity = state
                .activities
                .get_mut(&activity_id)
                .ok_or_else(|| AgentError::Provider("progressive_state_unavailable".into()))?;
            activity.names_status = StageStatus::Complete;
            activity.matched = matched.clone();
            activity.visible_keys = visible_keys.clone();
            activity.next_sequence = sequence;
            activity.event_budget = event_budget.clone();
        }

        {
            let mut state = self
                .progressive
                .lock()
                .map_err(|_| AgentError::Provider("progressive_state_unavailable".into()))?;
            state
                .activities
                .get_mut(&activity_id)
                .ok_or_else(|| AgentError::Provider("progressive_state_unavailable".into()))?
                .bodies_status = StageStatus::Running;
        }
        event_budget.emit(
            Self::stage_event(
                &activity_id,
                SearchStage::Bodies,
                StageStatus::Running,
                0,
                visible_keys.len(),
                StageCompletion::default(),
            ),
            context.events,
            context.mode == crate::ReadExecutionMode::Live,
        )?;
        timing.ensure_overall_active()?;
        let bodies = match snapshot.search_bodies_page(query, keyword_limit as u32, 0) {
            Ok(page) => page,
            Err(error) => {
                timing.ensure_overall_active()?;
                return Err(error);
            }
        };
        timing.ensure_overall_active()?;
        let mut body_public = Vec::new();
        let mut bodies_scanned = 0_usize;
        let mut bodies_last_progress_at = timing.now();
        let mut bodies_last_current = None;
        for BodyFtsHit { item, snippet } in bodies.items {
            timing.ensure_overall_active()?;
            bodies_scanned = bodies_scanned.saturating_add(1);
            bodies_last_current = Some(item.name.clone());
            let now = timing.now();
            if bodies_scanned == 1
                || bodies_scanned.is_multiple_of(METADATA_PROGRESS_RECORD_INTERVAL as usize)
                || now.saturating_duration_since(bodies_last_progress_at)
                    >= METADATA_PROGRESS_TIME_INTERVAL
            {
                event_budget.emit(
                    Self::stage_event_with_current(
                        &activity_id,
                        SearchStage::Bodies,
                        StageStatus::Running,
                        bodies_scanned,
                        visible_keys.len(),
                        Some(&item.name),
                        StageCompletion::default(),
                    ),
                    context.events,
                    context.mode == crate::ReadExecutionMode::Live,
                )?;
                bodies_last_progress_at = now;
            }
            let key = (item.service.clone(), item.item_id.clone());
            let is_new = matched.insert(key.clone());
            if is_new && visible_keys.len() >= keyword_limit {
                continue;
            }
            let change = if is_new {
                visible_keys.insert(key.clone());
                ResultChange::Add
            } else {
                ResultChange::Enrich
            };
            body_public.push(Self::private_public_result(&activity_id, &item, change));
            let provider_item = Self::provider_keyword_item(&item, Some(&snippet));
            if let Some(index) = provider_indexes.get(&key).copied() {
                let mut projected = provider_results.clone();
                projected[index] = provider_item.clone();
                if serde_json::to_vec(&projected)
                    .map_err(|_| AgentError::Provider("search_result_encode_failed".into()))?
                    .len()
                    <= MAX_KEYWORD_PROVIDER_BYTES
                    && Self::search_provider_content_fits(query, &projected, context.input_budget)?
                {
                    provider_results[index] = provider_item;
                }
            } else if provider_results.len() < MAX_KEYWORD_PROVIDER_ITEMS {
                let mut projected = provider_results.clone();
                projected.push(provider_item.clone());
                if serde_json::to_vec(&projected)
                    .map_err(|_| AgentError::Provider("search_result_encode_failed".into()))?
                    .len()
                    <= MAX_KEYWORD_PROVIDER_BYTES
                    && Self::search_provider_content_fits(query, &projected, context.input_budget)?
                {
                    provider_indexes.insert(key, provider_results.len());
                    provider_results.push(provider_item);
                    if assistant_sources.len() < crate::session_v2::MAX_SOURCE_REFS {
                        assistant_sources.push(Self::private_source(&item));
                    }
                }
            }
        }
        Self::emit_partial_batches(
            &activity_id,
            SearchStage::Bodies,
            &mut sequence,
            body_public,
            &mut event_budget,
            context.events,
            context.mode == crate::ReadExecutionMode::Live,
            &mut || timing.ensure_overall_active(),
        )?;
        event_budget.emit(
            Self::stage_event_with_current(
                &activity_id,
                SearchStage::Bodies,
                StageStatus::Complete,
                bodies_scanned,
                visible_keys.len(),
                bodies_last_current.as_deref(),
                StageCompletion {
                    coverage_complete: Some(!bodies.has_more),
                    budget_reached: Some(bodies.has_more || visible_keys.len() >= keyword_limit),
                    continuation_available: Some(false),
                },
            ),
            context.events,
            context.mode == crate::ReadExecutionMode::Live,
        )?;
        {
            let mut state = self
                .progressive
                .lock()
                .map_err(|_| AgentError::Provider("progressive_state_unavailable".into()))?;
            let activity = state
                .activities
                .get_mut(&activity_id)
                .ok_or_else(|| AgentError::Provider("progressive_state_unavailable".into()))?;
            activity.bodies_status = StageStatus::Complete;
            activity.matched = matched.clone();
            activity.visible_keys = visible_keys.clone();
            activity.next_sequence = sequence;
            activity.event_budget = event_budget.clone();
        }

        {
            let mut state = self
                .progressive
                .lock()
                .map_err(|_| AgentError::Provider("progressive_state_unavailable".into()))?;
            state
                .activities
                .get_mut(&activity_id)
                .ok_or_else(|| AgentError::Provider("progressive_state_unavailable".into()))?
                .deep_status = StageStatus::Running;
        }
        event_budget.emit(
            Self::stage_event(
                &activity_id,
                SearchStage::Deep,
                StageStatus::Running,
                0,
                visible_keys.len(),
                StageCompletion::default(),
            ),
            context.events,
            context.mode == crate::ReadExecutionMode::Live,
        )?;
        let mut last_progress_scanned = 0_u32;
        let mut last_progress_at = timing.now();
        let deliver = context.mode == crate::ReadExecutionMode::Live;
        let candidate_page = {
            let mut on_progress = |scanned: u32, item: &ArchiveItemPrivateV1| {
                let now = timing.now();
                if scanned.saturating_sub(last_progress_scanned)
                    >= METADATA_PROGRESS_RECORD_INTERVAL
                    || now.saturating_duration_since(last_progress_at)
                        >= METADATA_PROGRESS_TIME_INTERVAL
                {
                    timing.ensure_overall_active()?;
                    event_budget.emit(
                        Self::stage_event_with_current(
                            &activity_id,
                            SearchStage::Deep,
                            StageStatus::Running,
                            scanned as usize,
                            visible_keys.len(),
                            Some(&item.name),
                            StageCompletion::default(),
                        ),
                        context.events,
                        deliver,
                    )?;
                    last_progress_scanned = scanned;
                    last_progress_at = now;
                }
                Ok(())
            };
            self.candidate_page(
                snapshot.as_ref(),
                authority,
                &binding,
                &matched,
                0,
                0,
                0,
                0,
                context.provider_step_seq,
                &timing,
                &mut on_progress,
            )?
        };
        let continuation_candidate = context.provider_steps_remaining_after_current >= 2
            && !candidate_page.budget_reached
            && (!candidate_page.provider_candidates.is_empty() || candidate_page.has_more);
        let coverage_complete = candidate_page.provider_candidates.is_empty()
            && !candidate_page.has_more
            && !candidate_page.budget_reached;
        let candidate_deep_context = continuation_candidate.then(|| {
            serde_json::json!({
                "contract_version": 1,
                "activity_id": activity_id,
                "continuation": candidate_page.continuation,
                "candidates": candidate_page.provider_candidates,
                "scanned": candidate_page.state.metadata_scanned,
                "total": serde_json::Value::Null,
                "body_reads_used": 0,
                "body_reads_remaining": MAX_DEEP_READS,
                "provider_steps_remaining": context.provider_steps_remaining_after_current,
            })
        });
        let candidate_provider_content = Self::search_provider_content(
            query,
            &provider_results,
            coverage_complete,
            candidate_deep_context.as_ref(),
        )?;
        let continuation_fits = candidate_provider_content.len() <= MAX_ACTIVITY_PROVIDER_BYTES
            && context.input_budget.tokens_for(&candidate_provider_content)
                <= context.input_budget.remaining_tokens;
        let can_continue = continuation_candidate && continuation_fits;
        let model_budget_reached = continuation_candidate && !continuation_fits;
        let deep_status = if can_continue {
            StageStatus::Running
        } else {
            // The metadata scan has already run, even when coverage is bounded by the
            // provider-step or input budget. Report that limitation through the coverage
            // fields instead of emitting the invalid running -> skipped transition.
            StageStatus::Complete
        };
        let budget_reached = candidate_page.budget_reached
            || (!coverage_complete && context.provider_steps_remaining_after_current < 2)
            || model_budget_reached;
        let provider_content = if can_continue {
            candidate_provider_content
        } else {
            Self::search_provider_content(query, &provider_results, coverage_complete, None)?
        };
        if provider_content.len() > MAX_ACTIVITY_PROVIDER_BYTES {
            return Err(AgentError::Provider(
                "progressive_provider_budget_exhausted".into(),
            ));
        }
        context.input_budget.charge(&provider_content)?;
        {
            let mut state = self
                .progressive
                .lock()
                .map_err(|_| AgentError::Provider("progressive_state_unavailable".into()))?;
            let activity = state
                .activities
                .get_mut(&activity_id)
                .ok_or_else(|| AgentError::Provider("progressive_state_unavailable".into()))?;
            activity.deep_status = deep_status;
            activity.coverage_complete = coverage_complete;
            activity.budget_reached = budget_reached;
            activity.continuation_available = can_continue;
        }
        event_budget.emit(
            Self::stage_event_with_current(
                &activity_id,
                SearchStage::Deep,
                deep_status,
                candidate_page.state.metadata_scanned as usize,
                visible_keys.len(),
                candidate_page.last_current_item.as_deref(),
                StageCompletion {
                    coverage_complete: Some(coverage_complete),
                    budget_reached: Some(budget_reached),
                    continuation_available: Some(can_continue),
                },
            ),
            context.events,
            context.mode == crate::ReadExecutionMode::Live,
        )?;
        {
            let mut state = self
                .progressive
                .lock()
                .map_err(|_| AgentError::Provider("progressive_state_unavailable".into()))?;
            state
                .activities
                .get_mut(&activity_id)
                .ok_or_else(|| AgentError::Provider("progressive_state_unavailable".into()))?
                .event_budget = event_budget.clone();
        }
        timing.ensure_overall_active()?;
        let visible_hit_count = visible_keys.len();
        let mut state = self
            .progressive
            .lock()
            .map_err(|_| AgentError::Provider("progressive_state_unavailable".into()))?;
        let projected = state
            .provider_bytes
            .checked_add(provider_content.len())
            .ok_or_else(|| AgentError::Provider("progressive_provider_budget_exhausted".into()))?;
        if projected > MAX_TURN_PROGRESSIVE_PROVIDER_BYTES {
            return Err(AgentError::Provider(
                "progressive_provider_budget_exhausted".into(),
            ));
        }
        let activity = state
            .activities
            .get_mut(&activity_id)
            .ok_or_else(|| AgentError::Provider("progressive_state_unavailable".into()))?;
        activity.binding = binding;
        activity.scope = scope;
        activity.matched = matched;
        activity.next_sequence = sequence;
        activity.visible_keys = visible_keys;
        activity.provider_bytes = provider_content.len();
        activity.event_budget = event_budget;
        activity.deep_status = deep_status;
        activity.coverage_complete = coverage_complete;
        activity.budget_reached = budget_reached;
        activity.continuation_available = can_continue;
        state.provider_bytes = projected;
        Ok(StagedReadResult {
            provider_content,
            activity_id,
            assistant_sources,
            visible_hits: Self::public_count(visible_hit_count),
            coverage_complete,
            budget_reached,
            continuation_available: can_continue,
        })
    }

    fn deep_progressive(
        &self,
        activity_id: &str,
        continuation: &str,
        selected_keys: &[String],
        context: &mut crate::ReadExecutionContext<'_, '_>,
    ) -> Result<StagedReadResult, AgentError> {
        if selected_keys.len() > MAX_SELECTED_CANDIDATES
            || selected_keys.iter().collect::<BTreeSet<_>>().len() != selected_keys.len()
        {
            return Err(AgentError::ToolArgs(
                "invalid deep-search candidates".into(),
            ));
        }
        if context.provider_steps_remaining_after_current == 0 {
            return Err(AgentError::Provider(
                "provider_step_budget_exhausted".into(),
            ));
        }
        let authority = context.progressive_authority.ok_or_else(|| {
            AgentError::Provider("progressive_search_authority_unavailable".into())
        })?;
        let (
            binding,
            scope,
            matched,
            consumed_pages,
            mut sequence,
            mut visible_keys,
            body_reads_used,
            activity_provider_bytes,
            turn_provider_bytes,
            mut event_budget,
        ) = {
            let state = self
                .progressive
                .lock()
                .map_err(|_| AgentError::Provider("progressive_state_unavailable".into()))?;
            let activity = state
                .activities
                .get(activity_id)
                .ok_or_else(|| AgentError::ToolArgs("unknown search activity".into()))?;
            (
                activity.binding.clone(),
                activity.scope.clone(),
                activity.matched.clone(),
                activity.consumed_pages.clone(),
                activity.next_sequence,
                activity.visible_keys.clone(),
                activity.body_reads_used,
                activity.provider_bytes,
                state.provider_bytes,
                activity.event_budget.clone(),
            )
        };
        if binding.session_id != context.binding.session_id
            || binding.request_id != context.binding.request_id
            || binding.activity_id != activity_id
        {
            return Err(AgentError::ToolArgs(
                "search activity binding mismatch".into(),
            ));
        }
        let continuation_state = authority.open_continuation(&binding, continuation)?;
        let consumed_by = consumed_pages.get(&continuation_state.page).cloned();
        let replaying_consumed_page = consumed_by.is_some();
        if let Some(owner) = consumed_by {
            if owner != context.binding.tool_use_id
                || context.mode != crate::ReadExecutionMode::RecoveryCompare
            {
                return Err(AgentError::ToolArgs(
                    "deep-search page already consumed".into(),
                ));
            }
        }
        let normalized_scope =
            NormalizedSearchScope::new(scope.account().to_string(), scope.services().to_vec())?;
        let timing = ProgressiveCallTiming::new(Arc::clone(&self.clock), context.cancellation);
        let deadline = timing.store_deadline();
        timing.ensure_overall_active()?;
        let snapshot = self
            .source
            .begin_search_snapshot(&normalized_scope, &deadline)?;
        timing.ensure_overall_active()?;
        event_budget.emit(
            Self::stage_event(
                activity_id,
                SearchStage::Deep,
                StageStatus::Running,
                continuation_state.metadata_scanned as usize,
                visible_keys.len(),
                StageCompletion::default(),
            ),
            context.events,
            context.mode == crate::ReadExecutionMode::Live,
        )?;
        let mut last_progress_scanned = continuation_state.metadata_scanned;
        let mut last_progress_at = timing.now();
        let deliver = context.mode == crate::ReadExecutionMode::Live;
        let page = {
            let mut on_progress = |scanned: u32, item: &ArchiveItemPrivateV1| {
                let now = timing.now();
                if scanned.saturating_sub(last_progress_scanned)
                    >= METADATA_PROGRESS_RECORD_INTERVAL
                    || now.saturating_duration_since(last_progress_at)
                        >= METADATA_PROGRESS_TIME_INTERVAL
                {
                    timing.ensure_overall_active()?;
                    event_budget.emit(
                        Self::stage_event_with_current(
                            activity_id,
                            SearchStage::Deep,
                            StageStatus::Running,
                            scanned as usize,
                            visible_keys.len(),
                            Some(&item.name),
                            StageCompletion::default(),
                        ),
                        context.events,
                        deliver,
                    )?;
                    last_progress_scanned = scanned;
                    last_progress_at = now;
                }
                Ok(())
            };
            self.candidate_page(
                snapshot.as_ref(),
                authority,
                &binding,
                &matched,
                continuation_state.service_offset,
                continuation_state.page,
                continuation_state.service_offset,
                continuation_state.body_reads_used,
                continuation_state.issued_at_provider_step,
                &timing,
                &mut on_progress,
            )?
        };
        if page.state != continuation_state || page.continuation != continuation {
            return Err(AgentError::Provider(
                "archive_changed_restart_search".into(),
            ));
        }
        let selected = selected_keys
            .iter()
            .map(|key| {
                page.issued_candidates
                    .iter()
                    .position(|candidate| &candidate.candidate_key == key)
                    .ok_or_else(|| AgentError::ToolArgs("invalid deep-search candidate".into()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let replay_body_reads_used = if replaying_consumed_page {
            continuation_state.body_reads_used
        } else {
            body_reads_used
        };
        if usize::from(replay_body_reads_used).saturating_add(selected.len())
            > usize::try_from(MAX_DEEP_READS).unwrap()
        {
            return Err(AgentError::Provider(
                "deep_search_body_budget_exhausted".into(),
            ));
        }
        // Body text is not available until after a verified-handle read. Reserve the
        // worst-case per-call provider allowance before that first read so an exhausted
        // model context cannot cause otherwise unusable archive I/O.
        if !selected.is_empty() {
            context
                .input_budget
                .ensure_can_charge_upper_bound(MAX_DEEP_PROVIDER_BYTES)?;
            if !replaying_consumed_page
                && (MAX_DEEP_PROVIDER_BYTES
                    > MAX_ACTIVITY_PROVIDER_BYTES.saturating_sub(activity_provider_bytes)
                    || MAX_DEEP_PROVIDER_BYTES
                        > MAX_TURN_PROGRESSIVE_PROVIDER_BYTES.saturating_sub(turn_provider_bytes))
            {
                return Err(AgentError::Provider(
                    "progressive_provider_budget_exhausted".into(),
                ));
            }
        }

        let mut provider_results = Vec::new();
        let mut public_results = Vec::new();
        let mut assistant_sources = Vec::new();
        for index in selected {
            timing.ensure_overall_active()?;
            let item = page
                .private_items
                .get(index)
                .ok_or_else(|| AgentError::Provider("candidate_page_invalid".into()))?;
            let locator = item
                .body_rel_path
                .as_ref()
                .ok_or_else(|| AgentError::Provider("archive_body_unavailable".into()))?;
            let bytes = match self.source.read_private_body(locator, &deadline) {
                Ok(bytes) => bytes,
                Err(_) => {
                    // The archive adapter intentionally exposes only a closed error. Re-check
                    // the authoritative cancellation/deadline state before treating it as a
                    // genuinely unavailable body so no result can escape after interruption.
                    timing.ensure_overall_active()?;
                    let mut unavailable = item.clone();
                    unavailable.body_rel_path = None;
                    public_results.push(Self::private_public_result(
                        activity_id,
                        &unavailable,
                        ResultChange::Add,
                    ));
                    provider_results.push(serde_json::json!({
                        "candidate_key": page.issued_candidates[index].candidate_key,
                        "service": item.service,
                        "name": truncate_collapsed(&item.name, MAX_RESULT_NAME_BYTES),
                        "item_type": truncate_collapsed(&item.item_type, 64),
                        "body_available": false,
                        "source": Self::private_source(item),
                    }));
                    if assistant_sources.len() < crate::session_v2::MAX_SOURCE_REFS {
                        assistant_sources.push(Self::private_source(item));
                    }
                    timing.ensure_overall_active()?;
                    continue;
                }
            };
            timing.ensure_overall_active()?;
            let text = Self::body_model_text(&item.service, &bytes);
            let excerpt = truncate_collapsed(&text, MAX_DEEP_EXCERPT_BYTES);
            let key = (item.service.clone(), item.item_id.clone());
            let change = if visible_keys.insert(key) {
                ResultChange::Add
            } else {
                ResultChange::Enrich
            };
            public_results.push(Self::private_public_result(activity_id, item, change));
            provider_results.push(serde_json::json!({
                "candidate_key": page.issued_candidates[index].candidate_key,
                "service": item.service,
                "name": truncate_collapsed(&item.name, MAX_RESULT_NAME_BYTES),
                "item_type": truncate_collapsed(&item.item_type, 64),
                "excerpt": excerpt,
                "source": Self::private_source(item),
            }));
            if assistant_sources.len() < crate::session_v2::MAX_SOURCE_REFS {
                assistant_sources.push(Self::private_source(item));
            }
        }
        Self::emit_partial_batches(
            activity_id,
            SearchStage::Deep,
            &mut sequence,
            public_results,
            &mut event_budget,
            context.events,
            context.mode == crate::ReadExecutionMode::Live,
            &mut || timing.ensure_overall_active(),
        )?;
        timing.ensure_overall_active()?;

        let new_body_reads = replay_body_reads_used
            .checked_add(
                u16::try_from(provider_results.len())
                    .map_err(|_| AgentError::Provider("progressive_counter_overflow".into()))?,
            )
            .ok_or_else(|| AgentError::Provider("progressive_counter_overflow".into()))?;
        let next_page = if page.has_more
            && !page.budget_reached
            && new_body_reads < MAX_DEEP_READS as u16
            && context.provider_steps_remaining_after_current >= 2
        {
            let mut on_progress = |scanned: u32, item: &ArchiveItemPrivateV1| {
                let now = timing.now();
                if scanned.saturating_sub(last_progress_scanned)
                    >= METADATA_PROGRESS_RECORD_INTERVAL
                    || now.saturating_duration_since(last_progress_at)
                        >= METADATA_PROGRESS_TIME_INTERVAL
                {
                    timing.ensure_overall_active()?;
                    event_budget.emit(
                        Self::stage_event_with_current(
                            activity_id,
                            SearchStage::Deep,
                            StageStatus::Running,
                            scanned as usize,
                            visible_keys.len(),
                            Some(&item.name),
                            StageCompletion::default(),
                        ),
                        context.events,
                        deliver,
                    )?;
                    last_progress_scanned = scanned;
                    last_progress_at = now;
                }
                Ok(())
            };
            Some(
                self.candidate_page(
                    snapshot.as_ref(),
                    authority,
                    &binding,
                    &matched,
                    page.next_offset,
                    page.state
                        .page
                        .checked_add(1)
                        .ok_or_else(|| AgentError::Provider("progressive_page_overflow".into()))?,
                    page.state.metadata_scanned,
                    new_body_reads,
                    context.provider_step_seq,
                    &timing,
                    &mut on_progress,
                )?,
            )
        } else {
            None
        };
        let can_continue = next_page.as_ref().is_some_and(|next| {
            !next.budget_reached && (!next.provider_candidates.is_empty() || next.has_more)
        });
        let next_page_budget_reached = next_page.as_ref().is_some_and(|next| next.budget_reached);
        let budget_reached = page.budget_reached
            || next_page_budget_reached
            || new_body_reads >= MAX_DEEP_READS as u16
            || (page.has_more && context.provider_steps_remaining_after_current < 2);
        let deep_context = next_page.as_ref().filter(|_| can_continue).map(|next| {
            serde_json::json!({
                "contract_version": 1,
                "activity_id": activity_id,
                "continuation": next.continuation,
                "candidates": next.provider_candidates,
                "scanned": next.state.metadata_scanned,
                "total": serde_json::Value::Null,
                "body_reads_used": new_body_reads,
                "body_reads_remaining": MAX_DEEP_READS.saturating_sub(u32::from(new_body_reads)),
                "provider_steps_remaining": context.provider_steps_remaining_after_current,
            })
        });
        let selected_count = provider_results.len();
        let coverage_complete = selected_count == page.provider_candidates.len()
            && !page.has_more
            && !page.budget_reached;
        let provider_content = serde_json::json!({
            "activity_id": activity_id,
            "stage": "deep",
            "selected": provider_results,
            "coverage_complete": coverage_complete,
            "budget_reached": budget_reached,
            "deep_context": deep_context,
        })
        .to_string();
        if provider_content.len() > MAX_DEEP_PROVIDER_BYTES {
            return Err(AgentError::Provider(
                "progressive_provider_budget_exhausted".into(),
            ));
        }
        context.input_budget.charge(&provider_content)?;
        let new_activity_bytes = if replaying_consumed_page {
            activity_provider_bytes
        } else {
            activity_provider_bytes
                .checked_add(provider_content.len())
                .ok_or_else(|| {
                    AgentError::Provider("progressive_provider_budget_exhausted".into())
                })?
        };
        if !replaying_consumed_page && new_activity_bytes > MAX_ACTIVITY_PROVIDER_BYTES {
            return Err(AgentError::Provider(
                "progressive_provider_budget_exhausted".into(),
            ));
        }
        let mut state = self
            .progressive
            .lock()
            .map_err(|_| AgentError::Provider("progressive_state_unavailable".into()))?;
        let projected_turn = if replaying_consumed_page {
            state.provider_bytes
        } else {
            state
                .provider_bytes
                .checked_add(provider_content.len())
                .ok_or_else(|| {
                    AgentError::Provider("progressive_provider_budget_exhausted".into())
                })?
        };
        if !replaying_consumed_page && projected_turn > MAX_TURN_PROGRESSIVE_PROVIDER_BYTES {
            return Err(AgentError::Provider(
                "progressive_provider_budget_exhausted".into(),
            ));
        }
        if !replaying_consumed_page {
            let activity = state
                .activities
                .get_mut(activity_id)
                .ok_or_else(|| AgentError::Provider("progressive_state_unavailable".into()))?;
            activity
                .consumed_pages
                .insert(page.state.page, context.binding.tool_use_id.clone());
            activity.next_sequence = sequence;
            activity.visible_keys = visible_keys;
            activity.body_reads_used = new_body_reads;
            activity.provider_bytes = new_activity_bytes;
            activity.event_budget = event_budget;
            activity.coverage_complete = coverage_complete;
            activity.budget_reached = budget_reached;
            activity.continuation_available = can_continue;
            state.provider_bytes = projected_turn;
        }
        Ok(StagedReadResult {
            provider_content,
            activity_id: activity_id.to_string(),
            assistant_sources,
            visible_hits: Self::public_count(selected_count),
            coverage_complete,
            budget_reached,
            continuation_available: can_continue,
        })
    }

    /// Stage 3 — agentic deep read (S-AG.18/#643). Keyword search (`search`) only finds
    /// items whose name or body literally contains the query; this surfaces the ones it
    /// missed so the model can judge them semantically. It builds the keyword-matched set
    /// (to exclude), metadata-scans the in-scope services, then reads up to a **budget** of
    /// unmatched candidate bodies from `cursor`, returning short snippets + a coverage note
    /// and a `next_cursor` to continue. It never reads the whole mailbox in one pass.
    #[cfg(test)]
    fn deep_search(
        &self,
        services: &[String],
        query: &str,
        cursor: Option<u32>,
        max_reads: Option<u32>,
        emit: &mut dyn TurnEventSink,
    ) -> Result<StagedReadResult, AgentError> {
        let activity_id = Self::new_activity_id()?;
        let mut sequence = 0_u16;
        let mut event_budget = ActivityEventBudget::default();
        event_budget.emit(
            Self::stage_event(
                &activity_id,
                SearchStage::Deep,
                StageStatus::Running,
                0,
                0,
                StageCompletion::default(),
            ),
            emit,
            true,
        )?;

        // Items the keyword passes already found — the deep read only covers the misses.
        let mut matched: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        for it in self.source.search_names(query)? {
            matched.insert((it.service, it.id));
        }
        for pair in self.source.search_bodies(query)? {
            matched.insert(pair);
        }

        // Metadata scan (names only, cheap) across the in-scope services, stable order,
        // keeping only unmatched items that actually have an archived body to read.
        let scan: Vec<String> = if services.is_empty() {
            SCANNABLE_SERVICES.iter().map(|s| s.to_string()).collect()
        } else {
            services.to_vec()
        };
        let mut candidates: Vec<ItemRef> = Vec::new();
        for svc in &scan {
            for it in self.source.list_page(svc, u32::MAX, 0)? {
                if it.path.is_some() && !matched.contains(&(it.service.clone(), it.id.clone())) {
                    candidates.push(it);
                }
            }
        }
        let total = candidates.len();

        // Budgeted read window from `cursor`.
        let start = cursor.unwrap_or(0) as usize;
        let budget = max_reads.unwrap_or(DEFAULT_DEEP_READS).min(MAX_DEEP_READS) as usize;
        let mut read_items: Vec<serde_json::Value> = Vec::new();
        let mut public_items = Vec::new();
        for it in candidates.iter().skip(start).take(budget) {
            // Same content-preview shape as the keyword hits (header + body preview).
            let value = self.hit_json(it);
            public_items.push(Self::public_result(&activity_id, it, ResultChange::Add));
            read_items.push(value);
        }
        let next = start + read_items.len();
        let more = next < total;

        Self::emit_partial_batches(
            &activity_id,
            SearchStage::Deep,
            &mut sequence,
            public_items,
            &mut event_budget,
            emit,
            true,
            &mut || Ok(()),
        )?;
        event_budget.emit(
            Self::stage_event(
                &activity_id,
                SearchStage::Deep,
                StageStatus::Complete,
                candidates.len(),
                read_items.len(),
                StageCompletion {
                    coverage_complete: Some(!more),
                    budget_reached: Some(more),
                    continuation_available: Some(more),
                },
            ),
            emit,
            true,
        )?;

        let coverage = if more {
            format!(
                "Read {} of {total} unmatched candidates (from {start}). Judge these by \
                 content; to search deeper, call deep-search again with cursor={next}.",
                read_items.len()
            )
        } else {
            format!("Read all {total} unmatched candidates — this is the full deep scan.")
        };
        let provider_content = serde_json::json!({
            "query": query,
            "stage": "deep",
            "candidates_total": total,
            "read": read_items.len(),
            "cursor": start,
            "next_cursor": if more { Some(next) } else { None },
            "budget_reached": more,
            "candidates": read_items,
            "coverage_note": coverage,
        })
        .to_string();
        let assistant_sources = candidates
            .iter()
            .skip(start)
            .take(read_items.len())
            .take(crate::session_v2::MAX_SOURCE_REFS)
            .map(Self::assistant_source)
            .collect();
        Ok(StagedReadResult {
            provider_content,
            activity_id,
            assistant_sources,
            visible_hits: Self::public_count(read_items.len()),
            coverage_complete: !more,
            budget_reached: more,
            continuation_available: more,
        })
    }

    fn read(&self, service: &str, id: &str, max_bytes: Option<u64>) -> Result<String, AgentError> {
        let item = self
            .source
            .get(service, id)?
            .ok_or_else(|| AgentError::ToolArgs(format!("no item {service}/{id}")))?;
        let bytes = self.source.read_body(service, id)?;
        let text = Self::body_model_text(service, &bytes);
        let budget = max_bytes.unwrap_or(DEFAULT_READ_BUDGET) as usize;
        let (content, truncated) = Self::utf8_budget_slice(&text, budget);
        let source = Self::citation_ref(&item);
        Ok(serde_json::json!({
            "service": item.service,
            "id": item.id,
            "name": item.name,
            "path": item.path,
            "source": source,
            "content_kind": Self::content_kind(service),
            "bytes_total": text.len(),
            "bytes_returned": content.len(),
            "truncated": truncated,
            "content": content,
        })
        .to_string())
    }

    fn list(
        &self,
        service: &str,
        parent: Option<&str>,
        limit: Option<u32>,
        offset: Option<u32>,
    ) -> Result<String, AgentError> {
        let limit = Self::list_limit(limit);
        let offset = offset.unwrap_or(0);
        let items = match parent {
            Some("root") | Some("") => Self::page_items(self.source.roots(service)?, limit, offset),
            Some(parent) => Self::page_items(self.source.children(service, parent)?, limit, offset),
            None => self.source.list_page(service, limit, offset)?,
        };
        let count = self.source.count(service)?;
        let results: Vec<serde_json::Value> = items.iter().map(Self::source_ref).collect();
        Ok(serde_json::json!({
            "service": service,
            "parent": parent,
            "limit": limit,
            "offset": offset,
            "service_total": count,
            "returned": results.len(),
            "results": results,
        })
        .to_string())
    }

    fn export(&self, service: &str, id: &str) -> Result<String, AgentError> {
        let item = self
            .source
            .get(service, id)?
            .ok_or_else(|| AgentError::ToolArgs(format!("no item {service}/{id}")))?;
        let bytes = self.source.read_body(service, id)?;
        let (format, content) = convert_export(service, &bytes)?;
        let source = Self::citation_ref(&item);
        Ok(serde_json::json!({
            "service": item.service,
            "id": item.id,
            "path": item.path,
            "source": source,
            "format": format,
            "content": content,
        })
        .to_string())
    }
}

/// Convert an archived body to a portable export. Calendar→ics / Contacts→vcard need
/// the `retrieval` feature (the connectors converters); otherwise everything is `raw`.
fn convert_export(service: &str, bytes: &[u8]) -> Result<(&'static str, String), AgentError> {
    #[cfg(feature = "retrieval")]
    {
        if service == "calendar" || service == "contacts" {
            let v: serde_json::Value = serde_json::from_slice(bytes)
                .map_err(|e| AgentError::Provider(format!("export parse: {e}")))?;
            return Ok(match service {
                "calendar" => ("ics", isyncyou_connectors::event_to_ics(&v)),
                _ => ("vcard", isyncyou_connectors::contact_to_vcard(&v)),
            });
        }
    }
    let _ = service;
    Ok(("raw", String::from_utf8_lossy(bytes).into_owned()))
}

impl<A: ArchiveSource> ToolExecutor for RetrievalExecutor<A> {
    fn execute_read(&self, action: &ToolAction) -> Result<String, AgentError> {
        // Defensive: the loop only routes read-class actions here.
        if action.class() != ToolClass::Read {
            return Err(AgentError::ToolArgs(format!(
                "{} is destructive and must go through confirmation, not the read executor",
                action.op()
            )));
        }
        self.ensure_account(action)?;
        match action {
            ToolAction::Search {
                services,
                query,
                limit,
                ..
            } => self.search(services, query, *limit),
            ToolAction::DeepSearch { .. } => Err(AgentError::Provider(
                "deep_search_requires_turn_context".into(),
            )),
            ToolAction::Read {
                service,
                id,
                max_bytes,
                ..
            } => self.read(service, id, *max_bytes),
            ToolAction::List {
                service,
                parent,
                limit,
                offset,
                ..
            } => self.list(service, parent.as_deref(), *limit, *offset),
            ToolAction::Export { service, id, .. } => self.export(service, id),
            // restore-local is read-class but writes a local file; app-host owns the
            // controlled restore root and wraps this executor when that operation is enabled.
            ToolAction::RestoreLocal { .. } => Err(AgentError::ToolArgs(
                "restore-local is implemented in the app-host operations layer".into(),
            )),
            other => Err(AgentError::ToolArgs(format!(
                "unsupported read op: {}",
                other.op()
            ))),
        }
    }

    fn execute_read_streamed(
        &self,
        action: &ToolAction,
        emit: &mut dyn TurnEventSink,
    ) -> Result<String, AgentError> {
        if action.class() != ToolClass::Read {
            return self.execute_read(action);
        }
        self.ensure_account(action)?;
        // Search + deep-search run as visible stages; every other read is single-shot.
        match action {
            ToolAction::Search {
                services,
                query,
                limit,
                ..
            } => Ok(self
                .search_staged(services, query, *limit, emit)?
                .provider_content),
            ToolAction::DeepSearch { .. } => Err(AgentError::Provider(
                "deep_search_requires_turn_context".into(),
            )),
            _ => self.execute_read(action),
        }
    }

    fn execute_read_with_context(
        &self,
        action: &ToolAction,
        context: crate::ReadExecutionContext<'_, '_>,
    ) -> Result<crate::ReadExecutionOutputV2, AgentError> {
        if action.class() != ToolClass::Read {
            return Err(AgentError::ToolArgs(
                "destructive action cannot execute as read".into(),
            ));
        }
        self.ensure_account(action)?;
        if (!matches!(action, ToolAction::DeepSearch { .. })
            && action.account() != context.binding.resolved_account_key)
            || context.binding.admission_account_digest
                != crate::admission_account_digest(&context.binding.resolved_account_key)?
        {
            return Err(AgentError::Provider("turn_account_binding_changed".into()));
        }
        if context.cancellation.is_cancelled() {
            return Err(AgentError::Cancelled);
        }

        match action {
            ToolAction::Search {
                services,
                query,
                limit,
                ..
            } => {
                let mut context = context;
                let result = self.search_progressive(services, query, *limit, &mut context)?;
                let public_sources = result
                    .assistant_sources
                    .iter()
                    .take(crate::activity::MAX_PUBLIC_TOOL_RESULT_SOURCES)
                    .cloned()
                    .collect();
                Ok(crate::ReadExecutionOutputV2::Search(
                    crate::SeparatedSearchOutputV2 {
                        provider_content: result.provider_content,
                        public_projection: crate::PublicToolResultV1 {
                            schema_version: ACTIVITY_SCHEMA_VERSION,
                            operation: "search".into(),
                            activity_id: result.activity_id,
                            visible_hits: result.visible_hits,
                            coverage_complete: result.coverage_complete,
                            budget_reached: result.budget_reached,
                            continuation_available: result.continuation_available
                                && context.provider_steps_remaining_after_current >= 2,
                            sources: public_sources,
                        },
                        assistant_sources: result.assistant_sources,
                    },
                ))
            }
            ToolAction::DeepSearch {
                activity_id,
                continuation,
                candidates,
            } => {
                let mut context = context;
                let result =
                    self.deep_progressive(activity_id, continuation, candidates, &mut context)?;
                let public_sources = result
                    .assistant_sources
                    .iter()
                    .take(crate::activity::MAX_PUBLIC_TOOL_RESULT_SOURCES)
                    .cloned()
                    .collect();
                Ok(crate::ReadExecutionOutputV2::DeepSearch(
                    crate::SeparatedSearchOutputV2 {
                        provider_content: result.provider_content,
                        public_projection: crate::PublicToolResultV1 {
                            schema_version: ACTIVITY_SCHEMA_VERSION,
                            operation: "deep-search".into(),
                            activity_id: result.activity_id,
                            visible_hits: result.visible_hits,
                            coverage_complete: result.coverage_complete,
                            budget_reached: result.budget_reached,
                            continuation_available: result.continuation_available
                                && context.provider_steps_remaining_after_current >= 2,
                            sources: public_sources,
                        },
                        assistant_sources: result.assistant_sources,
                    },
                ))
            }
            ToolAction::Read { service, id, .. } => {
                let content = self.execute_read(action)?;
                context.input_budget.charge(&content)?;
                let sources = self
                    .source
                    .get(service, id)?
                    .map(|item| vec![Self::assistant_source(&item)])
                    .unwrap_or_default();
                Ok(crate::ReadExecutionOutputV2::Read(
                    crate::ExistingSharedReadOutputV2 {
                        content,
                        untrusted: true,
                        assistant_sources: sources,
                    },
                ))
            }
            ToolAction::List { .. } => {
                let content = self.execute_read(action)?;
                context.input_budget.charge(&content)?;
                Ok(crate::ReadExecutionOutputV2::List(
                    crate::ExistingSharedReadOutputV2 {
                        content,
                        untrusted: true,
                        assistant_sources: Vec::new(),
                    },
                ))
            }
            ToolAction::Export { service, id, .. } => {
                let content = self.execute_read(action)?;
                context.input_budget.charge(&content)?;
                let sources = self
                    .source
                    .get(service, id)?
                    .map(|item| vec![Self::assistant_source(&item)])
                    .unwrap_or_default();
                Ok(crate::ReadExecutionOutputV2::Export(
                    crate::ExistingSharedReadOutputV2 {
                        content,
                        untrusted: true,
                        assistant_sources: sources,
                    },
                ))
            }
            ToolAction::RestoreLocal { .. } => Err(AgentError::ToolArgs(
                "restore-local is implemented in the app-host operations layer".into(),
            )),
            ToolAction::Backup { .. }
            | ToolAction::RestoreCloud { .. }
            | ToolAction::LiveWrite { .. }
            | ToolAction::Share { .. } => unreachable!("classified destructive above"),
        }
    }

    fn finish_with_exit(
        &self,
        exit: TurnExitKind,
        proposed_text: Option<String>,
        assistant_sources: Vec<SourceRef>,
        events: &mut dyn TurnEventSink,
    ) -> Result<crate::TurnExitOutputV1, AgentError> {
        let mut state = self
            .progressive
            .lock()
            .map_err(|_| AgentError::Provider("progressive_state_unavailable".into()))?;
        let mut exit_activities = Vec::with_capacity(state.creation_order.len());
        let mut final_activities = Vec::with_capacity(state.creation_order.len());
        let mut terminal_event_delivery = crate::TerminalEventDelivery::Accepted;
        let order = state.creation_order.clone();

        for activity_id in order {
            let activity = state
                .activities
                .get_mut(&activity_id)
                .ok_or_else(|| AgentError::Provider("progressive_state_unavailable".into()))?;
            let terminal_status = |current| match current {
                StageStatus::Queued => Some(match exit {
                    TurnExitKind::Cancelled => StageStatus::Cancelled,
                    _ => StageStatus::Skipped,
                }),
                StageStatus::Running => Some(match exit {
                    TurnExitKind::Final => StageStatus::Complete,
                    TurnExitKind::PendingConfirmation => StageStatus::Skipped,
                    TurnExitKind::Cancelled => StageStatus::Cancelled,
                    TurnExitKind::ProviderError
                    | TurnExitKind::OutcomeUnknown
                    | TurnExitKind::StepLimit => StageStatus::Failed,
                }),
                StageStatus::Complete
                | StageStatus::Failed
                | StageStatus::Skipped
                | StageStatus::Cancelled => None,
            };
            for stage in [SearchStage::Names, SearchStage::Bodies, SearchStage::Deep] {
                let current = match stage {
                    SearchStage::Names => activity.names_status,
                    SearchStage::Bodies => activity.bodies_status,
                    SearchStage::Deep => activity.deep_status,
                };
                let Some(status) = terminal_status(current) else {
                    continue;
                };
                match stage {
                    SearchStage::Names => activity.names_status = status,
                    SearchStage::Bodies => activity.bodies_status = status,
                    SearchStage::Deep => activity.deep_status = status,
                }
                let is_deep = stage == SearchStage::Deep;
                let event = Self::stage_event(
                    &activity_id,
                    stage,
                    status,
                    if is_deep {
                        usize::from(activity.body_reads_used)
                    } else {
                        activity.matched.len()
                    },
                    activity.visible_keys.len(),
                    StageCompletion {
                        coverage_complete: is_deep.then_some(activity.coverage_complete),
                        budget_reached: is_deep.then_some(activity.budget_reached),
                        continuation_available: is_deep.then_some(false),
                    },
                );
                if activity.event_budget.emit(event, events, true).is_err() {
                    terminal_event_delivery = crate::TerminalEventDelivery::Unavailable;
                }
            }
            activity.continuation_available = false;
            exit_activities.push(ProgressiveActivityExitV1 {
                activity_id: activity_id.clone(),
                names_status: activity.names_status,
                bodies_status: activity.bodies_status,
                deep_status: activity.deep_status,
            });
            final_activities.push(ProgressiveActivityFinalizationV1 {
                activity_id,
                deep_status: activity.deep_status,
                coverage_complete: activity.coverage_complete,
                budget_reached: activity.budget_reached,
                continuation_available: false,
            });
        }

        let terminal_code = match exit {
            TurnExitKind::Final => None,
            TurnExitKind::PendingConfirmation => Some("pending_confirmation"),
            TurnExitKind::ProviderError => Some("provider_error"),
            TurnExitKind::Cancelled => Some("cancelled"),
            TurnExitKind::OutcomeUnknown => Some("turn_outcome_unknown"),
            TurnExitKind::StepLimit => Some("turn_step_limit"),
        }
        .map(str::to_owned);
        let exit_state = ProgressiveExitStateV1 {
            exit_version: 1,
            exit_kind: exit,
            activities: exit_activities,
            terminal_code,
        };

        let completion = if exit == TurnExitKind::Final {
            let mut final_text = proposed_text
                .ok_or_else(|| AgentError::Provider("turn_finalization_missing".into()))?;
            let coverage_reason = final_activities
                .iter()
                .any(|activity| activity.budget_reached)
                .then_some(CoverageNoteReason::BudgetReached)
                .or_else(|| {
                    final_activities
                        .iter()
                        .any(|activity| !activity.coverage_complete)
                        .then_some(CoverageNoteReason::Incomplete)
                });
            if let Some(reason) = coverage_reason {
                let note = match reason {
                    CoverageNoteReason::Incomplete => INCOMPLETE_COVERAGE_NOTE,
                    CoverageNoteReason::BudgetReached => BUDGET_COVERAGE_NOTE,
                };
                let separator = if final_text.is_empty() { "" } else { "\n\n" };
                let reserved = separator.len().saturating_add(note.len());
                let max_prefix = crate::session_v2::MAX_FINAL_TEXT_BYTES.saturating_sub(reserved);
                if final_text.len() > max_prefix {
                    let mut boundary = max_prefix;
                    while boundary > 0 && !final_text.is_char_boundary(boundary) {
                        boundary -= 1;
                    }
                    final_text.truncate(boundary);
                }
                final_text.push_str(separator);
                final_text.push_str(note);
            }
            if final_text.len() > crate::session_v2::MAX_FINAL_TEXT_BYTES {
                return Err(AgentError::Provider("final_text_too_large".into()));
            }
            let digest = ring::digest::digest(&ring::digest::SHA256, final_text.as_bytes());
            let finalized_text_sha256 = digest
                .as_ref()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            Some(crate::TurnCompletionV2 {
                final_text,
                assistant_sources,
                progressive_finalization: (!final_activities.is_empty()).then_some(
                    ProgressiveFinalizationV1 {
                        finalization_version: 1,
                        activities: final_activities,
                        coverage_note: coverage_reason
                            .map(|reason| CoverageNoteV1 { version: 1, reason }),
                        finalized_text_sha256,
                    },
                ),
            })
        } else {
            None
        };

        Ok(crate::TurnExitOutputV1 {
            exit_state,
            completion,
            terminal_event_delivery,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::FakeProvider;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;

    /// In-memory archive for testing the executor logic without a store.
    struct FakeArchive {
        account: String,
        items: Vec<(ItemRef, Option<Vec<u8>>)>,
    }
    impl FakeArchive {
        fn item(
            service: &str,
            id: &str,
            name: &str,
            body: Option<&str>,
        ) -> (ItemRef, Option<Vec<u8>>) {
            (
                ItemRef {
                    service: service.into(),
                    id: id.into(),
                    name: name.into(),
                    item_type: "message".into(),
                    path: Some(format!("{service}/{id}.bin")),
                },
                body.map(|b| b.as_bytes().to_vec()),
            )
        }
    }
    impl ArchiveSource for FakeArchive {
        fn account(&self) -> &str {
            &self.account
        }

        fn search_names(&self, query: &str) -> Result<Vec<ItemRef>, AgentError> {
            let q = query.to_lowercase();
            Ok(self
                .items
                .iter()
                .filter(|(i, _)| i.name.to_lowercase().contains(&q))
                .map(|(i, _)| i.clone())
                .collect())
        }
        fn search_bodies(&self, query: &str) -> Result<Vec<(String, String)>, AgentError> {
            let q = query.to_lowercase();
            Ok(self
                .items
                .iter()
                .filter(|(_, b)| {
                    b.as_ref()
                        .map(|b| String::from_utf8_lossy(b).to_lowercase().contains(&q))
                        .unwrap_or(false)
                })
                .map(|(i, _)| (i.service.clone(), i.id.clone()))
                .collect())
        }
        fn get(&self, service: &str, id: &str) -> Result<Option<ItemRef>, AgentError> {
            Ok(self
                .items
                .iter()
                .find(|(i, _)| i.service == service && i.id == id)
                .map(|(i, _)| i.clone()))
        }
        fn read_body(&self, service: &str, id: &str) -> Result<Vec<u8>, AgentError> {
            self.items
                .iter()
                .find(|(i, _)| i.service == service && i.id == id)
                .and_then(|(_, b)| b.clone())
                .ok_or_else(|| AgentError::ToolArgs(format!("no body {service}/{id}")))
        }
        fn list_page(
            &self,
            service: &str,
            limit: u32,
            offset: u32,
        ) -> Result<Vec<ItemRef>, AgentError> {
            Ok(self
                .items
                .iter()
                .filter(|(i, _)| i.service == service)
                .skip(offset as usize)
                .take(limit as usize)
                .map(|(i, _)| i.clone())
                .collect())
        }
        fn roots(&self, service: &str) -> Result<Vec<ItemRef>, AgentError> {
            Ok(self
                .items
                .iter()
                .filter(|(i, _)| i.service == service)
                .map(|(i, _)| i.clone())
                .collect())
        }
        fn children(&self, service: &str, parent: &str) -> Result<Vec<ItemRef>, AgentError> {
            let _ = parent;
            self.roots(service)
        }
        fn count(&self, service: &str) -> Result<u64, AgentError> {
            Ok(self
                .items
                .iter()
                .filter(|(i, _)| i.service == service)
                .count() as u64)
        }
    }

    #[derive(Clone)]
    struct ProgressiveFakeArchive {
        account: String,
        items: Arc<Vec<(ArchiveItemPrivateV1, Vec<u8>)>>,
        body_reads: Arc<AtomicUsize>,
    }

    struct SteppingProgressiveClock {
        start: Instant,
        step_ms: u64,
        ticks: AtomicU64,
    }

    impl SteppingProgressiveClock {
        fn new(step_ms: u64) -> Self {
            Self {
                start: Instant::now(),
                step_ms,
                ticks: AtomicU64::new(0),
            }
        }
    }

    impl ProgressiveClock for SteppingProgressiveClock {
        fn now(&self) -> Instant {
            let tick = self.ticks.fetch_add(1, Ordering::SeqCst);
            self.start + Duration::from_millis(tick.saturating_mul(self.step_ms))
        }
    }

    #[derive(Clone)]
    struct CancelDuringBodyArchive {
        inner: ProgressiveFakeArchive,
        cancellation: crate::CancellationToken,
    }

    struct ProgressiveFakeSnapshot {
        items: Arc<Vec<(ArchiveItemPrivateV1, Vec<u8>)>>,
        services: Vec<String>,
    }

    impl ProgressiveFakeSnapshot {
        fn scoped(&self) -> Vec<ArchiveItemPrivateV1> {
            let mut items = self
                .items
                .iter()
                .filter(|(item, _)| self.services.contains(&item.service))
                .map(|(item, _)| item.clone())
                .collect::<Vec<_>>();
            items.sort_by(|left, right| {
                (&left.service, &left.item_id).cmp(&(&right.service, &right.item_id))
            });
            items
        }
    }

    impl ArchiveSearchSnapshot for ProgressiveFakeSnapshot {
        fn search_names_page(
            &self,
            query: &str,
            limit: u32,
            offset: u32,
        ) -> Result<crate::archive::SearchPage<ArchiveItemPrivateV1>, AgentError> {
            let query = query.to_lowercase();
            let matches = self
                .scoped()
                .into_iter()
                .filter(|item| item.name.to_lowercase().contains(&query))
                .collect::<Vec<_>>();
            Ok(fake_page(matches, limit, offset))
        }

        fn search_bodies_page(
            &self,
            query: &str,
            limit: u32,
            offset: u32,
        ) -> Result<crate::archive::SearchPage<BodyFtsHit>, AgentError> {
            let query = query.to_lowercase();
            let mut matches = self
                .items
                .iter()
                .filter(|(item, body)| {
                    self.services.contains(&item.service)
                        && String::from_utf8_lossy(body)
                            .to_lowercase()
                            .contains(&query)
                })
                .map(|(item, _)| BodyFtsHit {
                    item: item.clone(),
                    snippet: format!("matched {query}"),
                })
                .collect::<Vec<_>>();
            matches.sort_by(|left, right| {
                (&left.item.service, &left.item.item_id)
                    .cmp(&(&right.item.service, &right.item.item_id))
            });
            Ok(fake_page(matches, limit, offset))
        }

        fn metadata_page(
            &self,
            limit: u32,
            offset: u32,
        ) -> Result<crate::archive::SearchPage<ArchiveItemPrivateV1>, AgentError> {
            Ok(fake_page(self.scoped(), limit, offset))
        }
    }

    fn fake_page<T>(items: Vec<T>, limit: u32, offset: u32) -> crate::archive::SearchPage<T> {
        let start = offset as usize;
        let limit = limit as usize;
        let has_more = items.len() > start.saturating_add(limit);
        crate::archive::SearchPage {
            items: items.into_iter().skip(start).take(limit).collect(),
            has_more,
        }
    }

    impl ArchiveSource for ProgressiveFakeArchive {
        fn account(&self) -> &str {
            &self.account
        }

        fn search_names(&self, _query: &str) -> Result<Vec<ItemRef>, AgentError> {
            unreachable!("progressive path must use its bound snapshot")
        }

        fn search_bodies(&self, _query: &str) -> Result<Vec<(String, String)>, AgentError> {
            unreachable!("progressive path must use its bound snapshot")
        }

        fn get(&self, _service: &str, _id: &str) -> Result<Option<ItemRef>, AgentError> {
            unreachable!("progressive path must use its bound snapshot")
        }

        fn read_body(&self, _service: &str, _id: &str) -> Result<Vec<u8>, AgentError> {
            panic!("progressive path must use private verified-handle reads")
        }

        fn list_page(
            &self,
            _service: &str,
            _limit: u32,
            _offset: u32,
        ) -> Result<Vec<ItemRef>, AgentError> {
            unreachable!("progressive path must use its bound snapshot")
        }

        fn roots(&self, _service: &str) -> Result<Vec<ItemRef>, AgentError> {
            unreachable!()
        }

        fn children(&self, _service: &str, _parent: &str) -> Result<Vec<ItemRef>, AgentError> {
            unreachable!()
        }

        fn count(&self, _service: &str) -> Result<u64, AgentError> {
            unreachable!()
        }

        fn begin_search_snapshot(
            &self,
            scope: &NormalizedSearchScope,
            _deadline: &StoreSearchDeadline,
        ) -> Result<Box<dyn ArchiveSearchSnapshot>, AgentError> {
            assert_eq!(scope.account(), self.account);
            Ok(Box::new(ProgressiveFakeSnapshot {
                items: Arc::clone(&self.items),
                services: scope.services().to_vec(),
            }))
        }

        fn read_private_body(
            &self,
            locator: &crate::archive::ValidatedArchiveRelativePath,
            _deadline: &StoreSearchDeadline,
        ) -> Result<Vec<u8>, AgentError> {
            self.body_reads.fetch_add(1, Ordering::SeqCst);
            if locator.as_str().contains("unreadable") {
                return Err(AgentError::Provider(format!(
                    "raw filesystem failure at /private/archive/{}",
                    locator.as_str()
                )));
            }
            self.items
                .iter()
                .find(|(item, _)| {
                    item.body_rel_path
                        .as_ref()
                        .is_some_and(|path| path.as_str() == locator.as_str())
                })
                .map(|(_, body)| body.clone())
                .ok_or_else(|| AgentError::Provider("archive_body_unavailable".into()))
        }
    }

    impl ArchiveSource for CancelDuringBodyArchive {
        fn account(&self) -> &str {
            self.inner.account()
        }

        fn search_names(&self, query: &str) -> Result<Vec<ItemRef>, AgentError> {
            self.inner.search_names(query)
        }

        fn search_bodies(&self, query: &str) -> Result<Vec<(String, String)>, AgentError> {
            self.inner.search_bodies(query)
        }

        fn get(&self, service: &str, id: &str) -> Result<Option<ItemRef>, AgentError> {
            self.inner.get(service, id)
        }

        fn read_body(&self, service: &str, id: &str) -> Result<Vec<u8>, AgentError> {
            self.inner.read_body(service, id)
        }

        fn list_page(
            &self,
            service: &str,
            limit: u32,
            offset: u32,
        ) -> Result<Vec<ItemRef>, AgentError> {
            self.inner.list_page(service, limit, offset)
        }

        fn roots(&self, service: &str) -> Result<Vec<ItemRef>, AgentError> {
            self.inner.roots(service)
        }

        fn children(&self, service: &str, parent: &str) -> Result<Vec<ItemRef>, AgentError> {
            self.inner.children(service, parent)
        }

        fn count(&self, service: &str) -> Result<u64, AgentError> {
            self.inner.count(service)
        }

        fn begin_search_snapshot(
            &self,
            scope: &NormalizedSearchScope,
            deadline: &StoreSearchDeadline,
        ) -> Result<Box<dyn ArchiveSearchSnapshot>, AgentError> {
            self.inner.begin_search_snapshot(scope, deadline)
        }

        fn read_private_body(
            &self,
            _locator: &crate::archive::ValidatedArchiveRelativePath,
            _deadline: &StoreSearchDeadline,
        ) -> Result<Vec<u8>, AgentError> {
            self.inner.body_reads.fetch_add(1, Ordering::SeqCst);
            self.cancellation.cancel();
            Err(AgentError::Provider("archive_body_interrupted".into()))
        }
    }

    fn progressive_item(id: &str, name: &str, body: &str) -> (ArchiveItemPrivateV1, Vec<u8>) {
        (
            ArchiveItemPrivateV1 {
                service: "mail".into(),
                item_id: id.into(),
                name: name.into(),
                item_type: "message".into(),
                sender: None,
                remote_mtime: None,
                size: Some(body.len() as u64),
                body_rel_path: Some(
                    crate::archive::ValidatedArchiveRelativePath::parse(format!("mail/{id}.eml"))
                        .unwrap(),
                ),
                display_path: None,
            },
            body.as_bytes().to_vec(),
        )
    }

    #[test]
    fn progressive_search_reads_no_body_until_verified_model_selection() {
        let body_reads = Arc::new(AtomicUsize::new(0));
        let executor = RetrievalExecutor::new(ProgressiveFakeArchive {
            account: "account".into(),
            items: Arc::new(vec![
                progressive_item("keyword", "Invoice 2026", "known keyword body"),
                progressive_item("candidate", "Music receipt", "DistroKid annual charge"),
            ]),
            body_reads: Arc::clone(&body_reads),
        });
        let authority = crate::HmacProgressiveSearchAuthority::new([7; 32]);
        let cancellation = crate::CancellationToken::default();
        let mut events = Vec::new();
        let mut emit = |event| events.push(event);
        let mut sink = crate::InfallibleTurnEventSink::new(&mut emit);
        let binding = crate::ReadExecutionBindingV2 {
            session_id: "session".into(),
            request_id: "request".into(),
            tool_use_id: "search-A".into(),
            resolved_account_key: "account".into(),
            admission_account_digest: crate::admission_account_digest("account").unwrap(),
        };
        let mut budget = crate::ProviderInputBudgetV1::new(None, 1_000_000, 0);
        let search = executor
            .execute_read_with_context(
                &ToolAction::Search {
                    account: "account".into(),
                    services: vec!["mail".into()],
                    query: "invoice".into(),
                    limit: Some(20),
                },
                crate::ReadExecutionContext {
                    binding: &binding,
                    local_effect: None,
                    mode: crate::ReadExecutionMode::Live,
                    provider_step_seq: 0,
                    provider_steps_remaining_after_current: 15,
                    input_budget: &mut budget,
                    cancellation: &cancellation,
                    events: &mut sink,
                    progressive_authority: Some(&authority),
                },
            )
            .unwrap();
        assert_eq!(body_reads.load(Ordering::SeqCst), 0);
        let crate::ReadExecutionOutputV2::Search(search) = search else {
            panic!("search output")
        };
        let private: serde_json::Value = serde_json::from_str(&search.provider_content).unwrap();
        let deep = &private["deep_context"];
        let activity_id = deep["activity_id"].as_str().unwrap().to_string();
        let continuation = deep["continuation"].as_str().unwrap().to_string();
        let candidate = deep["candidates"][0]["candidate_key"]
            .as_str()
            .unwrap()
            .to_string();
        let deep_binding = crate::ReadExecutionBindingV2 {
            tool_use_id: "deep-B".into(),
            ..binding
        };
        let deep_output = executor
            .execute_read_with_context(
                &ToolAction::DeepSearch {
                    activity_id,
                    continuation,
                    candidates: vec![candidate],
                },
                crate::ReadExecutionContext {
                    binding: &deep_binding,
                    local_effect: None,
                    mode: crate::ReadExecutionMode::Live,
                    provider_step_seq: 1,
                    provider_steps_remaining_after_current: 14,
                    input_budget: &mut budget,
                    cancellation: &cancellation,
                    events: &mut sink,
                    progressive_authority: Some(&authority),
                },
            )
            .unwrap();
        assert_eq!(body_reads.load(Ordering::SeqCst), 1);
        let crate::ReadExecutionOutputV2::DeepSearch(deep_output) = deep_output else {
            panic!("deep output")
        };
        let initial_plan = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::StageProgress(progress) => Some((progress.stage, progress.status)),
                _ => None,
            })
            .take(4)
            .collect::<Vec<_>>();
        assert_eq!(
            initial_plan,
            vec![
                (SearchStage::Names, StageStatus::Queued),
                (SearchStage::Bodies, StageStatus::Queued),
                (SearchStage::Deep, StageStatus::Queued),
                (SearchStage::Names, StageStatus::Running),
            ]
        );
        let public = serde_json::to_string(&deep_output.public_projection).unwrap();
        assert!(!public.contains("DistroKid"));
        assert!(!public.contains("mail/candidate.eml"));
        let public_stream = events
            .iter()
            .map(StreamEvent::to_public_json_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!public_stream.contains("DistroKid"));
        assert!(!public_stream.contains("mail/candidate.eml"));
        assert!(deep_output.provider_content.contains("DistroKid"));
    }

    #[test]
    fn progressive_public_transport_never_contains_fts_or_deep_body_excerpt() {
        let body_reads = Arc::new(AtomicUsize::new(0));
        let items = Arc::new(vec![
            progressive_item(
                "keyword",
                "Ordinary document",
                "invoice body phrase visible only to the provider",
            ),
            progressive_item(
                "candidate",
                "Music receipt",
                "DistroKid annual charge visible only to the provider",
            ),
        ]);
        let executor = RetrievalExecutor::new(ProgressiveFakeArchive {
            account: "account".into(),
            items,
            body_reads: Arc::clone(&body_reads),
        });
        let authority = crate::HmacProgressiveSearchAuthority::new([41; 32]);
        let cancellation = crate::CancellationToken::default();
        let binding = crate::ReadExecutionBindingV2 {
            session_id: "session".into(),
            request_id: "request-public-boundary".into(),
            tool_use_id: "search-public-boundary".into(),
            resolved_account_key: "account".into(),
            admission_account_digest: crate::admission_account_digest("account").unwrap(),
        };
        let mut budget = crate::ProviderInputBudgetV1::new(None, 1_000_000, 0);
        let mut events = Vec::new();
        let output = {
            let mut collect = |event| events.push(event);
            let mut sink = crate::InfallibleTurnEventSink::new(&mut collect);
            executor
                .execute_read_with_context(
                    &ToolAction::Search {
                        account: "account".into(),
                        services: vec!["mail".into()],
                        query: "invoice".into(),
                        limit: Some(20),
                    },
                    crate::ReadExecutionContext {
                        binding: &binding,
                        local_effect: None,
                        mode: crate::ReadExecutionMode::Live,
                        provider_step_seq: 0,
                        provider_steps_remaining_after_current: 15,
                        input_budget: &mut budget,
                        cancellation: &cancellation,
                        events: &mut sink,
                        progressive_authority: Some(&authority),
                    },
                )
                .unwrap()
        };
        let crate::ReadExecutionOutputV2::Search(output) = output else {
            panic!("search output")
        };

        assert!(output.provider_content.contains("matched invoice"));
        let public = events
            .iter()
            .map(StreamEvent::to_public_json_string)
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in [
            "matched invoice",
            "invoice body phrase",
            "DistroKid annual charge",
            "snippet",
            "excerpt",
        ] {
            assert!(
                !public.contains(forbidden),
                "public stream leaked {forbidden}"
            );
        }
        assert_eq!(body_reads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn cancellation_during_private_body_read_emits_no_late_partial_result() {
        let body_reads = Arc::new(AtomicUsize::new(0));
        let cancellation = crate::CancellationToken::default();
        let executor = RetrievalExecutor::new(CancelDuringBodyArchive {
            inner: ProgressiveFakeArchive {
                account: "account".into(),
                items: Arc::new(vec![
                    progressive_item("keyword", "Invoice 2026", "known invoice body"),
                    progressive_item("candidate", "Music receipt", "DistroKid annual charge"),
                ]),
                body_reads: Arc::clone(&body_reads),
            },
            cancellation: cancellation.clone(),
        });
        let authority = crate::HmacProgressiveSearchAuthority::new([42; 32]);
        let (binding, activity_id, continuation, candidate) =
            open_progressive_candidate(&executor, &authority, &cancellation);
        let mut budget = crate::ProviderInputBudgetV1::new(None, 1_000_000, 0);
        let mut events = Vec::new();
        let error = {
            let mut collect = |event| events.push(event);
            let mut sink = crate::InfallibleTurnEventSink::new(&mut collect);
            executor
                .execute_read_with_context(
                    &ToolAction::DeepSearch {
                        activity_id,
                        continuation,
                        candidates: vec![candidate],
                    },
                    crate::ReadExecutionContext {
                        binding: &binding,
                        local_effect: None,
                        mode: crate::ReadExecutionMode::Live,
                        provider_step_seq: 1,
                        provider_steps_remaining_after_current: 14,
                        input_budget: &mut budget,
                        cancellation: &cancellation,
                        events: &mut sink,
                        progressive_authority: Some(&authority),
                    },
                )
                .unwrap_err()
        };

        assert!(matches!(error, AgentError::Cancelled));
        assert_eq!(body_reads.load(Ordering::SeqCst), 1);
        assert!(!events
            .iter()
            .any(|event| matches!(event, StreamEvent::PartialResult(_))));
    }

    fn execute_large_progressive_fixture(
        step_ms: u64,
    ) -> (crate::SeparatedSearchOutputV2, Vec<StreamEvent>) {
        let mut items = Vec::with_capacity(10_000);
        for index in 0..10_000 {
            let mut item = progressive_item(
                &format!("item-{index:05}"),
                &format!("Synthetic archive item {index:05}"),
                "body without the search term",
            );
            item.0.body_rel_path = None;
            items.push(item);
        }
        let executor = RetrievalExecutor::with_progressive_clock(
            ProgressiveFakeArchive {
                account: "account".into(),
                items: Arc::new(items),
                body_reads: Arc::new(AtomicUsize::new(0)),
            },
            Arc::new(SteppingProgressiveClock::new(step_ms)),
        );
        let authority = crate::HmacProgressiveSearchAuthority::new([43; 32]);
        let cancellation = crate::CancellationToken::default();
        let binding = crate::ReadExecutionBindingV2 {
            session_id: "session".into(),
            request_id: format!("large-fixture-{step_ms}"),
            tool_use_id: format!("large-search-{step_ms}"),
            resolved_account_key: "account".into(),
            admission_account_digest: crate::admission_account_digest("account").unwrap(),
        };
        let mut budget = crate::ProviderInputBudgetV1::new(None, 1_000_000, 0);
        let mut events = Vec::new();
        let output = {
            let mut collect = |event| events.push(event);
            let mut sink = crate::InfallibleTurnEventSink::new(&mut collect);
            executor
                .execute_read_with_context(
                    &ToolAction::Search {
                        account: "account".into(),
                        services: vec!["mail".into()],
                        query: "absent-search-term".into(),
                        limit: Some(20),
                    },
                    crate::ReadExecutionContext {
                        binding: &binding,
                        local_effect: None,
                        mode: crate::ReadExecutionMode::Live,
                        provider_step_seq: 0,
                        provider_steps_remaining_after_current: 15,
                        input_budget: &mut budget,
                        cancellation: &cancellation,
                        events: &mut sink,
                        progressive_authority: Some(&authority),
                    },
                )
                .unwrap()
        };
        let crate::ReadExecutionOutputV2::Search(output) = output else {
            panic!("search output")
        };
        (output, events)
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_large_deep_page(
        executor: &RetrievalExecutor<ProgressiveFakeArchive>,
        authority: &crate::HmacProgressiveSearchAuthority,
        cancellation: &crate::CancellationToken,
        activity_id: &str,
        continuation: &str,
        candidates: Vec<String>,
        provider_step_seq: u8,
        budget: &mut crate::ProviderInputBudgetV1<'_>,
    ) -> crate::SeparatedSearchOutputV2 {
        let binding = crate::ReadExecutionBindingV2 {
            session_id: "session".into(),
            request_id: "large-continuation".into(),
            tool_use_id: format!("large-deep-{provider_step_seq}"),
            resolved_account_key: "account".into(),
            admission_account_digest: crate::admission_account_digest("account").unwrap(),
        };
        let mut events = Vec::new();
        let mut collect = |event| events.push(event);
        let mut sink = crate::InfallibleTurnEventSink::new(&mut collect);
        let output = executor
            .execute_read_with_context(
                &ToolAction::DeepSearch {
                    activity_id: activity_id.into(),
                    continuation: continuation.into(),
                    candidates,
                },
                crate::ReadExecutionContext {
                    binding: &binding,
                    local_effect: None,
                    mode: crate::ReadExecutionMode::Live,
                    provider_step_seq,
                    provider_steps_remaining_after_current: 15 - provider_step_seq,
                    input_budget: budget,
                    cancellation,
                    events: &mut sink,
                    progressive_authority: Some(authority),
                },
            )
            .unwrap();
        let crate::ReadExecutionOutputV2::DeepSearch(output) = output else {
            panic!("deep-search output")
        };
        output
    }

    #[test]
    fn large_fixture_enforces_record_cap_and_coalesces_current_progress() {
        let (output, events) = execute_large_progressive_fixture(0);
        assert!(!output.public_projection.coverage_complete);
        assert!(!output.public_projection.budget_reached);
        assert!(output.public_projection.continuation_available);

        let running = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::StageProgress(progress)
                    if progress.stage == SearchStage::Deep
                        && progress.status == StageStatus::Running
                        && progress.current_item.is_some()
                        && progress.continuation_available.is_none() =>
                {
                    Some(progress.scanned)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(running.first().copied(), Some(25));
        assert_eq!(
            running.last().copied(),
            Some(MAX_METADATA_SCANNED_PER_CANDIDATE_PAGE)
        );
        assert_eq!(
            running.len(),
            usize::try_from(
                MAX_METADATA_SCANNED_PER_CANDIDATE_PAGE / METADATA_PROGRESS_RECORD_INTERVAL
            )
            .unwrap()
        );
        assert!(running
            .windows(2)
            .all(|pair| { pair[1].saturating_sub(pair[0]) == METADATA_PROGRESS_RECORD_INTERVAL }));
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::StageProgress(progress)
                if progress.stage == SearchStage::Deep
                    && progress.budget_reached == Some(false)
                    && progress.coverage_complete == Some(false)
                    && progress.continuation_available == Some(true)
        )));
    }

    #[test]
    fn large_fixture_continuation_reaches_candidate_after_first_thousand_records() {
        let mut items = Vec::with_capacity(10_000);
        for index in 0..10_000 {
            let mut item = progressive_item(
                &format!("item-{index:05}"),
                &format!("Synthetic archive item {index:05}"),
                "body without the search term",
            );
            if index != 1_200 {
                item.0.body_rel_path = None;
            }
            items.push(item);
        }
        let body_reads = Arc::new(AtomicUsize::new(0));
        let executor = RetrievalExecutor::new(ProgressiveFakeArchive {
            account: "account".into(),
            items: Arc::new(items),
            body_reads: Arc::clone(&body_reads),
        });
        let authority = crate::HmacProgressiveSearchAuthority::new([44; 32]);
        let cancellation = crate::CancellationToken::default();
        let search_binding = crate::ReadExecutionBindingV2 {
            session_id: "session".into(),
            request_id: "large-continuation".into(),
            tool_use_id: "large-search".into(),
            resolved_account_key: "account".into(),
            admission_account_digest: crate::admission_account_digest("account").unwrap(),
        };
        let mut budget = crate::ProviderInputBudgetV1::new(None, 1_000_000, 0);
        let mut events = Vec::new();
        let search = {
            let mut collect = |event| events.push(event);
            let mut sink = crate::InfallibleTurnEventSink::new(&mut collect);
            executor
                .execute_read_with_context(
                    &ToolAction::Search {
                        account: "account".into(),
                        services: vec!["mail".into()],
                        query: "absent-search-term".into(),
                        limit: Some(20),
                    },
                    crate::ReadExecutionContext {
                        binding: &search_binding,
                        local_effect: None,
                        mode: crate::ReadExecutionMode::Live,
                        provider_step_seq: 0,
                        provider_steps_remaining_after_current: 15,
                        input_budget: &mut budget,
                        cancellation: &cancellation,
                        events: &mut sink,
                        progressive_authority: Some(&authority),
                    },
                )
                .unwrap()
        };
        let crate::ReadExecutionOutputV2::Search(search) = search else {
            panic!("search output")
        };
        let mut private: serde_json::Value =
            serde_json::from_str(&search.provider_content).unwrap();
        let mut deep = private["deep_context"].take();
        assert_eq!(deep["scanned"], 500);
        assert!(deep["candidates"].as_array().unwrap().is_empty());
        let activity_id = deep["activity_id"].as_str().unwrap().to_string();

        for (provider_step, expected_scanned) in [(1, 1_000), (2, 1_500)] {
            let continuation = deep["continuation"].as_str().unwrap().to_string();
            let output = execute_large_deep_page(
                &executor,
                &authority,
                &cancellation,
                &activity_id,
                &continuation,
                Vec::new(),
                provider_step,
                &mut budget,
            );
            private = serde_json::from_str(&output.provider_content).unwrap();
            deep = private["deep_context"].take();
            assert_eq!(deep["scanned"], expected_scanned);
            assert!(!output.public_projection.budget_reached);
            assert!(output.public_projection.continuation_available);
        }

        let candidate = deep["candidates"][0]["candidate_key"]
            .as_str()
            .unwrap()
            .to_string();
        let continuation = deep["continuation"].as_str().unwrap().to_string();
        let selected = execute_large_deep_page(
            &executor,
            &authority,
            &cancellation,
            &activity_id,
            &continuation,
            vec![candidate],
            3,
            &mut budget,
        );
        assert_eq!(body_reads.load(Ordering::SeqCst), 1);
        assert_eq!(selected.assistant_sources.len(), 1);
        assert!(selected.provider_content.contains("item-01200"));
    }

    #[test]
    fn candidate_page_byte_boundary_never_rolls_back_published_scan_counter() {
        let mut items = Vec::new();
        for index in 0..100 {
            let mut item = progressive_item(
                &format!("candidate-{index:03}"),
                &format!("{index:03}-{}", "candidate metadata ".repeat(40)),
                "bounded body",
            );
            item.0.sender = Some("bounded sender ".repeat(24));
            items.push(item);
        }
        let executor = RetrievalExecutor::new(ProgressiveFakeArchive {
            account: "account".into(),
            items: Arc::new(items),
            body_reads: Arc::new(AtomicUsize::new(0)),
        });
        let scope = CanonicalSearchScopeV1::new(
            "account",
            "absent-search-term",
            vec!["mail".into()],
            Some(20),
        )
        .unwrap();
        let binding = SearchActivityBindingV1 {
            session_id: "session".into(),
            request_id: "candidate-page-byte-boundary".into(),
            activity_id: "AAAAAAAAAAAAAAAAAAAAAA".into(),
            originating_search_tool_use_id: "search-tool".into(),
            canonical_scope_digest: scope.digest(),
        };
        let normalized_scope = NormalizedSearchScope::new("account", vec!["mail".into()]).unwrap();
        let cancellation = crate::CancellationToken::default();
        let timing = ProgressiveCallTiming::new(Arc::clone(&executor.clock), &cancellation);
        let snapshot = executor
            .source
            .begin_search_snapshot(&normalized_scope, &timing.store_deadline())
            .unwrap();
        let authority = crate::HmacProgressiveSearchAuthority::new([45; 32]);
        let mut published = Vec::new();
        let page = executor
            .candidate_page(
                snapshot.as_ref(),
                &authority,
                &binding,
                &BTreeSet::new(),
                0,
                0,
                0,
                0,
                0,
                &timing,
                &mut |scanned, _| {
                    published.push(scanned);
                    Ok(())
                },
            )
            .unwrap();

        assert!(page.has_more);
        assert!(page.provider_candidates.len() < MAX_CANDIDATES_PER_PAGE);
        assert_eq!(published.last().copied(), Some(page.state.metadata_scanned));
        assert_eq!(page.next_offset, page.state.metadata_scanned);
        assert!(published.windows(2).all(|pair| pair[1] == pair[0] + 1));
    }

    #[test]
    fn large_fixture_injected_two_second_metadata_deadline_stops_before_record_cap() {
        let (output, events) = execute_large_progressive_fixture(2);
        assert!(!output.public_projection.coverage_complete);
        assert!(output.public_projection.budget_reached);
        let terminal_scanned = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::StageProgress(progress)
                    if progress.stage == SearchStage::Deep
                        && progress.budget_reached == Some(true) =>
                {
                    Some(progress.scanned)
                }
                _ => None,
            })
            .next_back()
            .expect("deep budget terminal");
        assert!(terminal_scanned > 0);
        assert!(terminal_scanned < MAX_METADATA_SCANNED_PER_CANDIDATE_PAGE);
    }

    struct ProgressiveTurnObserver {
        authority: Arc<crate::HmacProgressiveSearchAuthority>,
    }

    impl crate::TurnObserver for ProgressiveTurnObserver {
        fn read_execution_binding(&self, tool_use_id: &str) -> Option<crate::ReadExecutionBinding> {
            Some(crate::ReadExecutionBindingV2 {
                session_id: "session".into(),
                request_id: "request".into(),
                tool_use_id: tool_use_id.into(),
                resolved_account_key: "account".into(),
                admission_account_digest: crate::admission_account_digest("account").unwrap(),
            })
        }

        fn progressive_authority(&self) -> Option<Arc<dyn crate::ProgressiveSearchAuthority>> {
            Some(self.authority.clone())
        }

        fn provider_input_limit(&self) -> usize {
            1_000_000
        }
    }

    #[test]
    fn fake_provider_turn_selects_keywordless_candidate_after_store_archive_search() {
        let items = Arc::new(vec![
            progressive_item(
                "keyword",
                "Ordinary document",
                "invoice body phrase visible only to the provider",
            ),
            progressive_item(
                "candidate",
                "Music receipt",
                "DistroKid annual charge visible only to the provider",
            ),
        ]);
        let authority = Arc::new(crate::HmacProgressiveSearchAuthority::new([44; 32]));
        let preflight = RetrievalExecutor::new(ProgressiveFakeArchive {
            account: "account".into(),
            items: Arc::clone(&items),
            body_reads: Arc::new(AtomicUsize::new(0)),
        });
        let cancellation = crate::CancellationToken::default();
        let (_, activity_id, continuation, candidate) =
            open_progressive_candidate(&preflight, authority.as_ref(), &cancellation);
        let private_continuation = continuation.clone();

        let body_reads = Arc::new(AtomicUsize::new(0));
        let executor = RetrievalExecutor::new(ProgressiveFakeArchive {
            account: "account".into(),
            items,
            body_reads: Arc::clone(&body_reads),
        });
        let mut provider = FakeProvider::new(vec![
            vec![crate::AssistantBlock::ToolUse {
                id: "search-A".into(),
                input: serde_json::json!({
                    "op": "search",
                    "account": "account",
                    "services": ["mail"],
                    "query": "invoice",
                    "limit": 20,
                }),
            }],
            vec![crate::AssistantBlock::ToolUse {
                id: "deep-B".into(),
                input: serde_json::json!({
                    "op": "deep-search",
                    "activity_id": activity_id,
                    "continuation": continuation,
                    "candidates": [candidate],
                }),
            }],
            vec![crate::AssistantBlock::Text(
                "The selected archive item contains the requested charge.".into(),
            )],
        ]);
        let mut observer = ProgressiveTurnObserver {
            authority: Arc::clone(&authority),
        };
        let mut history = vec![crate::Message::user("Find the relevant charge")];
        let mut events = Vec::new();
        let outcome = crate::run_turn_observed(
            &mut provider,
            &executor,
            &mut history,
            &mut |event| events.push(event),
            &mut observer,
        )
        .unwrap();

        assert!(matches!(outcome, crate::TurnOutcome::Final { .. }));
        assert_eq!(body_reads.load(Ordering::SeqCst), 1);
        let private_tool_results = history
            .iter()
            .filter(|message| message.role == crate::Role::Tool)
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(private_tool_results.contains("matched invoice"));
        assert!(private_tool_results.contains("DistroKid annual charge"));
        let public = events
            .iter()
            .map(StreamEvent::to_public_json_string)
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in [
            "invoice body phrase",
            "matched invoice",
            "DistroKid annual charge",
            private_continuation.as_str(),
        ] {
            assert!(
                !public.contains(forbidden),
                "public stream leaked {forbidden}"
            );
        }
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::PartialResult(result)
                if result.stage == SearchStage::Deep
                    && result.items.iter().any(|item| item.name == "Music receipt")
        )));
    }

    #[test]
    fn deep_unreadable_body_exposes_no_path_or_raw_error() {
        let body_reads = Arc::new(AtomicUsize::new(0));
        let executor = RetrievalExecutor::new(ProgressiveFakeArchive {
            account: "account".into(),
            items: Arc::new(vec![
                progressive_item("keyword", "Invoice 2026", "known keyword body"),
                progressive_item(
                    "unreadable",
                    "Unreadable candidate",
                    "private body must not escape",
                ),
            ]),
            body_reads: Arc::clone(&body_reads),
        });
        let authority = crate::HmacProgressiveSearchAuthority::new([13; 32]);
        let cancellation = crate::CancellationToken::default();
        let (binding, activity_id, continuation, candidate) =
            open_progressive_candidate(&executor, &authority, &cancellation);
        let mut events = Vec::new();
        let mut collect = |event| events.push(event);
        let mut sink = crate::InfallibleTurnEventSink::new(&mut collect);
        let mut budget = crate::ProviderInputBudgetV1::new(None, 1_000_000, 0);

        let output = executor
            .execute_read_with_context(
                &ToolAction::DeepSearch {
                    activity_id,
                    continuation,
                    candidates: vec![candidate],
                },
                crate::ReadExecutionContext {
                    binding: &binding,
                    local_effect: None,
                    mode: crate::ReadExecutionMode::Live,
                    provider_step_seq: 1,
                    provider_steps_remaining_after_current: 14,
                    input_budget: &mut budget,
                    cancellation: &cancellation,
                    events: &mut sink,
                    progressive_authority: Some(&authority),
                },
            )
            .unwrap();

        let crate::ReadExecutionOutputV2::DeepSearch(output) = output else {
            panic!("deep-search output")
        };
        let provider = output.provider_content;
        let public = events
            .iter()
            .map(StreamEvent::to_public_json_string)
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in [
            "/private/archive",
            "mail/unreadable.eml",
            "raw filesystem failure",
            "private body must not escape",
        ] {
            assert!(!provider.contains(forbidden), "provider leaked {forbidden}");
            assert!(
                !public.contains(forbidden),
                "public stream leaked {forbidden}"
            );
        }
        assert!(provider.contains("\"body_available\":false"));
        assert_eq!(body_reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn progressive_search_never_emits_done_from_retrieval() {
        let (_output, events) = finish_progressive_with_statuses(
            TurnExitKind::Final,
            StageStatus::Complete,
            StageStatus::Complete,
            StageStatus::Running,
        );

        assert!(
            events
                .iter()
                .any(|event| matches!(event, StreamEvent::StageProgress(_))),
            "retrieval finalization must emit its stage terminal"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::Done { .. })),
            "host persistence owns done events"
        );
    }

    #[test]
    fn progress_event_content_is_absent_from_logs_and_errors() {
        let private_marker = "private-item-and-path-sentinel";
        let event = StreamEvent::StageProgress(StageProgressV1 {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            activity_id: "abcdefghijklmnopqrstuv".into(),
            activity_kind: ActivityKind::ArchiveSearch,
            stage: SearchStage::Deep,
            status: StageStatus::Running,
            scanned: 1,
            total: None,
            hits: 0,
            current_item: Some(private_marker.into()),
            coverage_complete: None,
            budget_reached: None,
            continuation_available: None,
        });

        let debug = format!("{event:?}");
        assert!(debug.contains("stage_progress"));
        assert!(!debug.contains(private_marker));

        let mut sink = RejectingSink;
        let error = sink.emit(event).unwrap_err().to_string();
        assert_eq!(error, "turn stream unavailable");
        assert!(!error.contains(private_marker));
    }

    fn open_progressive_candidate<A: ArchiveSource>(
        executor: &RetrievalExecutor<A>,
        authority: &crate::HmacProgressiveSearchAuthority,
        cancellation: &crate::CancellationToken,
    ) -> (crate::ReadExecutionBindingV2, String, String, String) {
        let binding = crate::ReadExecutionBindingV2 {
            session_id: "session".into(),
            request_id: "request".into(),
            tool_use_id: "search-A".into(),
            resolved_account_key: "account".into(),
            admission_account_digest: crate::admission_account_digest("account").unwrap(),
        };
        let mut budget = crate::ProviderInputBudgetV1::new(None, 1_000_000, 0);
        let mut events = Vec::new();
        let mut collect = |event| events.push(event);
        let mut sink = crate::InfallibleTurnEventSink::new(&mut collect);
        let output = executor
            .execute_read_with_context(
                &ToolAction::Search {
                    account: "account".into(),
                    services: vec!["mail".into()],
                    query: "invoice".into(),
                    limit: Some(20),
                },
                crate::ReadExecutionContext {
                    binding: &binding,
                    local_effect: None,
                    mode: crate::ReadExecutionMode::Live,
                    provider_step_seq: 0,
                    provider_steps_remaining_after_current: 15,
                    input_budget: &mut budget,
                    cancellation,
                    events: &mut sink,
                    progressive_authority: Some(authority),
                },
            )
            .unwrap();
        let crate::ReadExecutionOutputV2::Search(output) = output else {
            panic!("search output")
        };
        let private: serde_json::Value = serde_json::from_str(&output.provider_content).unwrap();
        let deep = &private["deep_context"];
        (
            crate::ReadExecutionBindingV2 {
                tool_use_id: "deep-B".into(),
                ..binding
            },
            deep["activity_id"].as_str().unwrap().to_string(),
            deep["continuation"].as_str().unwrap().to_string(),
            deep["candidates"][0]["candidate_key"]
                .as_str()
                .unwrap()
                .to_string(),
        )
    }

    #[test]
    fn progressive_provider_budget_exhaustion_stops_before_read_and_omits_continuation() {
        let body_reads = Arc::new(AtomicUsize::new(0));
        let executor = RetrievalExecutor::new(ProgressiveFakeArchive {
            account: "account".into(),
            items: Arc::new(vec![
                progressive_item("keyword", "Invoice 2026", "known keyword body"),
                progressive_item("candidate", "Music receipt", "DistroKid annual charge"),
            ]),
            body_reads: Arc::clone(&body_reads),
        });
        let authority = crate::HmacProgressiveSearchAuthority::new([17; 32]);
        let cancellation = crate::CancellationToken::default();
        let (binding, activity_id, continuation, candidate) =
            open_progressive_candidate(&executor, &authority, &cancellation);
        let mut budget = crate::ProviderInputBudgetV1::new(None, MAX_DEEP_PROVIDER_BYTES - 1, 0);
        let mut events = Vec::new();
        let mut collect = |event| events.push(event);
        let mut sink = crate::InfallibleTurnEventSink::new(&mut collect);

        let error = executor
            .execute_read_with_context(
                &ToolAction::DeepSearch {
                    activity_id,
                    continuation,
                    candidates: vec![candidate],
                },
                crate::ReadExecutionContext {
                    binding: &binding,
                    local_effect: None,
                    mode: crate::ReadExecutionMode::Live,
                    provider_step_seq: 1,
                    provider_steps_remaining_after_current: 14,
                    input_budget: &mut budget,
                    cancellation: &cancellation,
                    events: &mut sink,
                    progressive_authority: Some(&authority),
                },
            )
            .unwrap_err();

        assert!(matches!(
            error,
            AgentError::Provider(code) if code == "provider_input_budget_exhausted"
        ));
        assert_eq!(body_reads.load(Ordering::SeqCst), 0);
        assert!(!events.iter().any(|event| {
            matches!(
                event,
                StreamEvent::StageProgress(progress)
                    if progress.continuation_available == Some(true)
            )
        }));
    }

    #[test]
    fn initial_search_omits_deep_context_when_model_budget_cannot_fit_candidates() {
        let body_reads = Arc::new(AtomicUsize::new(0));
        let mut items = vec![progressive_item(
            "keyword",
            "Invoice 2026",
            "known invoice body",
        )];
        for index in 0..32 {
            items.push(progressive_item(
                &format!("candidate-{index:02}"),
                &format!("Semantically related candidate {index:02}"),
                "candidate body",
            ));
        }
        let executor = RetrievalExecutor::new(ProgressiveFakeArchive {
            account: "account".into(),
            items: Arc::new(items),
            body_reads: Arc::clone(&body_reads),
        });
        let authority = crate::HmacProgressiveSearchAuthority::new([27; 32]);
        let cancellation = crate::CancellationToken::default();
        let binding = crate::ReadExecutionBindingV2 {
            session_id: "session".into(),
            request_id: "request".into(),
            tool_use_id: "search-budget".into(),
            resolved_account_key: "account".into(),
            admission_account_digest: crate::admission_account_digest("account").unwrap(),
        };
        let mut budget = crate::ProviderInputBudgetV1::new(None, 4_096, 0);
        let mut events = Vec::new();
        let mut collect = |event| events.push(event);
        let mut sink = crate::InfallibleTurnEventSink::new(&mut collect);

        let output = executor
            .execute_read_with_context(
                &ToolAction::Search {
                    account: "account".into(),
                    services: vec!["mail".into()],
                    query: "invoice".into(),
                    limit: Some(20),
                },
                crate::ReadExecutionContext {
                    binding: &binding,
                    local_effect: None,
                    mode: crate::ReadExecutionMode::Live,
                    provider_step_seq: 0,
                    provider_steps_remaining_after_current: 15,
                    input_budget: &mut budget,
                    cancellation: &cancellation,
                    events: &mut sink,
                    progressive_authority: Some(&authority),
                },
            )
            .unwrap();
        let crate::ReadExecutionOutputV2::Search(output) = output else {
            panic!("search output")
        };
        let provider: serde_json::Value = serde_json::from_str(&output.provider_content).unwrap();

        assert!(provider["deep_context"].is_null());
        assert!(output.public_projection.budget_reached);
        assert!(!output.public_projection.continuation_available);
        assert!(output.provider_content.len() <= 4_096);
        assert_eq!(body_reads.load(Ordering::SeqCst), 0);
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::StageProgress(progress)
                if progress.stage == SearchStage::Deep
                    && progress.status == StageStatus::Complete
                    && progress.budget_reached == Some(true)
                    && progress.continuation_available == Some(false)
        )));
    }

    #[test]
    fn initial_search_and_deep_provider_content_enforce_independent_byte_caps() {
        let body_reads = Arc::new(AtomicUsize::new(0));
        let executor = RetrievalExecutor::new(ProgressiveFakeArchive {
            account: "account".into(),
            items: Arc::new(vec![
                progressive_item("keyword", "Invoice 2026", "known keyword body"),
                progressive_item("candidate", "Music receipt", "DistroKid annual charge"),
            ]),
            body_reads: Arc::clone(&body_reads),
        });
        let authority = crate::HmacProgressiveSearchAuthority::new([18; 32]);
        let cancellation = crate::CancellationToken::default();
        let (binding, activity_id, continuation, candidate) =
            open_progressive_candidate(&executor, &authority, &cancellation);
        let mut budget = crate::ProviderInputBudgetV1::new(None, MAX_DEEP_PROVIDER_BYTES, 0);
        let mut events = Vec::new();
        let mut collect = |event| events.push(event);
        let mut sink = crate::InfallibleTurnEventSink::new(&mut collect);

        let output = executor
            .execute_read_with_context(
                &ToolAction::DeepSearch {
                    activity_id,
                    continuation,
                    candidates: vec![candidate],
                },
                crate::ReadExecutionContext {
                    binding: &binding,
                    local_effect: None,
                    mode: crate::ReadExecutionMode::Live,
                    provider_step_seq: 1,
                    provider_steps_remaining_after_current: 14,
                    input_budget: &mut budget,
                    cancellation: &cancellation,
                    events: &mut sink,
                    progressive_authority: Some(&authority),
                },
            )
            .unwrap();
        let crate::ReadExecutionOutputV2::DeepSearch(output) = output else {
            panic!("deep output")
        };

        assert!(output.provider_content.len() <= MAX_DEEP_PROVIDER_BYTES);
        assert_eq!(body_reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn progressive_provider_content_enforces_activity_and_turn_aggregate_caps_before_body_io() {
        for exhaust_turn_budget in [false, true] {
            let body_reads = Arc::new(AtomicUsize::new(0));
            let executor = RetrievalExecutor::new(ProgressiveFakeArchive {
                account: "account".into(),
                items: Arc::new(vec![
                    progressive_item("keyword", "Invoice 2026", "known keyword body"),
                    progressive_item("candidate", "Music receipt", "DistroKid annual charge"),
                ]),
                body_reads: Arc::clone(&body_reads),
            });
            let authority = crate::HmacProgressiveSearchAuthority::new([19; 32]);
            let cancellation = crate::CancellationToken::default();
            let (binding, activity_id, continuation, candidate) =
                open_progressive_candidate(&executor, &authority, &cancellation);
            {
                let mut state = executor.progressive.lock().unwrap();
                if exhaust_turn_budget {
                    state.provider_bytes =
                        MAX_TURN_PROGRESSIVE_PROVIDER_BYTES - MAX_DEEP_PROVIDER_BYTES + 1;
                } else {
                    state
                        .activities
                        .get_mut(&activity_id)
                        .unwrap()
                        .provider_bytes = MAX_ACTIVITY_PROVIDER_BYTES - MAX_DEEP_PROVIDER_BYTES + 1;
                }
            }
            let mut budget = crate::ProviderInputBudgetV1::new(None, 1_000_000, 0);
            let mut events = Vec::new();
            let mut collect = |event| events.push(event);
            let mut sink = crate::InfallibleTurnEventSink::new(&mut collect);

            let error = executor
                .execute_read_with_context(
                    &ToolAction::DeepSearch {
                        activity_id,
                        continuation,
                        candidates: vec![candidate],
                    },
                    crate::ReadExecutionContext {
                        binding: &binding,
                        local_effect: None,
                        mode: crate::ReadExecutionMode::Live,
                        provider_step_seq: 1,
                        provider_steps_remaining_after_current: 14,
                        input_budget: &mut budget,
                        cancellation: &cancellation,
                        events: &mut sink,
                        progressive_authority: Some(&authority),
                    },
                )
                .unwrap_err();

            assert!(matches!(
                error,
                AgentError::Provider(code) if code == "progressive_provider_budget_exhausted"
            ));
            assert_eq!(body_reads.load(Ordering::SeqCst), 0);
            assert_eq!(events.len(), 1);
            assert!(matches!(
                &events[0],
                StreamEvent::StageProgress(progress)
                    if progress.stage == SearchStage::Deep
                        && progress.status == StageStatus::Running
            ));
            assert!(!events
                .iter()
                .any(|event| matches!(event, StreamEvent::PartialResult(_))));
        }
    }

    #[test]
    fn deep_search_empty_selection_consumes_page_and_advances_without_body_read() {
        let body_reads = Arc::new(AtomicUsize::new(0));
        let executor = RetrievalExecutor::new(ProgressiveFakeArchive {
            account: "account".into(),
            items: Arc::new(vec![
                progressive_item("keyword", "Invoice 2026", "known keyword body"),
                progressive_item("candidate", "Music receipt", "DistroKid annual charge"),
            ]),
            body_reads: Arc::clone(&body_reads),
        });
        let authority = crate::HmacProgressiveSearchAuthority::new([20; 32]);
        let cancellation = crate::CancellationToken::default();
        let (binding, activity_id, continuation, _) =
            open_progressive_candidate(&executor, &authority, &cancellation);
        let mut budget = crate::ProviderInputBudgetV1::new(None, MAX_DEEP_PROVIDER_BYTES / 2, 0);
        let mut events = Vec::new();
        let mut collect = |event| events.push(event);
        let mut sink = crate::InfallibleTurnEventSink::new(&mut collect);

        let output = executor
            .execute_read_with_context(
                &ToolAction::DeepSearch {
                    activity_id: activity_id.clone(),
                    continuation: continuation.clone(),
                    candidates: Vec::new(),
                },
                crate::ReadExecutionContext {
                    binding: &binding,
                    local_effect: None,
                    mode: crate::ReadExecutionMode::Live,
                    provider_step_seq: 1,
                    provider_steps_remaining_after_current: 14,
                    input_budget: &mut budget,
                    cancellation: &cancellation,
                    events: &mut sink,
                    progressive_authority: Some(&authority),
                },
            )
            .unwrap();
        assert!(matches!(
            output,
            crate::ReadExecutionOutputV2::DeepSearch(_)
        ));
        assert_eq!(body_reads.load(Ordering::SeqCst), 0);
        let state = executor.progressive.lock().unwrap();
        assert_eq!(
            state.activities[&activity_id]
                .consumed_pages
                .get(&0)
                .map(String::as_str),
            Some(binding.tool_use_id.as_str())
        );
    }

    #[test]
    fn deep_search_rejects_candidate_not_in_issued_page() {
        let body_reads = Arc::new(AtomicUsize::new(0));
        let executor = RetrievalExecutor::new(ProgressiveFakeArchive {
            account: "account".into(),
            items: Arc::new(vec![
                progressive_item("keyword", "Invoice 2026", "known keyword body"),
                progressive_item("candidate", "Music receipt", "DistroKid annual charge"),
            ]),
            body_reads: Arc::clone(&body_reads),
        });
        let authority = crate::HmacProgressiveSearchAuthority::new([21; 32]);
        let cancellation = crate::CancellationToken::default();
        let (binding, activity_id, continuation, _) =
            open_progressive_candidate(&executor, &authority, &cancellation);
        let mut budget = crate::ProviderInputBudgetV1::new(None, 1_000_000, 0);
        let mut events = Vec::new();
        let mut collect = |event| events.push(event);
        let mut sink = crate::InfallibleTurnEventSink::new(&mut collect);

        let error = executor
            .execute_read_with_context(
                &ToolAction::DeepSearch {
                    activity_id,
                    continuation,
                    candidates: vec!["A".repeat(43)],
                },
                crate::ReadExecutionContext {
                    binding: &binding,
                    local_effect: None,
                    mode: crate::ReadExecutionMode::Live,
                    provider_step_seq: 1,
                    provider_steps_remaining_after_current: 14,
                    input_budget: &mut budget,
                    cancellation: &cancellation,
                    events: &mut sink,
                    progressive_authority: Some(&authority),
                },
            )
            .unwrap_err();

        assert!(matches!(error, AgentError::ToolArgs(_)));
        assert_eq!(body_reads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn deep_search_at_step_fourteen_offers_no_unconsumable_continuation() {
        let body_reads = Arc::new(AtomicUsize::new(0));
        let executor = RetrievalExecutor::new(ProgressiveFakeArchive {
            account: "account".into(),
            items: Arc::new(vec![
                progressive_item("keyword", "Invoice 2026", "known keyword body"),
                progressive_item("candidate", "Music receipt", "DistroKid annual charge"),
            ]),
            body_reads: Arc::clone(&body_reads),
        });
        let authority = crate::HmacProgressiveSearchAuthority::new([21; 32]);
        let cancellation = crate::CancellationToken::default();
        let binding = crate::ReadExecutionBindingV2 {
            session_id: "session".into(),
            request_id: "request".into(),
            tool_use_id: "search-at-step-14".into(),
            resolved_account_key: "account".into(),
            admission_account_digest: crate::admission_account_digest("account").unwrap(),
        };
        let mut budget = crate::ProviderInputBudgetV1::new(None, 1_000_000, 0);
        let mut events = Vec::new();
        let mut collect = |event| events.push(event);
        let mut sink = crate::InfallibleTurnEventSink::new(&mut collect);

        let output = executor
            .execute_read_with_context(
                &ToolAction::Search {
                    account: "account".into(),
                    services: vec!["mail".into()],
                    query: "invoice".into(),
                    limit: Some(20),
                },
                crate::ReadExecutionContext {
                    binding: &binding,
                    local_effect: None,
                    mode: crate::ReadExecutionMode::Live,
                    provider_step_seq: 14,
                    provider_steps_remaining_after_current: 1,
                    input_budget: &mut budget,
                    cancellation: &cancellation,
                    events: &mut sink,
                    progressive_authority: Some(&authority),
                },
            )
            .unwrap();
        let crate::ReadExecutionOutputV2::Search(output) = output else {
            panic!("search output")
        };
        let provider: serde_json::Value = serde_json::from_str(&output.provider_content).unwrap();
        assert!(provider["deep_context"].is_null());
        assert!(!output.public_projection.continuation_available);
        assert!(!output.public_projection.coverage_complete);
        assert!(output.public_projection.budget_reached);
        assert_eq!(body_reads.load(Ordering::SeqCst), 0);
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::StageProgress(progress)
                if progress.stage == SearchStage::Deep
                    && progress.status == StageStatus::Complete
                    && progress.coverage_complete == Some(false)
                    && progress.continuation_available == Some(false)
        )));
    }

    #[test]
    fn previous_continuation_consumed_at_last_step_is_rejected_before_body_io() {
        let body_reads = Arc::new(AtomicUsize::new(0));
        let executor = RetrievalExecutor::new(ProgressiveFakeArchive {
            account: "account".into(),
            items: Arc::new(vec![
                progressive_item("keyword", "Invoice 2026", "known keyword body"),
                progressive_item("candidate", "Music receipt", "DistroKid annual charge"),
            ]),
            body_reads: Arc::clone(&body_reads),
        });
        let authority = crate::HmacProgressiveSearchAuthority::new([22; 32]);
        let cancellation = crate::CancellationToken::default();
        let (binding, activity_id, continuation, candidate) =
            open_progressive_candidate(&executor, &authority, &cancellation);
        let mut budget = crate::ProviderInputBudgetV1::new(None, 1_000_000, 0);
        let mut events = Vec::new();
        let mut collect = |event| events.push(event);
        let mut sink = crate::InfallibleTurnEventSink::new(&mut collect);

        let error = executor
            .execute_read_with_context(
                &ToolAction::DeepSearch {
                    activity_id,
                    continuation,
                    candidates: vec![candidate],
                },
                crate::ReadExecutionContext {
                    binding: &binding,
                    local_effect: None,
                    mode: crate::ReadExecutionMode::Live,
                    provider_step_seq: 15,
                    provider_steps_remaining_after_current: 0,
                    input_budget: &mut budget,
                    cancellation: &cancellation,
                    events: &mut sink,
                    progressive_authority: Some(&authority),
                },
            )
            .unwrap_err();
        assert!(matches!(
            error,
            AgentError::Provider(code) if code == "provider_step_budget_exhausted"
        ));
        assert_eq!(body_reads.load(Ordering::SeqCst), 0);
        assert!(events.is_empty());
    }

    #[test]
    fn deep_search_rejects_replayed_or_out_of_order_page() {
        let body_reads = Arc::new(AtomicUsize::new(0));
        let executor = RetrievalExecutor::new(ProgressiveFakeArchive {
            account: "account".into(),
            items: Arc::new(vec![
                progressive_item("keyword", "Invoice 2026", "known keyword body"),
                progressive_item("candidate", "Music receipt", "DistroKid annual charge"),
            ]),
            body_reads: Arc::clone(&body_reads),
        });
        let authority = crate::HmacProgressiveSearchAuthority::new([23; 32]);
        let cancellation = crate::CancellationToken::default();
        let (binding, activity_id, continuation, candidate) =
            open_progressive_candidate(&executor, &authority, &cancellation);
        let action = ToolAction::DeepSearch {
            activity_id,
            continuation,
            candidates: vec![candidate],
        };
        let mut events = Vec::new();
        let mut collect = |event| events.push(event);
        let mut sink = crate::InfallibleTurnEventSink::new(&mut collect);
        let mut budget = crate::ProviderInputBudgetV1::new(None, 1_000_000, 0);
        executor
            .execute_read_with_context(
                &action,
                crate::ReadExecutionContext {
                    binding: &binding,
                    local_effect: None,
                    mode: crate::ReadExecutionMode::Live,
                    provider_step_seq: 1,
                    provider_steps_remaining_after_current: 14,
                    input_budget: &mut budget,
                    cancellation: &cancellation,
                    events: &mut sink,
                    progressive_authority: Some(&authority),
                },
            )
            .unwrap();
        let mut replay_budget = crate::ProviderInputBudgetV1::new(None, 1_000_000, 0);
        let error = executor
            .execute_read_with_context(
                &action,
                crate::ReadExecutionContext {
                    binding: &binding,
                    local_effect: None,
                    mode: crate::ReadExecutionMode::Live,
                    provider_step_seq: 2,
                    provider_steps_remaining_after_current: 13,
                    input_budget: &mut replay_budget,
                    cancellation: &cancellation,
                    events: &mut sink,
                    progressive_authority: Some(&authority),
                },
            )
            .unwrap_err();
        assert!(matches!(
            error,
            AgentError::ToolArgs(message) if message == "deep-search page already consumed"
        ));
        assert_eq!(body_reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn deep_search_exact_same_action_recovery_may_compare_replay_consumed_page() {
        let body_reads = Arc::new(AtomicUsize::new(0));
        let executor = RetrievalExecutor::new(ProgressiveFakeArchive {
            account: "account".into(),
            items: Arc::new(vec![
                progressive_item("keyword", "Invoice 2026", "known keyword body"),
                progressive_item("candidate", "Music receipt", "DistroKid annual charge"),
            ]),
            body_reads: Arc::clone(&body_reads),
        });
        let authority = crate::HmacProgressiveSearchAuthority::new([24; 32]);
        let cancellation = crate::CancellationToken::default();
        let (binding, activity_id, continuation, candidate) =
            open_progressive_candidate(&executor, &authority, &cancellation);
        let action = ToolAction::DeepSearch {
            activity_id,
            continuation,
            candidates: vec![candidate],
        };
        let mut events = Vec::new();
        let mut collect = |event| events.push(event);
        let mut sink = crate::InfallibleTurnEventSink::new(&mut collect);
        let mut budget = crate::ProviderInputBudgetV1::new(None, 1_000_000, 0);
        let live = executor
            .execute_read_with_context(
                &action,
                crate::ReadExecutionContext {
                    binding: &binding,
                    local_effect: None,
                    mode: crate::ReadExecutionMode::Live,
                    provider_step_seq: 1,
                    provider_steps_remaining_after_current: 14,
                    input_budget: &mut budget,
                    cancellation: &cancellation,
                    events: &mut sink,
                    progressive_authority: Some(&authority),
                },
            )
            .unwrap()
            .into_completion(&action)
            .unwrap();
        let mut recovery_budget = crate::ProviderInputBudgetV1::new(None, 1_000_000, 0);
        let recovered = executor
            .execute_read_with_context(
                &action,
                crate::ReadExecutionContext {
                    binding: &binding,
                    local_effect: None,
                    mode: crate::ReadExecutionMode::RecoveryCompare,
                    provider_step_seq: 1,
                    provider_steps_remaining_after_current: 14,
                    input_budget: &mut recovery_budget,
                    cancellation: &cancellation,
                    events: &mut sink,
                    progressive_authority: Some(&authority),
                },
            )
            .unwrap()
            .into_completion(&action)
            .unwrap();
        assert_eq!(live.provider_content, recovered.provider_content);
        assert_eq!(live.assistant_sources, recovered.assistant_sources);
        assert_eq!(body_reads.load(Ordering::SeqCst), 2);
    }

    fn progressive_executor_with_open_deep() -> RetrievalExecutor<ProgressiveFakeArchive> {
        let executor = RetrievalExecutor::new(ProgressiveFakeArchive {
            account: "account".into(),
            items: Arc::new(vec![
                progressive_item("keyword", "Invoice 2026", "known keyword body"),
                progressive_item("candidate", "Music receipt", "DistroKid annual charge"),
            ]),
            body_reads: Arc::new(AtomicUsize::new(0)),
        });
        let authority = crate::HmacProgressiveSearchAuthority::new([9; 32]);
        let cancellation = crate::CancellationToken::default();
        let binding = crate::ReadExecutionBindingV2 {
            session_id: "session".into(),
            request_id: "request".into(),
            tool_use_id: "search-A".into(),
            resolved_account_key: "account".into(),
            admission_account_digest: crate::admission_account_digest("account").unwrap(),
        };
        let mut budget = crate::ProviderInputBudgetV1::new(None, 1_000_000, 0);
        let mut events = Vec::new();
        let mut collect = |event| events.push(event);
        let mut sink = crate::InfallibleTurnEventSink::new(&mut collect);
        executor
            .execute_read_with_context(
                &ToolAction::Search {
                    account: "account".into(),
                    services: vec!["mail".into()],
                    query: "invoice".into(),
                    limit: Some(20),
                },
                crate::ReadExecutionContext {
                    binding: &binding,
                    local_effect: None,
                    mode: crate::ReadExecutionMode::Live,
                    provider_step_seq: 0,
                    provider_steps_remaining_after_current: 15,
                    input_budget: &mut budget,
                    cancellation: &cancellation,
                    events: &mut sink,
                    progressive_authority: Some(&authority),
                },
            )
            .unwrap();
        executor
    }

    fn set_progressive_stage_statuses(
        executor: &RetrievalExecutor<ProgressiveFakeArchive>,
        names: StageStatus,
        bodies: StageStatus,
        deep: StageStatus,
    ) {
        let mut state = executor.progressive.lock().unwrap();
        let activity_id = state.creation_order[0].clone();
        let activity = state.activities.get_mut(&activity_id).unwrap();
        activity.names_status = names;
        activity.bodies_status = bodies;
        activity.deep_status = deep;
    }

    fn finish_progressive_with_statuses(
        exit: TurnExitKind,
        names: StageStatus,
        bodies: StageStatus,
        deep: StageStatus,
    ) -> (crate::TurnExitOutputV1, Vec<StreamEvent>) {
        let executor = progressive_executor_with_open_deep();
        set_progressive_stage_statuses(&executor, names, bodies, deep);
        let mut events = Vec::new();
        let mut collect = |event| events.push(event);
        let mut sink = crate::InfallibleTurnEventSink::new(&mut collect);
        let output = executor
            .finish_with_exit(
                exit,
                (exit == TurnExitKind::Final).then(|| "Answer".to_owned()),
                Vec::new(),
                &mut sink,
            )
            .unwrap();
        (output, events)
    }

    #[test]
    fn progressive_search_finish_with_exit_closes_every_turn_outcome() {
        let cases = [
            (TurnExitKind::Final, StageStatus::Complete),
            (TurnExitKind::PendingConfirmation, StageStatus::Skipped),
            (TurnExitKind::ProviderError, StageStatus::Failed),
            (TurnExitKind::Cancelled, StageStatus::Cancelled),
            (TurnExitKind::OutcomeUnknown, StageStatus::Failed),
            (TurnExitKind::StepLimit, StageStatus::Failed),
        ];
        for (exit, expected_status) in cases {
            let executor = progressive_executor_with_open_deep();
            let mut events = Vec::new();
            let mut collect = |event| events.push(event);
            let mut sink = crate::InfallibleTurnEventSink::new(&mut collect);
            let output = executor
                .finish_with_exit(
                    exit,
                    (exit == TurnExitKind::Final).then(|| "Answer".to_owned()),
                    Vec::new(),
                    &mut sink,
                )
                .unwrap();
            assert_eq!(output.exit_state.exit_kind, exit);
            assert_eq!(output.exit_state.activities.len(), 1);
            assert_eq!(output.exit_state.activities[0].deep_status, expected_status);
            assert!(events.iter().any(|event| matches!(
                event,
                StreamEvent::StageProgress(progress)
                    if progress.stage == SearchStage::Deep
                        && progress.status == expected_status
                        && progress.continuation_available == Some(false)
            )));
            match exit {
                TurnExitKind::Final => {
                    let completion = output.completion.unwrap();
                    assert!(completion.final_text.ends_with(INCOMPLETE_COVERAGE_NOTE));
                    let marker = completion.progressive_finalization.unwrap();
                    assert_eq!(
                        marker.coverage_note.unwrap().reason,
                        CoverageNoteReason::Incomplete
                    );
                    assert!(!marker.activities[0].continuation_available);
                }
                _ => assert!(output.completion.is_none()),
            }
        }
    }

    #[test]
    fn multiple_incomplete_activities_append_one_deterministic_coverage_note() {
        let executor = RetrievalExecutor::new(ProgressiveFakeArchive {
            account: "account".into(),
            items: Arc::new(vec![
                progressive_item("keyword", "Invoice 2026", "known keyword body"),
                progressive_item("candidate", "Music receipt", "DistroKid annual charge"),
            ]),
            body_reads: Arc::new(AtomicUsize::new(0)),
        });
        let authority = crate::HmacProgressiveSearchAuthority::new([30; 32]);
        let cancellation = crate::CancellationToken::default();
        let mut activity_ids = Vec::new();
        let mut events = Vec::new();
        let mut collect = |event| events.push(event);
        let mut sink = crate::InfallibleTurnEventSink::new(&mut collect);

        for (index, tool_use_id) in ["search-a", "search-b"].into_iter().enumerate() {
            let binding = crate::ReadExecutionBindingV2 {
                session_id: "session".into(),
                request_id: format!("request-{index}"),
                tool_use_id: tool_use_id.into(),
                resolved_account_key: "account".into(),
                admission_account_digest: crate::admission_account_digest("account").unwrap(),
            };
            let mut budget = crate::ProviderInputBudgetV1::new(None, 1_000_000, 0);
            let output = executor
                .execute_read_with_context(
                    &ToolAction::Search {
                        account: "account".into(),
                        services: vec!["mail".into()],
                        query: "invoice".into(),
                        limit: Some(20),
                    },
                    crate::ReadExecutionContext {
                        binding: &binding,
                        local_effect: None,
                        mode: crate::ReadExecutionMode::Live,
                        provider_step_seq: u8::try_from(index).unwrap(),
                        provider_steps_remaining_after_current: 15,
                        input_budget: &mut budget,
                        cancellation: &cancellation,
                        events: &mut sink,
                        progressive_authority: Some(&authority),
                    },
                )
                .unwrap();
            let crate::ReadExecutionOutputV2::Search(output) = output else {
                panic!("search output")
            };
            activity_ids.push(output.public_projection.activity_id);
        }

        let finalized = executor
            .finish_with_exit(
                TurnExitKind::Final,
                Some("Answer".into()),
                Vec::new(),
                &mut sink,
            )
            .unwrap()
            .completion
            .unwrap();
        let marker = finalized.progressive_finalization.unwrap();

        assert_eq!(
            marker
                .activities
                .iter()
                .map(|activity| activity.activity_id.clone())
                .collect::<Vec<_>>(),
            activity_ids
        );
        assert_eq!(
            finalized
                .final_text
                .matches(INCOMPLETE_COVERAGE_NOTE)
                .count(),
            1
        );
        assert_eq!(
            marker.coverage_note.unwrap().reason,
            CoverageNoteReason::Incomplete
        );
    }

    #[test]
    fn progressive_search_stage_failure_commits_one_terminal_state_per_stage() {
        let (output, events) = finish_progressive_with_statuses(
            TurnExitKind::ProviderError,
            StageStatus::Running,
            StageStatus::Queued,
            StageStatus::Queued,
        );
        let activity = &output.exit_state.activities[0];
        assert_eq!(activity.names_status, StageStatus::Failed);
        assert_eq!(activity.bodies_status, StageStatus::Skipped);
        assert_eq!(activity.deep_status, StageStatus::Skipped);
        for (stage, status) in [
            (SearchStage::Names, StageStatus::Failed),
            (SearchStage::Bodies, StageStatus::Skipped),
            (SearchStage::Deep, StageStatus::Skipped),
        ] {
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(
                        event,
                        StreamEvent::StageProgress(progress)
                            if progress.stage == stage && progress.status == status
                    ))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn progressive_search_pending_confirmation_skips_open_stages_truthfully() {
        let (output, _) = finish_progressive_with_statuses(
            TurnExitKind::PendingConfirmation,
            StageStatus::Complete,
            StageStatus::Running,
            StageStatus::Queued,
        );
        let activity = &output.exit_state.activities[0];
        assert_eq!(activity.names_status, StageStatus::Complete);
        assert_eq!(activity.bodies_status, StageStatus::Skipped);
        assert_eq!(activity.deep_status, StageStatus::Skipped);
        assert!(output.completion.is_none());
    }

    #[test]
    fn progressive_search_provider_error_fails_current_and_skips_future_stages() {
        let (output, _) = finish_progressive_with_statuses(
            TurnExitKind::ProviderError,
            StageStatus::Complete,
            StageStatus::Running,
            StageStatus::Queued,
        );
        let activity = &output.exit_state.activities[0];
        assert_eq!(activity.names_status, StageStatus::Complete);
        assert_eq!(activity.bodies_status, StageStatus::Failed);
        assert_eq!(activity.deep_status, StageStatus::Skipped);
    }

    #[test]
    fn progressive_search_outcome_unknown_persists_no_coverage_success() {
        let (output, _) = finish_progressive_with_statuses(
            TurnExitKind::OutcomeUnknown,
            StageStatus::Complete,
            StageStatus::Complete,
            StageStatus::Running,
        );
        let activity = &output.exit_state.activities[0];
        assert_eq!(activity.deep_status, StageStatus::Failed);
        assert_eq!(
            output.exit_state.terminal_code.as_deref(),
            Some("turn_outcome_unknown")
        );
        assert!(output.completion.is_none());
    }

    #[test]
    fn progressive_search_cancelled_exit_cancels_running_and_queued_stages() {
        let (output, _) = finish_progressive_with_statuses(
            TurnExitKind::Cancelled,
            StageStatus::Running,
            StageStatus::Queued,
            StageStatus::Queued,
        );
        let activity = &output.exit_state.activities[0];
        assert_eq!(activity.names_status, StageStatus::Cancelled);
        assert_eq!(activity.bodies_status, StageStatus::Cancelled);
        assert_eq!(activity.deep_status, StageStatus::Cancelled);
    }

    struct CancelOnStageSink {
        cancellation: crate::CancellationToken,
        stage: SearchStage,
        events: Vec<StreamEvent>,
    }

    impl TurnEventSink for CancelOnStageSink {
        fn emit(&mut self, event: StreamEvent) -> Result<(), AgentError> {
            if matches!(
                &event,
                StreamEvent::StageProgress(progress)
                    if progress.stage == self.stage && progress.status == StageStatus::Running
            ) {
                self.cancellation.cancel();
            }
            self.events.push(event);
            Ok(())
        }
    }

    #[test]
    fn progressive_search_cancel_during_each_stage_stops_further_io() {
        for stage in [SearchStage::Names, SearchStage::Bodies, SearchStage::Deep] {
            let executor = RetrievalExecutor::new(ProgressiveFakeArchive {
                account: "account".into(),
                items: Arc::new(vec![
                    progressive_item("keyword", "Invoice 2026", "known keyword body"),
                    progressive_item("candidate", "Music receipt", "DistroKid annual charge"),
                ]),
                body_reads: Arc::new(AtomicUsize::new(0)),
            });
            let authority = crate::HmacProgressiveSearchAuthority::new([31; 32]);
            let cancellation = crate::CancellationToken::default();
            let binding = crate::ReadExecutionBindingV2 {
                session_id: "session".into(),
                request_id: format!("request-{stage:?}"),
                tool_use_id: format!("search-{stage:?}"),
                resolved_account_key: "account".into(),
                admission_account_digest: crate::admission_account_digest("account").unwrap(),
            };
            let mut budget = crate::ProviderInputBudgetV1::new(None, 1_000_000, 0);
            let mut sink = CancelOnStageSink {
                cancellation: cancellation.clone(),
                stage,
                events: Vec::new(),
            };
            let error = executor
                .execute_read_with_context(
                    &ToolAction::Search {
                        account: "account".into(),
                        services: vec!["mail".into()],
                        query: "invoice".into(),
                        limit: Some(20),
                    },
                    crate::ReadExecutionContext {
                        binding: &binding,
                        local_effect: None,
                        mode: crate::ReadExecutionMode::Live,
                        provider_step_seq: 0,
                        provider_steps_remaining_after_current: 15,
                        input_budget: &mut budget,
                        cancellation: &cancellation,
                        events: &mut sink,
                        progressive_authority: Some(&authority),
                    },
                )
                .unwrap_err();
            assert!(matches!(error, AgentError::Cancelled));
            let target_index = sink
                .events
                .iter()
                .position(|event| {
                    matches!(
                        event,
                        StreamEvent::StageProgress(progress)
                            if progress.stage == stage
                                && progress.status == StageStatus::Running
                    )
                })
                .unwrap();
            assert!(
                !sink.events[target_index + 1..].iter().any(|event| matches!(
                    event,
                    StreamEvent::StageProgress(progress) if progress.status == StageStatus::Running
                ))
            );
        }
    }

    struct RejectingSink;

    impl TurnEventSink for RejectingSink {
        fn emit(&mut self, _event: StreamEvent) -> Result<(), AgentError> {
            Err(AgentError::StreamUnavailable)
        }
    }

    #[test]
    fn progressive_search_terminal_delivery_is_not_claimed_after_sink_loss() {
        let executor = progressive_executor_with_open_deep();
        let output = executor
            .finish_with_exit(
                TurnExitKind::ProviderError,
                None,
                Vec::new(),
                &mut RejectingSink,
            )
            .unwrap();
        assert_eq!(
            output.terminal_event_delivery,
            crate::TerminalEventDelivery::Unavailable
        );
        assert_eq!(
            output.exit_state.activities[0].deep_status,
            StageStatus::Failed
        );
    }

    struct RoutingArchive {
        account: String,
        calls: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    }

    impl RoutingArchive {
        fn new(calls: std::rc::Rc<std::cell::RefCell<Vec<String>>>) -> Self {
            Self {
                account: "me".into(),
                calls,
            }
        }

        fn route_item(service: &str, id: &str, name: &str) -> ItemRef {
            ItemRef {
                service: service.into(),
                id: id.into(),
                name: name.into(),
                item_type: "folder".into(),
                path: Some(format!("{service}/{id}.bin")),
            }
        }
    }

    impl ArchiveSource for RoutingArchive {
        fn account(&self) -> &str {
            &self.account
        }

        fn search_names(&self, _query: &str) -> Result<Vec<ItemRef>, AgentError> {
            Ok(Vec::new())
        }

        fn search_bodies(&self, _query: &str) -> Result<Vec<(String, String)>, AgentError> {
            Ok(Vec::new())
        }

        fn get(&self, _service: &str, _id: &str) -> Result<Option<ItemRef>, AgentError> {
            Ok(None)
        }

        fn read_body(&self, service: &str, id: &str) -> Result<Vec<u8>, AgentError> {
            Err(AgentError::ToolArgs(format!("no body {service}/{id}")))
        }

        fn list_page(
            &self,
            service: &str,
            limit: u32,
            offset: u32,
        ) -> Result<Vec<ItemRef>, AgentError> {
            self.calls
                .borrow_mut()
                .push(format!("list_page:{limit}:{offset}"));
            let items = vec![
                Self::route_item(service, "flat-0", "Flat 0"),
                Self::route_item(service, "flat-1", "Flat 1"),
                Self::route_item(service, "flat-2", "Flat 2"),
            ];
            Ok(items
                .into_iter()
                .skip(offset as usize)
                .take(limit as usize)
                .collect())
        }

        fn roots(&self, service: &str) -> Result<Vec<ItemRef>, AgentError> {
            self.calls.borrow_mut().push("roots".into());
            Ok(vec![Self::route_item(service, "root-only", "Root Only")])
        }

        fn children(&self, service: &str, parent: &str) -> Result<Vec<ItemRef>, AgentError> {
            self.calls.borrow_mut().push(format!("children:{parent}"));
            Ok(vec![Self::route_item(service, "child-only", "Child Only")])
        }

        fn count(&self, _service: &str) -> Result<u64, AgentError> {
            Ok(123)
        }
    }

    fn fixture() -> RetrievalExecutor<FakeArchive> {
        RetrievalExecutor::new(FakeArchive {
            account: "me".into(),
            items: vec![
                FakeArchive::item(
                    "mail",
                    "m1",
                    "Spotify invoice March",
                    Some("Your Spotify receipt, total 9.99"),
                ),
                FakeArchive::item("mail", "m2", "Dinner plans", Some("see you at 8")),
                FakeArchive::item("onedrive", "f1", "spotify-logo.png", None),
            ],
        })
    }

    #[test]
    fn partial_result_sequence_and_result_keys_are_stable() {
        let executor = fixture();
        let item = &executor.source.items[0].0;
        let activity_id = "abcdefghijklmnopqrstuv";
        let key = RetrievalExecutor::<FakeArchive>::result_key(activity_id, item);
        assert_eq!(
            key,
            RetrievalExecutor::<FakeArchive>::result_key(activity_id, item)
        );
        assert_ne!(
            key,
            RetrievalExecutor::<FakeArchive>::result_key("zyxwvutsrqponmlkjihgfe", item)
        );

        let items = (0..(MAX_PARTIAL_RESULT_ITEMS + 1))
            .map(|index| {
                let mut item = item.clone();
                item.id = format!("item-{index}");
                RetrievalExecutor::<FakeArchive>::public_result(
                    activity_id,
                    &item,
                    ResultChange::Add,
                )
            })
            .collect();
        let mut sequence = 0;
        let mut events = Vec::new();
        let mut event_budget = ActivityEventBudget::default();
        RetrievalExecutor::<FakeArchive>::emit_partial_batches(
            activity_id,
            SearchStage::Names,
            &mut sequence,
            items,
            &mut event_budget,
            &mut |event| events.push(event),
            true,
            &mut || Ok(()),
        )
        .unwrap();
        let observed = events
            .into_iter()
            .map(|event| match event {
                StreamEvent::PartialResult(result) => (result.sequence, result.items.len()),
                _ => panic!("only partial results are emitted"),
            })
            .collect::<Vec<_>>();
        assert_eq!(observed, vec![(0, MAX_PARTIAL_RESULT_ITEMS), (1, 1)]);
        assert_eq!(sequence, 2);
    }

    #[test]
    fn item_local_path_is_never_projected_as_display_path() {
        let mut item = fixture().source.items[0].0.clone();
        item.path = Some("mail/private-cache/body.eml".into());
        let public = RetrievalExecutor::<FakeArchive>::public_result(
            "abcdefghijklmnopqrstuv",
            &item,
            ResultChange::Add,
        );
        assert!(public.display_path.is_none());
        assert!(!serde_json::to_string(&public)
            .unwrap()
            .contains("private-cache"));
    }

    #[cfg(feature = "retrieval")]
    fn upsert_store_body(
        store: &isyncyou_store::Store,
        root: &std::path::Path,
        mut item: isyncyou_store::Item,
        rel: &str,
        body: &[u8],
    ) {
        item.local_path = Some(rel.into());
        store.upsert_item(&item).unwrap();
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        isyncyou_core::envelope::write_body_atomic(&path, body).unwrap();
    }

    #[test]
    fn search_returns_source_tagged_hits_across_names_and_bodies() {
        let ex = fixture();
        let out = ex.search(&[], "spotify", None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        // m1 (name + body) and f1 (name) match; deduped; each carries source ids.
        assert_eq!(v["total_matches"], 2);
        let ids: Vec<&str> = v["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"m1") && ids.contains(&"f1"));
        for r in v["results"].as_array().unwrap() {
            assert!(r["service"].is_string() && r["id"].is_string() && r["path"].is_string());
            assert!(r["source"]["service"].is_string() && r["source"]["id"].is_string());
            assert!(r["snippet"].is_string());
        }
    }

    #[test]
    fn progressive_search_streams_staged_deduped_results() {
        let ex = fixture();
        let mut events: Vec<StreamEvent> = Vec::new();
        let out = ex
            .search_staged(&[], "spotify", None, &mut |e| events.push(e))
            .unwrap();

        // Stage boundaries, in order: names running→done, then bodies running→done.
        let stages: Vec<(SearchStage, StageStatus, u32)> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::StageProgress(progress) => {
                    Some((progress.stage, progress.status, progress.hits))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            stages,
            vec![
                (SearchStage::Names, StageStatus::Running, 0),
                (SearchStage::Names, StageStatus::Complete, 2),
                (SearchStage::Bodies, StageStatus::Running, 2),
                (SearchStage::Bodies, StageStatus::Complete, 2),
            ]
        );

        // Partial results: stage 1 carries the two name hits; the empty deduped body
        // stage does not emit an empty batch.
        let partials: Vec<(SearchStage, usize)> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::PartialResult(result) => Some((result.stage, result.items.len())),
                _ => None,
            })
            .collect();
        assert_eq!(partials, vec![(SearchStage::Names, 2)]);

        // Final JSON equals the non-staged search: deduped total, source-tagged, + hint.
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["total_matches"], 2);
        assert!(v["deep_search_hint"].is_string());
        for r in v["results"].as_array().unwrap() {
            for field in ["service", "id", "name", "item_type", "path", "snippet"] {
                assert!(
                    !r[field].is_null(),
                    "staged final result must carry {field}: {r}"
                );
            }
        }
    }

    #[test]
    fn progressive_search_stage2_adds_body_only_hits() {
        // "receipt" appears only in m1's body, in no name — stage 2 (full-text) must add it.
        let ex = fixture();
        let mut events: Vec<StreamEvent> = Vec::new();
        let out = ex
            .search_staged(&[], "receipt", None, &mut |e| events.push(e))
            .unwrap();
        let done: Vec<(SearchStage, u32)> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::StageProgress(progress)
                    if progress.status == StageStatus::Complete =>
                {
                    Some((progress.stage, progress.hits))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            done,
            vec![(SearchStage::Names, 0), (SearchStage::Bodies, 1)]
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["results"][0]["id"], "m1");
        assert!(
            v["results"][0]["snippet"]
                .as_str()
                .unwrap()
                .contains("receipt"),
            "body-only search hit should carry a readable snippet"
        );
    }

    #[test]
    fn non_streaming_search_body_only_hit_carries_snippet() {
        let ex = fixture();
        let out = ex.search(&[], "receipt", None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["results"][0]["id"], "m1");
        assert!(v["results"][0]["snippet"]
            .as_str()
            .unwrap()
            .contains("receipt"));
    }

    #[test]
    fn search_keeps_unreadable_body_hit_with_empty_snippet() {
        let ex = fixture();
        let out = ex.search(&["onedrive".into()], "spotify", None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["results"].as_array().unwrap().len(), 1);
        assert_eq!(v["results"][0]["id"], "f1");
        assert_eq!(v["results"][0]["snippet"], "");
    }

    #[test]
    fn deep_search_surfaces_keyword_less_candidate_and_excludes_matched() {
        // m1's body literally contains the query (found by plain search → excluded from the
        // deep pass); m2 is about the same topic but never says "distrokid" (the keyword-less
        // match the deep read must surface); m3 is noise.
        let ex = RetrievalExecutor::new(FakeArchive {
            account: "me".into(),
            items: vec![
                FakeArchive::item(
                    "mail",
                    "m1",
                    "Invoice March",
                    Some("Your DistroKid renewal, 22.99"),
                ),
                FakeArchive::item(
                    "mail",
                    "m2",
                    "Music payout",
                    Some("Your streaming distributor paid out 41.00"),
                ),
                FakeArchive::item("mail", "m3", "Lunch", Some("see you at noon")),
            ],
        });
        let mut events: Vec<StreamEvent> = Vec::new();
        let out = ex
            .deep_search(&["mail".into()], "distrokid", None, Some(10), &mut |e| {
                events.push(e)
            })
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let ids: Vec<&str> = v["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        assert!(
            ids.contains(&"m2"),
            "keyword-less candidate must be surfaced"
        );
        assert!(
            !ids.contains(&"m1"),
            "keyword-matched item is excluded from the deep pass"
        );
        let m2 = v["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "m2")
            .unwrap();
        assert!(
            m2["snippet"].as_str().unwrap().contains("distributor"),
            "the candidate body is surfaced for the model to judge"
        );
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::StageProgress(progress)
                if progress.stage == SearchStage::Deep
                    && progress.status == StageStatus::Complete
        )));
    }

    #[test]
    fn deep_search_budget_and_cursor_paginate() {
        let items: Vec<_> = (0..5)
            .map(|i| {
                FakeArchive::item(
                    "mail",
                    &format!("d{i}"),
                    &format!("Note {i}"),
                    Some(&format!("body {i}")),
                )
            })
            .collect();
        let ex = RetrievalExecutor::new(FakeArchive {
            account: "me".into(),
            items,
        });
        // Query matches nothing → all 5 are unmatched candidates; budget of 2 per pass.
        let out = ex
            .deep_search(&["mail".into()], "zzzznomatch", None, Some(2), &mut |_| {})
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["read"], 2);
        assert_eq!(v["candidates_total"], 5);
        assert_eq!(v["budget_reached"], true);
        assert_eq!(v["next_cursor"], 2);
        // Resume from the cursor to search deeper.
        let out2 = ex
            .deep_search(
                &["mail".into()],
                "zzzznomatch",
                Some(2),
                Some(2),
                &mut |_| {},
            )
            .unwrap();
        let v2: serde_json::Value = serde_json::from_str(&out2).unwrap();
        assert_eq!(v2["cursor"], 2);
        assert_eq!(v2["read"], 2);
        assert_eq!(v2["next_cursor"], 4);
    }

    #[test]
    fn search_respects_service_filter_and_limit() {
        let ex = fixture();
        let mail_only: serde_json::Value =
            serde_json::from_str(&ex.search(&["mail".into()], "spotify", None).unwrap()).unwrap();
        assert_eq!(mail_only["results"].as_array().unwrap().len(), 1); // only m1
        let limited: serde_json::Value =
            serde_json::from_str(&ex.search(&[], "spotify", Some(1)).unwrap()).unwrap();
        assert_eq!(limited["returned"], 1);
        assert_eq!(limited["total_matches"], 2);
    }

    #[test]
    fn read_respects_byte_budget_and_flags_truncation() {
        let ex = fixture();
        let out = ex.read("mail", "m1", Some(10)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["truncated"], true);
        assert_eq!(v["bytes_returned"], 10);
        assert!(v["bytes_total"].as_u64().unwrap() > 10);
        assert_eq!(v["content"].as_str().unwrap().len(), 10);
        // Without a budget it returns the whole body, untruncated.
        let full: serde_json::Value =
            serde_json::from_str(&ex.read("mail", "m1", None).unwrap()).unwrap();
        assert_eq!(full["truncated"], false);
    }

    #[test]
    fn read_counts_model_text_and_truncates_on_utf8_boundary() {
        let ex = RetrievalExecutor::new(FakeArchive {
            account: "me".into(),
            items: vec![FakeArchive::item("onedrive", "u1", "Utf8", Some("ééx"))],
        });
        let out = ex.read("onedrive", "u1", Some(3)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["bytes_total"], "ééx".len());
        assert_eq!(v["bytes_returned"], "é".len());
        assert_eq!(v["truncated"], true);
        assert_eq!(v["content"], "é");
    }

    #[test]
    fn read_includes_source_object() {
        let ex = fixture();
        let out = ex.read("mail", "m1", None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["source"]["service"], "mail");
        assert_eq!(v["source"]["id"], "m1");
        assert_eq!(v["source"]["path"], "mail/m1.bin");
        assert_eq!(v["name"], "Spotify invoice March");
        assert_eq!(v["content_kind"], "mail-text");
    }

    #[test]
    fn read_missing_body_returns_controlled_error() {
        let ex = fixture();
        let err = ex.read("onedrive", "f1", None).unwrap_err();
        assert!(err.to_string().contains("no body onedrive/f1"));
    }

    #[test]
    fn search_marks_unreadable_body_as_unavailable() {
        let ex = fixture();
        let out: serde_json::Value =
            serde_json::from_str(&ex.search(&[], "spotify-logo", None).unwrap()).unwrap();
        assert_eq!(out["results"][0]["body_available"], false);
        assert_eq!(out["results"][0]["snippet"], "");
    }

    #[cfg(feature = "retrieval")]
    #[test]
    fn read_mail_returns_model_text_not_raw_eml_headers() {
        let eml = concat!(
            "Subject: Quarterly Report\r\n",
            "From: Ada <ada@example.com>\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n",
            "\r\n",
            "Hello from the extracted body.\r\n"
        );
        let ex = RetrievalExecutor::new(FakeArchive {
            account: "me".into(),
            items: vec![FakeArchive::item("mail", "eml1", "Quarterly", Some(eml))],
        });
        let out = ex.read("mail", "eml1", None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let content = v["content"].as_str().unwrap();
        assert!(content.contains("Quarterly Report"));
        assert!(content.contains("Hello from the extracted body."));
        assert!(!content.contains("Content-Type:"));
        assert_eq!(v["bytes_total"], content.len());
        assert_eq!(v["bytes_returned"], content.len());
        assert_eq!(v["truncated"], false);
    }

    #[test]
    fn list_reports_items_and_service_total() {
        let ex = fixture();
        let v: serde_json::Value =
            serde_json::from_str(&ex.list("mail", None, None, None).unwrap()).unwrap();
        assert_eq!(v["service_total"], 2);
        assert_eq!(v["results"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn list_pages_flat_service_items_with_count() {
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let ex = RetrievalExecutor::new(RoutingArchive::new(calls.clone()));
        let v: serde_json::Value =
            serde_json::from_str(&ex.list("onedrive", None, Some(1), Some(1)).unwrap()).unwrap();
        assert_eq!(*calls.borrow(), vec!["list_page:1:1"]);
        assert_eq!(v["service"], "onedrive");
        assert_eq!(v["parent"], serde_json::Value::Null);
        assert_eq!(v["limit"], 1);
        assert_eq!(v["offset"], 1);
        assert_eq!(v["service_total"], 123);
        assert_eq!(v["returned"], 1);
        assert_eq!(v["results"][0]["id"], "flat-1");
        assert_eq!(v["results"][0]["path"], "onedrive/flat-1.bin");
        assert_eq!(v["results"][0]["source"]["id"], "flat-1");
        assert_eq!(v["results"][0]["source"]["path"], "onedrive/flat-1.bin");
    }

    #[test]
    fn list_root_uses_roots_not_whole_service() {
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let ex = RetrievalExecutor::new(RoutingArchive::new(calls.clone()));
        let v: serde_json::Value = serde_json::from_str(
            &ex.list("onedrive", Some("root"), Some(10), Some(0))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(*calls.borrow(), vec!["roots"]);
        assert_eq!(v["parent"], "root");
        assert_eq!(v["results"][0]["id"], "root-only");
    }

    #[test]
    fn list_parent_uses_children() {
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let ex = RetrievalExecutor::new(RoutingArchive::new(calls.clone()));
        let v: serde_json::Value = serde_json::from_str(
            &ex.list("onedrive", Some("folder-1"), Some(10), Some(0))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(*calls.borrow(), vec!["children:folder-1"]);
        assert_eq!(v["parent"], "folder-1");
        assert_eq!(v["results"][0]["id"], "child-only");
    }

    #[test]
    fn list_limit_and_offset_are_applied() {
        let ex = RetrievalExecutor::new(FakeArchive {
            account: "me".into(),
            items: vec![
                FakeArchive::item("mail", "m0", "Mail 0", Some("body 0")),
                FakeArchive::item("mail", "m1", "Mail 1", Some("body 1")),
                FakeArchive::item("mail", "m2", "Mail 2", Some("body 2")),
            ],
        });
        let v: serde_json::Value =
            serde_json::from_str(&ex.list("mail", None, Some(1), Some(2)).unwrap()).unwrap();
        assert_eq!(v["limit"], 1);
        assert_eq!(v["offset"], 2);
        assert_eq!(v["service_total"], 3);
        assert_eq!(v["returned"], 1);
        assert_eq!(v["results"][0]["id"], "m2");
    }

    #[test]
    fn deep_search_still_scans_candidates_after_list_refactor() {
        let items: Vec<_> = (0..(DEFAULT_LIST_LIMIT + 5))
            .map(|i| {
                FakeArchive::item(
                    "mail",
                    &format!("bulk-{i}"),
                    &format!("Bulk {i}"),
                    Some(&format!("unmatched body {i}")),
                )
            })
            .collect();
        let ex = RetrievalExecutor::new(FakeArchive {
            account: "me".into(),
            items,
        });
        let out = ex
            .deep_search(&["mail".into()], "zzzznomatch", None, Some(40), &mut |_| {})
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["candidates_total"], DEFAULT_LIST_LIMIT + 5);
        assert_eq!(v["read"], MAX_DEEP_READS);
        assert_eq!(v["budget_reached"], true);
    }

    #[test]
    fn export_raw_passthrough_carries_source_ids() {
        let ex = fixture();
        let v: serde_json::Value = serde_json::from_str(&ex.export("mail", "m1").unwrap()).unwrap();
        assert_eq!(v["format"], "raw"); // mail → raw without the connectors feature
        assert_eq!(v["service"], "mail");
        assert_eq!(v["id"], "m1");
        assert_eq!(v["source"]["service"], "mail");
        assert_eq!(v["source"]["id"], "m1");
        assert_eq!(v["source"]["path"], "mail/m1.bin");
        assert!(v["content"].as_str().unwrap().contains("Spotify"));
    }

    #[cfg(feature = "retrieval")]
    #[test]
    fn export_calendar_returns_ics_with_source() {
        let event = serde_json::json!({
            "id": "cal-1",
            "iCalUId": "uid-1",
            "subject": "Q2 review",
            "bodyPreview": "Agenda line",
            "location": { "displayName": "Room 1" },
            "start": { "dateTime": "2026-03-01T09:00:00.0000000", "timeZone": "UTC" },
            "end": { "dateTime": "2026-03-01T10:00:00.0000000", "timeZone": "UTC" },
            "lastModifiedDateTime": "2026-02-20T08:00:00Z"
        });
        let body = event.to_string();
        let ex = RetrievalExecutor::new(FakeArchive {
            account: "me".into(),
            items: vec![FakeArchive::item(
                "calendar",
                "cal-1",
                "Q2 review",
                Some(&body),
            )],
        });
        let v: serde_json::Value =
            serde_json::from_str(&ex.export("calendar", "cal-1").unwrap()).unwrap();
        assert_eq!(v["format"], "ics");
        assert_eq!(v["source"]["service"], "calendar");
        assert_eq!(v["source"]["id"], "cal-1");
        let content = v["content"].as_str().unwrap();
        assert!(content.starts_with("BEGIN:VCALENDAR\r\nVERSION:2.0"));
        assert!(content.contains("BEGIN:VEVENT"));
        assert!(content.contains("SUMMARY:Q2 review"));
        assert!(content.contains("DTSTART:20260301T090000"));
    }

    #[cfg(feature = "retrieval")]
    #[test]
    fn export_contact_returns_vcard_with_source() {
        let contact = serde_json::json!({
            "displayName": "Ada Lovelace",
            "givenName": "Ada",
            "surname": "Lovelace",
            "emailAddresses": [{ "address": "ada@example.com", "name": "Ada" }],
            "mobilePhone": "+1 555 0100",
            "companyName": "Analytical Engines",
            "jobTitle": "Mathematician"
        });
        let body = contact.to_string();
        let ex = RetrievalExecutor::new(FakeArchive {
            account: "me".into(),
            items: vec![FakeArchive::item(
                "contacts",
                "contact-1",
                "Ada Lovelace",
                Some(&body),
            )],
        });
        let v: serde_json::Value =
            serde_json::from_str(&ex.export("contacts", "contact-1").unwrap()).unwrap();
        assert_eq!(v["format"], "vcard");
        assert_eq!(v["source"]["service"], "contacts");
        assert_eq!(v["source"]["id"], "contact-1");
        let content = v["content"].as_str().unwrap();
        assert!(content.starts_with("BEGIN:VCARD\r\nVERSION:3.0"));
        assert!(content.contains("FN:Ada Lovelace"));
        assert!(content.contains("EMAIL:ada@example.com"));
    }

    #[cfg(feature = "retrieval")]
    #[test]
    fn export_invalid_structured_body_is_tool_error() {
        let ex = RetrievalExecutor::new(FakeArchive {
            account: "me".into(),
            items: vec![FakeArchive::item(
                "calendar",
                "bad-cal",
                "Broken",
                Some("not-json"),
            )],
        });
        let err = ex.export("calendar", "bad-cal").unwrap_err();
        assert!(err.to_string().contains("export parse"));
    }

    #[cfg(feature = "retrieval")]
    #[test]
    fn store_archive_executor_covers_search_read_list_export_shapes() {
        let _guard = crate::archive::BodyKeyTestGuard::new();
        isyncyou_core::envelope::set_body_key(618_090, [90u8; 32]);

        let dir = tempfile::tempdir().unwrap();
        let store = isyncyou_store::Store::open(dir.path().join(".isyncyou-store.db")).unwrap();

        upsert_store_body(
            &store,
            dir.path(),
            isyncyou_store::Item::new("me", "mail", "m1", "Visible mail", "message"),
            "mail/aa/m1.eml",
            concat!(
                "Subject: Store runtime mail\r\n",
                "Content-Type: text/plain; charset=utf-8\r\n",
                "\r\n",
                "Hello archived mail text.\r\n"
            )
            .as_bytes(),
        );
        upsert_store_body(
            &store,
            dir.path(),
            isyncyou_store::Item::new("me", "mail", "m2", "Body only mail", "message"),
            "mail/aa/m2.eml",
            b"needle618 appears only in the indexed body",
        );
        store
            .index_body(
                "me",
                "mail",
                "m2",
                "needle618 appears only in the indexed body",
            )
            .unwrap();

        let folder = isyncyou_store::Item::new("me", "onedrive", "folder", "Folder", "folder");
        store.upsert_item(&folder).unwrap();
        let mut child = isyncyou_store::Item::new("me", "onedrive", "child", "Child.txt", "file");
        child.parent_remote_id = Some("folder".into());
        store.upsert_item(&child).unwrap();

        let event = serde_json::json!({
            "id": "cal-1",
            "iCalUId": "uid-1",
            "subject": "Store event",
            "start": { "dateTime": "2026-03-01T09:00:00.0000000", "timeZone": "UTC" },
            "end": { "dateTime": "2026-03-01T10:00:00.0000000", "timeZone": "UTC" }
        })
        .to_string();
        upsert_store_body(
            &store,
            dir.path(),
            isyncyou_store::Item::new("me", "calendar", "cal-1", "Store event", "event"),
            "calendar/cal-1.json",
            event.as_bytes(),
        );

        let contact = serde_json::json!({
            "displayName": "Ada Lovelace",
            "givenName": "Ada",
            "surname": "Lovelace",
            "emailAddresses": [{ "address": "ada@example.com" }]
        })
        .to_string();
        upsert_store_body(
            &store,
            dir.path(),
            isyncyou_store::Item::new("me", "contacts", "contact-1", "Ada Lovelace", "contact"),
            "contacts/contact-1.json",
            contact.as_bytes(),
        );
        drop(store);

        let ex = RetrievalExecutor::new(crate::archive::StoreArchive::new("me", dir.path()));

        let search = ToolAction::Search {
            account: "me".into(),
            services: vec!["mail".into()],
            query: "needle618".into(),
            limit: Some(10),
        };
        let search_out: serde_json::Value =
            serde_json::from_str(&ex.execute_read(&search).unwrap()).unwrap();
        assert_eq!(search_out["results"][0]["id"], "m2");
        assert_eq!(search_out["results"][0]["source"]["service"], "mail");
        assert!(search_out["results"][0]["snippet"]
            .as_str()
            .unwrap()
            .contains("needle618"));

        let read = ToolAction::Read {
            account: "me".into(),
            service: "mail".into(),
            id: "m1".into(),
            max_bytes: None,
        };
        let read_out: serde_json::Value =
            serde_json::from_str(&ex.execute_read(&read).unwrap()).unwrap();
        assert_eq!(read_out["content_kind"], "mail-text");
        assert_eq!(read_out["source"]["path"], "mail/aa/m1.eml");
        assert!(read_out["content"]
            .as_str()
            .unwrap()
            .contains("Hello archived mail text."));
        assert!(!read_out["content"]
            .as_str()
            .unwrap()
            .contains("Content-Type:"));

        let list_root = ToolAction::List {
            account: "me".into(),
            service: "onedrive".into(),
            parent: Some("root".into()),
            limit: Some(10),
            offset: Some(0),
        };
        let root_out: serde_json::Value =
            serde_json::from_str(&ex.execute_read(&list_root).unwrap()).unwrap();
        assert_eq!(root_out["results"][0]["id"], "folder");
        assert_eq!(root_out["results"][0]["source"]["id"], "folder");

        let list_child = ToolAction::List {
            account: "me".into(),
            service: "onedrive".into(),
            parent: Some("folder".into()),
            limit: Some(10),
            offset: Some(0),
        };
        let child_out: serde_json::Value =
            serde_json::from_str(&ex.execute_read(&list_child).unwrap()).unwrap();
        assert_eq!(child_out["results"][0]["id"], "child");

        let calendar = ToolAction::Export {
            account: "me".into(),
            service: "calendar".into(),
            id: "cal-1".into(),
        };
        let calendar_out: serde_json::Value =
            serde_json::from_str(&ex.execute_read(&calendar).unwrap()).unwrap();
        assert_eq!(calendar_out["format"], "ics");
        assert!(calendar_out["content"]
            .as_str()
            .unwrap()
            .contains("BEGIN:VCALENDAR"));

        let contacts = ToolAction::Export {
            account: "me".into(),
            service: "contacts".into(),
            id: "contact-1".into(),
        };
        let contacts_out: serde_json::Value =
            serde_json::from_str(&ex.execute_read(&contacts).unwrap()).unwrap();
        assert_eq!(contacts_out["format"], "vcard");
        assert!(contacts_out["content"]
            .as_str()
            .unwrap()
            .contains("EMAIL:ada@example.com"));
    }

    #[test]
    fn destructive_action_is_refused_by_the_read_executor() {
        let ex = fixture();
        let backup = ToolAction::Backup {
            account: "me".into(),
            services: vec![],
        };
        let err = ex.execute_read(&backup).unwrap_err();
        assert!(err.to_string().contains("destructive"));
    }

    #[test]
    fn execute_read_dispatches_search_via_tooaction() {
        let ex = fixture();
        let action = ToolAction::Search {
            account: "me".into(),
            services: vec![],
            query: "spotify".into(),
            limit: None,
        };
        let out = ex.execute_read(&action).unwrap();
        assert!(out.contains("total_matches"));
    }

    #[test]
    fn execute_read_rejects_account_mismatch() {
        let ex = fixture();
        let action = ToolAction::Search {
            account: "other".into(),
            services: vec![],
            query: "spotify".into(),
            limit: None,
        };
        let err = ex.execute_read(&action).unwrap_err();
        assert!(err.to_string().contains("account mismatch"));
    }
}
