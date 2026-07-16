//! Allowlist-only reducer for private local-client drift captures (#627).
//!
//! Raw inputs stay under `/tmp`. This module emits only fixed schema keys,
//! allowlisted names, counters, booleans, and a normalized client version. It never
//! serializes arbitrary input values, prompt/response text, identifiers, or headers.

use reqwest::Url;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_VERSION_BYTES: u64 = 4 * 1024;
const MAX_EVENT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_DEBUG_BYTES: u64 = 4 * 1024 * 1024;
const MAX_MODEL_BYTES: u64 = 8 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 256 * 1024;
const SCOPE: &str = "local_cli_drift_only_not_product_auth";

// These are the exact headers present on the provider request at the wire boundary.
// Keep them aligned with the provider builders: Codex's HTTP client adds content-type
// through `.json()`, while every other entry is emitted by `request_headers()`.
const CLAUDE_REQUIRED_WIRE_HEADERS: &[&str] = &[
    "accept",
    "anthropic-beta",
    "anthropic-dangerous-direct-browser-access",
    "anthropic-version",
    "authorization",
    "content-type",
    "user-agent",
    "x-app",
    "x-claude-code-session-id",
];
const CODEX_REQUIRED_WIRE_HEADERS: &[&str] = &[
    "accept",
    "authorization",
    "chatgpt-account-id",
    "content-type",
    "originator",
    "user-agent",
];
const TRANSPORT_HEADERS: &[&str] = &["accept-encoding", "connection", "content-length", "host"];

const SHARED_USAGE_FIELDS: &[&str] = &["input_tokens", "output_tokens"];
const CLAUDE_USAGE_FIELDS: &[&str] = &[
    "cache_creation_input_tokens",
    "cache_read_input_tokens",
    "service_tier",
];
// Safely observed in the earlier reduced #623 CLI capture. These are client
// diagnostics, not fields consumed by the iSyncYou streaming/usage contract. Keep
// discarding the entire value; any other usage key still forces manual review.
const CLAUDE_IGNORED_USAGE_FIELDS: &[&str] = &[
    "cache_creation",
    "ephemeral_1h_input_tokens",
    "fast",
    "inference_geo",
    "iterations",
    "output_tokens_details",
    "server_tool_use",
    "speed",
    "web_search_requests",
];
const CLAUDE_RATE_LIMIT_FIELDS: &[&str] = &[
    "rateLimitType",
    "status",
    "resetsAt",
    "isUsingOverage",
    "overageStatus",
    "overageDisabledReason",
];
const CODEX_USAGE_FIELDS: &[&str] = &["cached_input_tokens", "reasoning_output_tokens"];
const CLAUDE_ASSISTANT_FIELDS: &[&str] = &[
    "message",
    "parent_tool_use_id",
    "request_id",
    "session_id",
    "type",
    "uuid",
];
const CLAUDE_RATE_EVENT_FIELDS: &[&str] = &["rate_limit_info", "session_id", "type", "uuid"];
const CLAUDE_RESULT_FIELDS: &[&str] = &[
    "api_error_status",
    "duration_api_ms",
    "duration_ms",
    "fast_mode_state",
    "is_error",
    "modelUsage",
    "num_turns",
    "permission_denials",
    "result",
    "session_id",
    "stop_reason",
    "subtype",
    "terminal_reason",
    "time_to_request_ms",
    "total_cost_usd",
    "ttft_ms",
    "ttft_stream_ms",
    "type",
    "usage",
    "uuid",
];
const CLAUDE_STREAM_FIELDS: &[&str] = &[
    "event",
    "parent_tool_use_id",
    "session_id",
    "ttft_ms",
    "type",
    "uuid",
];
const CLAUDE_SYSTEM_FIELDS: &[&str] = &[
    "agents",
    "analytics_disabled",
    "apiKeySource",
    "capabilities",
    "claude_code_version",
    "cwd",
    "exit_code",
    "fast_mode_state",
    "hook_event",
    "hook_id",
    "hook_name",
    "mcp_servers",
    "memory_paths",
    "model",
    "outcome",
    "output",
    "output_style",
    "permissionMode",
    "plugins",
    "product_feedback_disabled",
    "session_id",
    "skills",
    "slash_commands",
    "status",
    "stderr",
    "stdout",
    "subtype",
    "tools",
    "type",
    "uuid",
];
const CODEX_ITEM_FIELDS: &[&str] = &["item", "type"];
const CODEX_THREAD_FIELDS: &[&str] = &["thread_id", "type"];
const CODEX_TURN_COMPLETED_FIELDS: &[&str] = &["type", "usage"];
const CODEX_TURN_STARTED_FIELDS: &[&str] = &["type"];

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureProvider {
    Claude,
    Codex,
}

impl CaptureProvider {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    fn compatibility_version(self) -> &'static str {
        match self {
            Self::Claude => crate::provider::subscription::DEFAULT_CLI_VERSION,
            Self::Codex => crate::provider::codex::DEFAULT_CLI_VERSION,
        }
    }

    fn event_type(self, value: &str) -> Option<&'static str> {
        match self {
            Self::Claude => match value {
                "system" => Some("system"),
                "assistant" => Some("assistant"),
                "stream_event" => Some("stream_event"),
                "rate_limit_event" => Some("rate_limit_event"),
                "result" => Some("result"),
                _ => None,
            },
            Self::Codex => match value {
                "thread.started" => Some("thread.started"),
                "turn.started" => Some("turn.started"),
                "item.completed" => Some("item.completed"),
                "turn.completed" => Some("turn.completed"),
                _ => None,
            },
        }
    }

    fn usage_field(self, value: &str) -> Option<&'static str> {
        if let Some(value) = SHARED_USAGE_FIELDS
            .iter()
            .copied()
            .find(|allowed| *allowed == value)
        {
            return Some(value);
        }
        let provider_fields = match self {
            Self::Claude => CLAUDE_USAGE_FIELDS,
            Self::Codex => CODEX_USAGE_FIELDS,
        };
        provider_fields
            .iter()
            .copied()
            .find(|allowed| *allowed == value)
    }

    fn ignored_usage_field(self, value: &str) -> bool {
        self == Self::Claude && CLAUDE_IGNORED_USAGE_FIELDS.contains(&value)
    }

    fn event_fields(self, event_type: &str) -> &'static [&'static str] {
        match (self, event_type) {
            (Self::Claude, "assistant") => CLAUDE_ASSISTANT_FIELDS,
            (Self::Claude, "rate_limit_event") => CLAUDE_RATE_EVENT_FIELDS,
            (Self::Claude, "result") => CLAUDE_RESULT_FIELDS,
            (Self::Claude, "stream_event") => CLAUDE_STREAM_FIELDS,
            (Self::Claude, "system") => CLAUDE_SYSTEM_FIELDS,
            (Self::Codex, "item.completed") => CODEX_ITEM_FIELDS,
            (Self::Codex, "thread.started") => CODEX_THREAD_FIELDS,
            (Self::Codex, "turn.completed") => CODEX_TURN_COMPLETED_FIELDS,
            (Self::Codex, "turn.started") => CODEX_TURN_STARTED_FIELDS,
            _ => &[],
        }
    }
}

#[derive(Debug, Clone)]
pub struct DriftCaptureOptions {
    pub provider: CaptureProvider,
    pub version_file: PathBuf,
    pub event_file: PathBuf,
    pub debug_file: Option<PathBuf>,
    pub model_catalog: Option<PathBuf>,
    pub bundled_model_catalog: Option<PathBuf>,
    pub expected_sentinel: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftCaptureError {
    provider: &'static str,
    role: &'static str,
    line: Option<usize>,
    reason: &'static str,
}

impl DriftCaptureError {
    fn new(
        provider: CaptureProvider,
        role: &'static str,
        line: Option<usize>,
        reason: &'static str,
    ) -> Self {
        Self {
            provider: provider.name(),
            role,
            line,
            reason,
        }
    }
}

impl std::fmt::Display for DriftCaptureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} drift capture {} failed",
            self.provider, self.role
        )?;
        if let Some(line) = self.line {
            write!(formatter, " at line {line}")?;
        }
        write!(formatter, ": {}", self.reason)
    }
}

impl std::error::Error for DriftCaptureError {}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DriftDecision {
    NoDrift,
    ImplementationUpdateRequired,
    NotSafelyObservable,
}

#[derive(Debug, Serialize)]
pub struct DriftSummary {
    pub schema_version: u8,
    pub scope: &'static str,
    pub product_auth_evidence: bool,
    pub client: ClientSummary,
    pub controlled_sentinel_observed: bool,
    pub event_type_counts: BTreeMap<&'static str, u64>,
    pub usage_fields: Vec<&'static str>,
    pub rate_limit_fields: Vec<&'static str>,
    pub identifiers: IdentifierVisibility,
    pub wire: WireVisibility,
    pub model_catalog: Option<ModelCatalogSummary>,
    pub drift_decision: DriftDecision,
    pub raw_retained: bool,
    #[serde(skip)]
    review: ReviewMarkers,
}

impl DriftSummary {
    pub fn manual_review_categories(&self) -> Vec<&'static str> {
        self.review.categories()
    }
}

#[derive(Debug, Serialize)]
pub struct ClientSummary {
    pub name: &'static str,
    pub version: String,
}

#[derive(Debug, Default, Serialize)]
pub struct IdentifierVisibility {
    pub request_id_present: bool,
    pub session_id_present: bool,
    pub thread_id_present: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct HeaderVisibility {
    pub authorization_present: bool,
    pub billing_identity_present: bool,
    pub account_identity_present: bool,
    pub client_identity_present: bool,
    pub protocol_version_present: bool,
    pub stream_accept_present: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct WireVisibility {
    pub endpoint_observed: bool,
    pub headers_observed: bool,
    pub provider_hosts: Vec<&'static str>,
    pub path_categories: Vec<&'static str>,
    pub headers: HeaderVisibility,
}

#[derive(Debug, Default, Serialize)]
pub struct ModelCatalogSummary {
    pub models_count: u64,
    pub bundled_models_count: u64,
    pub service_tiers_present: bool,
    pub input_modalities_present: bool,
    pub supports_parallel_tool_calls_present: bool,
}

#[derive(Default)]
struct ReductionState {
    counts: BTreeMap<&'static str, u64>,
    usage_fields: BTreeSet<&'static str>,
    rate_limit_fields: BTreeSet<&'static str>,
    identifiers: IdentifierVisibility,
    hosts: BTreeSet<&'static str>,
    paths: BTreeSet<&'static str>,
    headers: HeaderVisibility,
    exact_headers: BTreeSet<&'static str>,
    claude_billing_block_observed: bool,
    sentinel_observed: bool,
    structured_debug_observed: bool,
    review: ReviewMarkers,
}

#[derive(Debug, Default)]
struct ReviewMarkers {
    version: bool,
    event_type: bool,
    event_field: bool,
    usage_field: bool,
    rate_limit_field: bool,
    debug_shape: bool,
    endpoint: bool,
    header: bool,
}

impl ReviewMarkers {
    fn requires_update(&self) -> bool {
        !self.categories().is_empty()
    }

    fn categories(&self) -> Vec<&'static str> {
        [
            (self.version, "version"),
            (self.event_type, "event_type"),
            (self.event_field, "event_field"),
            (self.usage_field, "usage_field"),
            (self.rate_limit_field, "rate_limit_field"),
            (self.debug_shape, "debug_shape"),
            (self.endpoint, "endpoint"),
            (self.header, "header"),
        ]
        .into_iter()
        .filter_map(|(present, category)| present.then_some(category))
        .collect()
    }
}

pub fn reduce_capture(options: &DriftCaptureOptions) -> Result<DriftSummary, DriftCaptureError> {
    if options.expected_sentinel.is_empty() || options.expected_sentinel.len() > 256 {
        return Err(DriftCaptureError::new(
            options.provider,
            "sentinel",
            None,
            "invalid sentinel",
        ));
    }
    let version_raw = read_bounded(
        options.provider,
        "version",
        &options.version_file,
        MAX_VERSION_BYTES,
    )?;
    let version = normalize_version(options.provider, &version_raw)?;
    let events = read_bounded(
        options.provider,
        "events",
        &options.event_file,
        MAX_EVENT_BYTES,
    )?;
    let mut state = ReductionState::default();
    inspect_events(options, &events, &mut state)?;

    if let Some(path) = &options.debug_file {
        let debug = read_bounded(options.provider, "debug", path, MAX_DEBUG_BYTES)?;
        inspect_debug(options.provider, &debug, &mut state);
    }

    let model_catalog = inspect_model_catalogs(options)?;
    let headers_observed = complete_wire_identity_observed(options.provider, &state);
    let endpoint_observed = expected_endpoint_observed(options.provider, &state);
    state.review.version = version != options.provider.compatibility_version();
    let drift_decision = if state.review.requires_update() {
        DriftDecision::ImplementationUpdateRequired
    } else if !state.structured_debug_observed || !headers_observed || !endpoint_observed {
        DriftDecision::NotSafelyObservable
    } else {
        DriftDecision::NoDrift
    };

    Ok(DriftSummary {
        schema_version: 1,
        scope: SCOPE,
        product_auth_evidence: false,
        client: ClientSummary {
            name: options.provider.name(),
            version,
        },
        controlled_sentinel_observed: state.sentinel_observed,
        event_type_counts: state.counts,
        usage_fields: state.usage_fields.into_iter().collect(),
        rate_limit_fields: state.rate_limit_fields.into_iter().collect(),
        identifiers: state.identifiers,
        wire: WireVisibility {
            endpoint_observed,
            headers_observed,
            provider_hosts: state.hosts.into_iter().collect(),
            path_categories: state.paths.into_iter().collect(),
            headers: state.headers,
        },
        model_catalog,
        drift_decision,
        raw_retained: false,
        review: state.review,
    })
}

fn required_wire_headers(provider: CaptureProvider) -> &'static [&'static str] {
    match provider {
        CaptureProvider::Claude => CLAUDE_REQUIRED_WIRE_HEADERS,
        CaptureProvider::Codex => CODEX_REQUIRED_WIRE_HEADERS,
    }
}

fn complete_wire_identity_observed(provider: CaptureProvider, state: &ReductionState) -> bool {
    let headers_complete = required_wire_headers(provider)
        .iter()
        .all(|header| state.exact_headers.contains(header));
    headers_complete
        && match provider {
            CaptureProvider::Claude => state.claude_billing_block_observed,
            CaptureProvider::Codex => true,
        }
}

fn expected_endpoint_observed(provider: CaptureProvider, state: &ReductionState) -> bool {
    match provider {
        CaptureProvider::Claude => {
            state.hosts.contains("api.anthropic.com") && state.paths.contains("messages")
        }
        CaptureProvider::Codex => {
            state.hosts.contains("chatgpt.com") && state.paths.contains("codex_responses")
        }
    }
}

pub fn write_summary_atomic(
    provider: CaptureProvider,
    summary: &DriftSummary,
    expected_sentinel: &str,
    output: &Path,
) -> Result<(), DriftCaptureError> {
    let bytes = serde_json::to_vec_pretty(summary)
        .map_err(|_| DriftCaptureError::new(provider, "output", None, "serialization failed"))?;
    validate_serialized_summary(provider, &bytes, expected_sentinel)?;
    let parent = output
        .parent()
        .ok_or_else(|| DriftCaptureError::new(provider, "output", None, "invalid destination"))?;
    fs::create_dir_all(parent)
        .map_err(|_| DriftCaptureError::new(provider, "output", None, "destination unavailable"))?;
    let name = output
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| DriftCaptureError::new(provider, "output", None, "invalid destination"))?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".{name}.tmp-{}-{sequence}", std::process::id()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp).map_err(|_| {
            DriftCaptureError::new(provider, "output", None, "temporary write failed")
        })?;
        file.write_all(&bytes).map_err(|_| {
            DriftCaptureError::new(provider, "output", None, "temporary write failed")
        })?;
        file.write_all(b"\n").map_err(|_| {
            DriftCaptureError::new(provider, "output", None, "temporary write failed")
        })?;
        file.sync_all().map_err(|_| {
            DriftCaptureError::new(provider, "output", None, "temporary sync failed")
        })?;
        fs::rename(&temp, output).map_err(|_| {
            DriftCaptureError::new(provider, "output", None, "atomic replace failed")
        })?;
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn read_bounded(
    provider: CaptureProvider,
    role: &'static str,
    path: &Path,
    max_bytes: u64,
) -> Result<String, DriftCaptureError> {
    let link_metadata = fs::symlink_metadata(path)
        .map_err(|_| DriftCaptureError::new(provider, role, None, "input unavailable"))?;
    if link_metadata.file_type().is_symlink() {
        return Err(DriftCaptureError::new(
            provider,
            role,
            None,
            "symlink input rejected",
        ));
    }
    let canonical = fs::canonicalize(path)
        .map_err(|_| DriftCaptureError::new(provider, role, None, "input unavailable"))?;
    if !canonical.starts_with("/tmp") {
        return Err(DriftCaptureError::new(
            provider,
            role,
            None,
            "input outside temporary root",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|_| DriftCaptureError::new(provider, role, None, "input unavailable"))?;
    let metadata = file
        .metadata()
        .map_err(|_| DriftCaptureError::new(provider, role, None, "input unavailable"))?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return Err(DriftCaptureError::new(
            provider,
            role,
            None,
            "input rejected",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.dev() != link_metadata.dev() || metadata.ino() != link_metadata.ino() {
            return Err(DriftCaptureError::new(
                provider,
                role,
                None,
                "input identity changed",
            ));
        }
        if metadata.mode() & 0o077 != 0 {
            return Err(DriftCaptureError::new(
                provider,
                role,
                None,
                "input permissions rejected",
            ));
        }
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| DriftCaptureError::new(provider, role, None, "input read failed"))?;
    if bytes.len() as u64 > max_bytes {
        return Err(DriftCaptureError::new(
            provider,
            role,
            None,
            "input rejected",
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| DriftCaptureError::new(provider, role, None, "input is not UTF-8"))
}

fn normalize_version(provider: CaptureProvider, raw: &str) -> Result<String, DriftCaptureError> {
    let candidates = raw.split_whitespace().filter_map(|token| {
        let candidate =
            token.trim_matches(|character: char| !(character.is_ascii_digit() || character == '.'));
        let parts = candidate.split('.').collect::<Vec<_>>();
        (parts.len() == 3
            && parts
                .iter()
                .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit())))
        .then(|| candidate.to_string())
    });
    let mut candidates = candidates.collect::<BTreeSet<_>>();
    if candidates.len() != 1 {
        return Err(DriftCaptureError::new(
            provider,
            "version",
            None,
            "version not recognized",
        ));
    }
    Ok(candidates.pop_first().expect("one candidate"))
}

fn inspect_events(
    options: &DriftCaptureOptions,
    raw: &str,
    state: &mut ReductionState,
) -> Result<(), DriftCaptureError> {
    for (index, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        if line.len() > MAX_LINE_BYTES {
            return Err(DriftCaptureError::new(
                options.provider,
                "events",
                Some(index + 1),
                "line too long",
            ));
        }
        let value: Value = serde_json::from_str(line).map_err(|_| {
            DriftCaptureError::new(options.provider, "events", Some(index + 1), "invalid JSON")
        })?;
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .and_then(|value| options.provider.event_type(value))
            .unwrap_or("unknown");
        if event_type == "unknown" {
            state.review.event_type = true;
        }
        if let Some(object) = value.as_object() {
            let allowed = options.provider.event_fields(event_type);
            if object
                .keys()
                .any(|field| !allowed.contains(&field.as_str()))
            {
                state.review.event_field = true;
            }
        } else {
            state.review.event_field = true;
        }
        *state.counts.entry(event_type).or_default() += 1;
        inspect_event_value(options.provider, &value, &options.expected_sentinel, state);
    }
    Ok(())
}

fn inspect_event_value(
    provider: CaptureProvider,
    value: &Value,
    sentinel: &str,
    state: &mut ReductionState,
) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                match key.as_str() {
                    "request_id" => state.identifiers.request_id_present |= !value.is_null(),
                    "session_id" => state.identifiers.session_id_present |= !value.is_null(),
                    "thread_id" => state.identifiers.thread_id_present |= !value.is_null(),
                    "usage" => inspect_usage(provider, value, state),
                    // Claude's aggregate per-model billing object contains model names
                    // and cost diagnostics. It is known from the reduced #623 capture,
                    // is not part of iSyncYou Usage, and is discarded as one unit.
                    "modelUsage" => {}
                    "rate_limit_info" => inspect_rate_limit(value, state),
                    _ => {}
                }
                inspect_event_value(provider, value, sentinel, state);
            }
        }
        Value::Array(values) => {
            for value in values {
                inspect_event_value(provider, value, sentinel, state);
            }
        }
        Value::String(value) => state.sentinel_observed |= value.contains(sentinel),
        _ => {}
    }
}

fn inspect_usage(provider: CaptureProvider, value: &Value, state: &mut ReductionState) {
    let Some(object) = value.as_object() else {
        state.review.usage_field = true;
        return;
    };
    for key in object.keys() {
        if let Some(field) = provider.usage_field(key) {
            state.usage_fields.insert(field);
        } else if provider.ignored_usage_field(key) {
            // Intentionally ignored as a whole; nested diagnostic values must not be
            // inspected or copied into the public summary.
        } else {
            state.review.usage_field = true;
        }
    }
}

fn inspect_rate_limit(value: &Value, state: &mut ReductionState) {
    let Some(object) = value.as_object() else {
        state.review.rate_limit_field = true;
        return;
    };
    for key in object.keys() {
        if let Some(field) = CLAUDE_RATE_LIMIT_FIELDS
            .iter()
            .copied()
            .find(|allowed| *allowed == key)
        {
            state.rate_limit_fields.insert(field);
        } else {
            state.review.rate_limit_field = true;
        }
    }
}

fn inspect_debug(provider: CaptureProvider, raw: &str, state: &mut ReductionState) {
    if let Ok(value) = serde_json::from_str::<Value>(raw) {
        state.structured_debug_observed = true;
        inspect_debug_value(provider, &value, state);
        return;
    }
    let mut parsed_any = false;
    for line in raw.lines() {
        if line.len() > MAX_LINE_BYTES {
            state.review.debug_shape = true;
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            parsed_any = true;
            inspect_debug_value(provider, &value, state);
        }
    }
    state.structured_debug_observed = parsed_any;
}

fn inspect_debug_value(provider: CaptureProvider, value: &Value, state: &mut ReductionState) {
    match value {
        Value::Array(values) => {
            for value in values {
                inspect_debug_value(provider, value, state);
            }
        }
        Value::Object(object) => {
            inspect_request_object(provider, object, state);
            for key in ["request", "http_request"] {
                if let Some(request) = object.get(key).and_then(Value::as_object) {
                    inspect_request_object(provider, request, state);
                }
            }
        }
        _ => {}
    }
}

fn inspect_request_object(
    provider: CaptureProvider,
    object: &serde_json::Map<String, Value>,
    state: &mut ReductionState,
) {
    if let Some(url) = object.get("url").and_then(Value::as_str) {
        inspect_url(provider, url, state);
    }
    if let Some(headers) = object.get("headers").and_then(Value::as_object) {
        for header in headers.keys() {
            inspect_header(provider, header, state);
        }
    }
    if provider == CaptureProvider::Claude {
        inspect_claude_billing_block(object, state);
        for key in ["body", "json"] {
            if let Some(body) = object.get(key).and_then(Value::as_object) {
                inspect_claude_billing_block(body, state);
            }
        }
    }
}

fn inspect_url(provider: CaptureProvider, raw: &str, state: &mut ReductionState) {
    let Ok(url) = Url::parse(raw) else {
        state.review.endpoint = true;
        return;
    };
    if url.scheme() != "https" {
        state.review.endpoint = true;
        return;
    }
    let Some(host) = url.host_str() else {
        state.review.endpoint = true;
        return;
    };
    let host = match host {
        "api.anthropic.com" => "api.anthropic.com",
        "chatgpt.com" => "chatgpt.com",
        _ => {
            state.review.endpoint = true;
            return;
        }
    };
    state.hosts.insert(host);
    let path = match (provider, url.path()) {
        (CaptureProvider::Claude, "/v1/messages") => "messages",
        (CaptureProvider::Codex, "/backend-api/codex/responses") => "codex_responses",
        (CaptureProvider::Codex, path) if path.contains("models") => "model_catalog",
        _ => {
            state.review.endpoint = true;
            return;
        }
    };
    state.paths.insert(path);
}

fn inspect_header(provider: CaptureProvider, header: &str, state: &mut ReductionState) {
    let normalized = header.to_ascii_lowercase();
    if TRANSPORT_HEADERS.contains(&normalized.as_str()) {
        return;
    }
    let Some(required) = required_wire_headers(provider)
        .iter()
        .copied()
        .find(|required| *required == normalized.as_str())
    else {
        state.review.header = true;
        return;
    };
    state.exact_headers.insert(required);
    match required {
        "authorization" => state.headers.authorization_present = true,
        "chatgpt-account-id" => state.headers.account_identity_present = true,
        "user-agent" | "x-app" | "x-claude-code-session-id" | "originator" => {
            state.headers.client_identity_present = true;
        }
        "anthropic-version"
        | "anthropic-beta"
        | "anthropic-dangerous-direct-browser-access"
        | "content-type" => state.headers.protocol_version_present = true,
        "accept" => state.headers.stream_accept_present = true,
        _ => {}
    }
}

fn inspect_claude_billing_block(
    object: &serde_json::Map<String, Value>,
    state: &mut ReductionState,
) {
    let Some(system) = object.get("system") else {
        return;
    };
    let Some(blocks) = system.as_array() else {
        state.review.debug_shape = true;
        return;
    };
    let Some(first_text) = blocks
        .first()
        .and_then(Value::as_object)
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
    else {
        state.review.debug_shape = true;
        return;
    };
    let expected = format!(
        "x-anthropic-billing-header: cc_version={}.cab; cc_entrypoint=sdk-cli; cch=00000;",
        CaptureProvider::Claude.compatibility_version()
    );
    if first_text == expected {
        state.claude_billing_block_observed = true;
        state.headers.billing_identity_present = true;
    } else {
        state.review.header = true;
    }
}

fn inspect_model_catalogs(
    options: &DriftCaptureOptions,
) -> Result<Option<ModelCatalogSummary>, DriftCaptureError> {
    if options.model_catalog.is_none() && options.bundled_model_catalog.is_none() {
        return Ok(None);
    }
    let mut summary = ModelCatalogSummary::default();
    if let Some(path) = &options.model_catalog {
        let raw = read_bounded(options.provider, "model_catalog", path, MAX_MODEL_BYTES)?;
        let value: Value = serde_json::from_str(&raw).map_err(|_| {
            DriftCaptureError::new(options.provider, "model_catalog", None, "invalid JSON")
        })?;
        let models = model_entries(&value).ok_or_else(|| {
            DriftCaptureError::new(
                options.provider,
                "model_catalog",
                None,
                "unsupported schema",
            )
        })?;
        summary.models_count = models.len() as u64;
        inspect_model_fields(models, &mut summary);
    }
    if let Some(path) = &options.bundled_model_catalog {
        let raw = read_bounded(
            options.provider,
            "bundled_model_catalog",
            path,
            MAX_MODEL_BYTES,
        )?;
        let value: Value = serde_json::from_str(&raw).map_err(|_| {
            DriftCaptureError::new(
                options.provider,
                "bundled_model_catalog",
                None,
                "invalid JSON",
            )
        })?;
        let models = model_entries(&value).ok_or_else(|| {
            DriftCaptureError::new(
                options.provider,
                "bundled_model_catalog",
                None,
                "unsupported schema",
            )
        })?;
        summary.bundled_models_count = models.len() as u64;
        inspect_model_fields(models, &mut summary);
    }
    Ok(Some(summary))
}

fn model_entries(value: &Value) -> Option<&Vec<Value>> {
    value.as_array().or_else(|| {
        value
            .get("models")
            .or_else(|| value.get("data"))
            .and_then(Value::as_array)
    })
}

fn inspect_model_fields(models: &[Value], summary: &mut ModelCatalogSummary) {
    for model in models.iter().filter_map(Value::as_object) {
        summary.service_tiers_present |= model.contains_key("service_tiers");
        summary.input_modalities_present |= model.contains_key("input_modalities");
        summary.supports_parallel_tool_calls_present |=
            model.contains_key("supports_parallel_tool_calls");
    }
}

fn validate_serialized_summary(
    provider: CaptureProvider,
    bytes: &[u8],
    expected_sentinel: &str,
) -> Result<(), DriftCaptureError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| DriftCaptureError::new(provider, "output", None, "serialization failed"))?;
    let lower = text.to_ascii_lowercase();
    let forbidden = [
        "bearer ",
        "access_token",
        "refresh_token",
        "\"code\":",
        "\"state\":",
        "/home/",
        "/users/",
    ];
    if text.contains(expected_sentinel)
        || text.contains('@')
        || forbidden.iter().any(|needle| lower.contains(needle))
    {
        return Err(DriftCaptureError::new(
            provider,
            "output",
            None,
            "self-scan rejected summary",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn fixture_dir() -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix("isyncyou-627-reducer-")
            .tempdir_in("/tmp")
            .unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn write_private(path: &Path, value: &str) {
        fs::write(path, value).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn claude_options(dir: &Path, events: &str) -> DriftCaptureOptions {
        let version = dir.join("version.txt");
        let event_file = dir.join("events.jsonl");
        write_private(
            &version,
            &format!(
                "{} (Claude Code)\n",
                CaptureProvider::Claude.compatibility_version()
            ),
        );
        write_private(&event_file, events);
        DriftCaptureOptions {
            provider: CaptureProvider::Claude,
            version_file: version,
            event_file,
            debug_file: None,
            model_catalog: None,
            bundled_model_catalog: None,
            expected_sentinel: "issue-627-controlled-sentinel".into(),
        }
    }

    fn serialized(summary: &DriftSummary) -> String {
        serde_json::to_string(summary).unwrap()
    }

    fn complete_claude_debug() -> Value {
        serde_json::json!({
            "request": {
                "url": "https://api.anthropic.com/v1/messages",
                "headers": CLAUDE_REQUIRED_WIRE_HEADERS
                    .iter()
                    .map(|header| ((*header).to_string(), Value::String("redacted".into())))
                    .collect::<serde_json::Map<String, Value>>(),
                "body": {
                    "system": [{
                        "type": "text",
                        "text": format!(
                            "x-anthropic-billing-header: cc_version={}.cab; cc_entrypoint=sdk-cli; cch=00000;",
                            CaptureProvider::Claude.compatibility_version()
                        )
                    }]
                }
            }
        })
    }

    fn complete_codex_debug() -> Value {
        serde_json::json!({
            "request": {
                "url": "https://chatgpt.com/backend-api/codex/responses",
                "headers": CODEX_REQUIRED_WIRE_HEADERS
                    .iter()
                    .map(|header| ((*header).to_string(), Value::String("redacted".into())))
                    .collect::<serde_json::Map<String, Value>>()
            }
        })
    }

    fn options_with_debug(
        dir: &Path,
        provider: CaptureProvider,
        debug: &Value,
    ) -> DriftCaptureOptions {
        let version = dir.join(format!("{}-version.txt", provider.name()));
        let events = dir.join(format!("{}-events.jsonl", provider.name()));
        let debug_file = dir.join(format!("{}-debug.json", provider.name()));
        let (version_text, event_text) = match provider {
            CaptureProvider::Claude => (
                format!(
                    "{} (Claude Code)\n",
                    CaptureProvider::Claude.compatibility_version()
                ),
                r#"{"type":"assistant","message":{"text":"issue-627-controlled-sentinel"}}"#,
            ),
            CaptureProvider::Codex => (
                format!(
                    "codex-cli {}\n",
                    CaptureProvider::Codex.compatibility_version()
                ),
                r#"{"type":"item.completed","item":{"text":"issue-627-controlled-sentinel"}}"#,
            ),
        };
        write_private(&version, &version_text);
        write_private(&events, event_text);
        write_private(&debug_file, &serde_json::to_string(debug).unwrap());
        DriftCaptureOptions {
            provider,
            version_file: version,
            event_file: events,
            debug_file: Some(debug_file),
            model_catalog: None,
            bundled_model_catalog: None,
            expected_sentinel: "issue-627-controlled-sentinel".into(),
        }
    }

    #[test]
    fn experimental_capture_redacts_authorization_and_account_ids() {
        let dir = fixture_dir();
        let mut options = claude_options(
            dir.path(),
            r#"{"type":"assistant","message":{"text":"issue-627-controlled-sentinel"}}"#,
        );
        let debug = dir.path().join("debug.jsonl");
        write_private(
            &debug,
            r#"{"request":{"url":"https://api.anthropic.com/v1/messages","headers":{"authorization":"Bearer token-secret","chatgpt-account-id":"account-secret","accept":"text/event-stream"}}}"#,
        );
        options.debug_file = Some(debug);

        let text = serialized(&reduce_capture(&options).unwrap());

        assert!(!text.contains("token-secret"));
        assert!(!text.contains("account-secret"));
        assert!(text.contains("authorization_present"));
        assert!(text.contains("account_identity_present"));
    }

    #[test]
    fn experimental_capture_does_not_copy_prompt_response_or_identifier_values() {
        let dir = fixture_dir();
        let events = r#"{"type":"assistant","request_id":"request-secret","session_id":"session-secret","thread_id":"thread-secret","message":{"prompt":"prompt-secret","response":"response-secret","text":"issue-627-controlled-sentinel"}}"#;
        let summary = reduce_capture(&claude_options(dir.path(), events)).unwrap();
        let text = serialized(&summary);

        for forbidden in [
            "request-secret",
            "session-secret",
            "thread-secret",
            "prompt-secret",
            "response-secret",
        ] {
            assert!(!text.contains(forbidden));
        }
        assert!(summary.identifiers.request_id_present);
        assert!(summary.identifiers.session_id_present);
        assert!(summary.identifiers.thread_id_present);
    }

    #[test]
    fn experimental_capture_rejects_raw_input_outside_tmp() {
        let dir = fixture_dir();
        let mut options = claude_options(dir.path(), "{\"type\":\"assistant\"}");
        options.version_file = PathBuf::from("/etc/hosts");

        let error = reduce_capture(&options).unwrap_err().to_string();

        assert!(error.contains("outside temporary root"));
        assert!(!error.contains("/etc/hosts"));
    }

    #[test]
    fn experimental_capture_rejects_symlink_escape() {
        let dir = fixture_dir();
        let link = dir.path().join("outside-link");
        symlink("/etc/hosts", &link).unwrap();
        let mut options = claude_options(dir.path(), "{\"type\":\"assistant\"}");
        options.version_file = link;

        let error = reduce_capture(&options).unwrap_err().to_string();

        assert!(error.contains("symlink input rejected"));
        assert!(!error.contains("hosts"));
    }

    #[test]
    fn experimental_capture_rejects_symlink_within_tmp() {
        let dir = fixture_dir();
        let target = dir.path().join("private-version");
        let link = dir.path().join("private-version-link");
        write_private(&target, "2.1.207 (Claude Code)\n");
        symlink(&target, &link).unwrap();
        let mut options = claude_options(dir.path(), "{\"type\":\"assistant\"}");
        options.version_file = link;

        let error = reduce_capture(&options).unwrap_err().to_string();

        assert!(error.contains("symlink input rejected"));
        assert!(!error.contains("private-version"));
    }

    #[test]
    fn experimental_capture_bounds_files_and_lines() {
        let dir = fixture_dir();
        let options = claude_options(dir.path(), "{\"type\":\"assistant\"}");
        write_private(
            &options.event_file,
            &"x".repeat(MAX_EVENT_BYTES as usize + 1),
        );
        assert!(reduce_capture(&options)
            .unwrap_err()
            .to_string()
            .contains("input rejected"));

        write_private(&options.event_file, &"x".repeat(MAX_LINE_BYTES + 1));
        assert!(reduce_capture(&options)
            .unwrap_err()
            .to_string()
            .contains("line too long"));
    }

    #[test]
    fn experimental_capture_ignores_unknown_json_values() {
        let dir = fixture_dir();
        let events = r#"{"type":"assistant","unknown":{"email":"person@example.test","token":"unknown-secret"},"message":{"text":"issue-627-controlled-sentinel"}}"#;

        let text = serialized(&reduce_capture(&claude_options(dir.path(), events)).unwrap());

        assert!(!text.contains("person@example.test"));
        assert!(!text.contains("unknown-secret"));
        assert!(!text.contains("unknown\""));
    }

    #[test]
    fn experimental_capture_known_cli_usage_diagnostics_are_discarded() {
        let dir = fixture_dir();
        let events = r#"{"type":"result","result":"issue-627-controlled-sentinel","usage":{"input_tokens":1,"cache_creation":{"private":"diagnostic-secret"},"ephemeral_1h_input_tokens":1,"fast":true,"inference_geo":"private","iterations":1,"output_tokens_details":{"private":"detail-secret"},"server_tool_use":{"private":true},"speed":"private","web_search_requests":0},"modelUsage":{"private-model":{"costUSD":"billing-secret"}}}"#;

        let summary = reduce_capture(&claude_options(dir.path(), events)).unwrap();
        let text = serialized(&summary);

        assert_eq!(summary.usage_fields, vec!["input_tokens"]);
        assert!(summary.manual_review_categories().is_empty());
        assert!(!text.contains("diagnostic-secret"));
        assert!(!text.contains("billing-secret"));
        assert!(!text.contains("detail-secret"));
        assert!(!text.contains("cache_creation\""));
        assert!(!text.contains("server_tool_use"));
        assert!(!text.contains("modelUsage"));
    }

    #[test]
    fn experimental_capture_new_usage_field_requires_manual_review() {
        for events in [
            r#"{"type":"result","result":"issue-627-controlled-sentinel","usage":{"new_metric":"metric-secret"}}"#,
            r#"{"type":"result","result":"issue-627-controlled-sentinel","usage":{"new_metric":{"input_tokens":1,"private":"metric-secret"}}}"#,
        ] {
            let dir = fixture_dir();
            let mut options = claude_options(dir.path(), events);
            let debug = dir.path().join("debug.json");
            write_private(&debug, &complete_claude_debug().to_string());
            options.debug_file = Some(debug);

            let summary = reduce_capture(&options).unwrap();
            let text = serialized(&summary);

            assert_eq!(summary.manual_review_categories(), vec!["usage_field"]);
            assert_eq!(
                summary.drift_decision,
                DriftDecision::ImplementationUpdateRequired
            );
            assert!(!text.contains("new_metric"));
            assert!(!text.contains("metric-secret"));
        }
    }

    #[test]
    fn experimental_capture_strips_url_query_fragment_and_userinfo() {
        let dir = fixture_dir();
        let mut options = claude_options(dir.path(), "{\"type\":\"assistant\"}");
        let debug = dir.path().join("debug.json");
        write_private(
            &debug,
            r#"{"url":"https://user:password@api.anthropic.com/v1/messages?code=oauth-secret#fragment","headers":{"authorization":"secret","accept":"text/event-stream"}}"#,
        );
        options.debug_file = Some(debug);

        let text = serialized(&reduce_capture(&options).unwrap());

        assert!(text.contains("api.anthropic.com"));
        assert!(text.contains("messages"));
        for forbidden in ["user", "password", "oauth-secret", "fragment", "?", "#"] {
            assert!(!text.contains(forbidden));
        }
    }

    #[test]
    fn experimental_capture_emits_presence_booleans_not_identifier_values() {
        let dir = fixture_dir();
        let options = claude_options(
            dir.path(),
            r#"{"type":"assistant","request_id":"req-value","session_id":"sess-value","thread_id":"thread-value"}"#,
        );

        let summary = reduce_capture(&options).unwrap();
        let text = serialized(&summary);

        assert!(summary.identifiers.request_id_present);
        assert!(summary.identifiers.session_id_present);
        assert!(summary.identifiers.thread_id_present);
        assert!(!text.contains("req-value"));
        assert!(!text.contains("sess-value"));
        assert!(!text.contains("thread-value"));
    }

    #[test]
    fn experimental_capture_complete_claude_wire_is_no_drift() {
        let dir = fixture_dir();
        let summary = reduce_capture(&options_with_debug(
            dir.path(),
            CaptureProvider::Claude,
            &complete_claude_debug(),
        ))
        .unwrap();

        assert!(summary.wire.headers_observed);
        assert!(summary.wire.headers.billing_identity_present);
        assert_eq!(summary.drift_decision, DriftDecision::NoDrift);
    }

    #[test]
    fn experimental_capture_complete_codex_wire_is_no_drift() {
        let dir = fixture_dir();
        let summary = reduce_capture(&options_with_debug(
            dir.path(),
            CaptureProvider::Codex,
            &complete_codex_debug(),
        ))
        .unwrap();

        assert!(summary.wire.headers_observed);
        assert!(summary.wire.headers.account_identity_present);
        assert_eq!(summary.drift_decision, DriftDecision::NoDrift);
    }

    #[test]
    fn experimental_capture_required_headers_match_runtime_builders() {
        let claude = crate::provider::subscription::SubscriptionProvider::new(
            "token",
            "model",
            "system",
            crate::provider::subscription::SubscriptionConfig::default(),
        )
        .unwrap();
        let mut claude_headers = claude
            .request_headers()
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        claude_headers.sort();
        assert_eq!(
            claude_headers,
            CLAUDE_REQUIRED_WIRE_HEADERS
                .iter()
                .map(|header| (*header).to_string())
                .collect::<Vec<_>>()
        );

        let codex = crate::provider::codex::CodexProvider::new(
            "token",
            "instructions",
            crate::provider::codex::CodexConfig {
                account_id: "account".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let mut codex_headers = codex
            .request_headers()
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        codex_headers.push("content-type".into());
        codex_headers.sort();
        assert_eq!(
            codex_headers,
            CODEX_REQUIRED_WIRE_HEADERS
                .iter()
                .map(|header| (*header).to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn experimental_capture_authorization_only_is_not_safely_observable() {
        let dir = fixture_dir();
        let debug = serde_json::json!({
            "request": {
                "url": "https://api.anthropic.com/v1/messages",
                "headers": {"authorization": "redacted"}
            }
        });
        let summary = reduce_capture(&options_with_debug(
            dir.path(),
            CaptureProvider::Claude,
            &debug,
        ))
        .unwrap();

        assert!(summary.wire.headers.authorization_present);
        assert!(!summary.wire.headers_observed);
        assert_eq!(summary.drift_decision, DriftDecision::NotSafelyObservable);
    }

    #[test]
    fn experimental_capture_each_missing_claude_header_is_not_safely_observable() {
        for missing in CLAUDE_REQUIRED_WIRE_HEADERS {
            let dir = fixture_dir();
            let mut debug = complete_claude_debug();
            debug["request"]["headers"]
                .as_object_mut()
                .unwrap()
                .remove(*missing);

            let summary = reduce_capture(&options_with_debug(
                dir.path(),
                CaptureProvider::Claude,
                &debug,
            ))
            .unwrap();

            assert!(!summary.wire.headers_observed, "missing {missing}");
            assert_eq!(
                summary.drift_decision,
                DriftDecision::NotSafelyObservable,
                "missing {missing}"
            );
        }
    }

    #[test]
    fn experimental_capture_each_missing_codex_header_is_not_safely_observable() {
        for missing in CODEX_REQUIRED_WIRE_HEADERS {
            let dir = fixture_dir();
            let mut debug = complete_codex_debug();
            debug["request"]["headers"]
                .as_object_mut()
                .unwrap()
                .remove(*missing);

            let summary = reduce_capture(&options_with_debug(
                dir.path(),
                CaptureProvider::Codex,
                &debug,
            ))
            .unwrap();

            assert!(!summary.wire.headers_observed, "missing {missing}");
            assert_eq!(
                summary.drift_decision,
                DriftDecision::NotSafelyObservable,
                "missing {missing}"
            );
        }
    }

    #[test]
    fn experimental_capture_missing_claude_billing_block_is_not_safely_observable() {
        let dir = fixture_dir();
        let mut debug = complete_claude_debug();
        debug["request"].as_object_mut().unwrap().remove("body");

        let summary = reduce_capture(&options_with_debug(
            dir.path(),
            CaptureProvider::Claude,
            &debug,
        ))
        .unwrap();

        assert!(!summary.wire.headers.billing_identity_present);
        assert!(!summary.wire.headers_observed);
        assert_eq!(summary.drift_decision, DriftDecision::NotSafelyObservable);
    }

    #[test]
    fn experimental_capture_unexpected_header_requires_implementation_update() {
        let dir = fixture_dir();
        let mut debug = complete_codex_debug();
        debug["request"]["headers"]["x-new-provider-identity"] = Value::String("redacted".into());

        let summary = reduce_capture(&options_with_debug(
            dir.path(),
            CaptureProvider::Codex,
            &debug,
        ))
        .unwrap();

        assert_eq!(summary.manual_review_categories(), vec!["header"]);
        assert_eq!(
            summary.drift_decision,
            DriftDecision::ImplementationUpdateRequired
        );
    }

    #[test]
    fn experimental_capture_summary_schema_is_stable() {
        let dir = fixture_dir();
        let options = claude_options(dir.path(), "{\"type\":\"assistant\"}");
        let value = serde_json::to_value(reduce_capture(&options).unwrap()).unwrap();
        let keys = value
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();

        assert_eq!(
            keys,
            vec![
                "client",
                "controlled_sentinel_observed",
                "drift_decision",
                "event_type_counts",
                "identifiers",
                "model_catalog",
                "product_auth_evidence",
                "rate_limit_fields",
                "raw_retained",
                "schema_version",
                "scope",
                "usage_fields",
                "wire",
            ]
        );
    }

    #[test]
    fn experimental_capture_model_catalog_emits_counts_and_presence_only() {
        let dir = fixture_dir();
        let version = dir.path().join("codex-version.txt");
        let events = dir.path().join("codex-events.jsonl");
        let models = dir.path().join("models.json");
        let bundled = dir.path().join("models-bundled.json");
        write_private(&version, "codex-cli 0.144.1\n");
        write_private(
            &events,
            r#"{"type":"turn.completed","message":"issue-627-controlled-sentinel","usage":{"input_tokens":1,"output_tokens":1}}"#,
        );
        write_private(
            &models,
            r#"[{"service_tiers":[],"input_modalities":[],"supports_parallel_tool_calls":true,"base_instructions":"catalog-secret"}]"#,
        );
        write_private(&bundled, r#"[{"model_messages":"bundled-secret"},{}]"#);
        let options = DriftCaptureOptions {
            provider: CaptureProvider::Codex,
            version_file: version,
            event_file: events,
            debug_file: None,
            model_catalog: Some(models),
            bundled_model_catalog: Some(bundled),
            expected_sentinel: "issue-627-controlled-sentinel".into(),
        };

        let summary = reduce_capture(&options).unwrap();
        let catalog = summary.model_catalog.as_ref().unwrap();
        let text = serialized(&summary);

        assert_eq!(catalog.models_count, 1);
        assert_eq!(catalog.bundled_models_count, 2);
        assert!(catalog.service_tiers_present);
        assert!(catalog.input_modalities_present);
        assert!(catalog.supports_parallel_tool_calls_present);
        assert!(!text.contains("catalog-secret"));
        assert!(!text.contains("bundled-secret"));
        assert!(!text.contains("base_instructions"));
        assert!(!text.contains("model_messages"));
    }

    #[test]
    fn experimental_capture_unknown_events_use_fixed_unknown_bucket() {
        let dir = fixture_dir();
        let options = claude_options(
            dir.path(),
            "{\"type\":\"brand-new-event\",\"secret\":\"never-copy\"}",
        );

        let summary = reduce_capture(&options).unwrap();
        let text = serialized(&summary);

        assert_eq!(summary.event_type_counts.get("unknown"), Some(&1));
        assert_eq!(
            summary.drift_decision,
            DriftDecision::ImplementationUpdateRequired
        );
        assert!(!text.contains("brand-new-event"));
        assert!(!text.contains("never-copy"));
    }

    #[test]
    fn experimental_capture_normalizes_version_without_copying_raw_line() {
        let dir = fixture_dir();
        let options = claude_options(dir.path(), "{\"type\":\"assistant\"}");
        write_private(
            &options.version_file,
            "2.1.207 (Claude Code) local=/home/private-user\n",
        );

        let summary = reduce_capture(&options).unwrap();
        let text = serialized(&summary);

        assert_eq!(summary.client.version, "2.1.207");
        assert!(!text.contains("Claude Code"));
        assert!(!text.contains("private-user"));
        assert!(!text.contains("/home/"));
    }
}
