//! Shared web-UI router assembly + the live request handlers, reused by the
//! desktop daemon (`isyncyoud`) and the standalone mobile client (#89). The daemon
//! calls [`build_live_router`] for the shared base; the mobile client adds its
//! full-node job handlers with [`with_mobile_full_node_jobs`].

mod agent_ops;
#[cfg(feature = "agent-subscription-experimental")]
mod local_cli_fallback;
mod mobile_jobs;

pub use agent_ops::{run_backup_account, AgentOperationPolicy, BackupDelta, BackupRun};
pub use mobile_jobs::{
    MobileJobExecutionError, MobileJobFailureCode, MobileJobKind, MobileJobRetryCode,
    MobileJobRunOutcome, MobileJobRuntime, MobileWorkerDeviceSnapshot,
};

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
use isyncyou_agent::ProductProviderId;
use isyncyou_connectors::ProgressSink;
use isyncyou_core::{Config, OneDriveMode, OneDriveModes};
use isyncyou_store::{Item, Store};
use isyncyou_webui::{OfflineModeRisk, OneDriveMoveRisk};
use std::collections::{BTreeSet, HashMap};
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
use std::path::Path;
use std::path::PathBuf;
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch as a string (handlers stamp "now" with it).
fn unix_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Mint a per-process capability token from `/dev/urandom` (hex), with a
/// pid-based fallback. Required on the destructive restore POST.
pub fn mint_cap_token() -> String {
    use std::io::Read;
    let mut buf = [0u8; 16];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => buf.iter().map(|b| format!("{b:02x}")).collect(),
        Err(_) => {
            // /dev/urandom unavailable — derive a NON-predictable fallback by mixing
            // several entropy sources (a freshly OS-seeded RandomState, the process
            // id, a high-resolution timestamp and a stack address) instead of a bare,
            // guessable pid. Still 32 hex chars like the primary path.
            use std::hash::{BuildHasher, Hasher};
            use std::time::{SystemTime, UNIX_EPOCH};
            let seed_addr = std::ptr::addr_of!(buf) as usize;
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let mut out = String::with_capacity(32);
            for i in 0..2u64 {
                let mut h = std::collections::hash_map::RandomState::new().build_hasher();
                h.write_u64(u64::from(std::process::id()));
                h.write_u128(nanos);
                h.write_usize(seed_addr);
                h.write_u64(i);
                out.push_str(&format!("{:016x}", h.finish()));
            }
            out
        }
    }
}

/// The desktop daemon's destructive-action handler: re-create an archived item
/// in the cloud using the cached `login --write` (restore-scoped) token. Mobile
/// wires `MobileJobRuntime` instead, so the route enqueues a durable job.
pub struct DaemonRestore {
    cfg: Config,
}
impl isyncyou_webui::RestoreHandler for DaemonRestore {
    fn restore(
        &self,
        account: &str,
        service: &str,
        id: &str,
    ) -> Result<isyncyou_webui::RestoreResponse, String> {
        // Refuse a not-yet-ledger-migrated service before resolving a token, so the
        // web UI gets the clear "not crash-safe yet" message. (Engine re-checks.)
        if !isyncyou_engine::cloud_restore_service_supported(service) {
            return Err(isyncyou_engine::unsupported_cloud_restore_service_error(
                service,
            ));
        }
        let token = isyncyou_engine::auth::resolve_cached_restore_token(&self.cfg, account)?;
        let new_id = isyncyou_engine::restore_cloud(&self.cfg, account, service, id, token)?;
        Ok(isyncyou_webui::RestoreResponse::Completed { new_id })
    }
}

/// Fallback read executor for builds without live agent providers (no store/SQLCipher
/// pull): returns a placeholder so the turn loop still runs in CI/release shapes.
#[cfg(not(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
)))]
struct StubExecutor;
#[cfg(not(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
)))]
impl isyncyou_agent::ToolExecutor for StubExecutor {
    fn execute_read(
        &self,
        _action: &isyncyou_agent::ToolAction,
    ) -> Result<String, isyncyou_agent::AgentError> {
        Ok("{\"note\":\"retrieval needs the agent OAuth provider build\"}".to_string())
    }
}

/// Build the read-class tool executor for a turn. The live-provider agent build binds the
/// real `StoreArchive` retrieval executor (S-AG.3/#618: search/read/list/export over the
/// encrypted store + on-disk body files for `account` under `archive_root`); other builds
/// get the stub. S-AG.18/#643 is the progressive/deep-search behavior layered on top.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn make_executor(
    account: &str,
    archive_root: std::path::PathBuf,
) -> Box<dyn isyncyou_agent::ToolExecutor + Send> {
    let restore_root = archive_root.join(".isyncyou-agent").join("restore-local");
    Box::new(agent_ops::RestoreLocalReadExecutor::new(
        isyncyou_agent::archive::StoreArchive::new(account, archive_root.clone()),
        isyncyou_agent::retrieval::RetrievalExecutor::new(
            isyncyou_agent::archive::StoreArchive::new(account, archive_root),
        ),
        restore_root,
    ))
}
#[cfg(not(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
)))]
fn make_executor(
    _account: &str,
    _archive_root: std::path::PathBuf,
) -> Box<dyn isyncyou_agent::ToolExecutor + Send> {
    Box::new(StubExecutor)
}

/// Serialize one stream event to a single-line JSON SSE-data payload.
fn agent_event_json(ev: &isyncyou_agent::StreamEvent) -> String {
    ev.to_public_json_string()
}

/// Default model for the in-app agent (override with `ISYNCYOU_AGENT_MODEL`). The
/// subscription serves Sonnet/Opus; Sonnet is the cheaper default for general use.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
const DEFAULT_MODEL: &str = "claude-sonnet-5";

/// The Claude subscription models the in-app switcher offers (id, human label). Each id is
/// verified against the subscription messages API.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
const CLAUDE_MODELS: &[(&str, &str)] = &[
    ("claude-opus-4-8", "Opus 4.8"),
    ("claude-sonnet-5", "Sonnet 5"),
    ("claude-haiku-4-5-20251001", "Haiku 4.5"),
];
/// The ChatGPT/Codex models the in-app switcher offers (id, human label).
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
const CODEX_MODELS: &[(&str, &str)] = &[("gpt-5.5", "GPT-5.5"), ("gpt-5.4", "GPT-5.4")];

#[cfg(test)]
type TestProviderScript = Arc<Mutex<Option<Vec<Vec<isyncyou_agent::AssistantBlock>>>>>;

/// Result returned by the confirmed destructive-action executor. #621 defines the
/// confirmation contract; #624 wires the real destructive operations behind this seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedActionResult {
    pub summary: String,
}

impl ConfirmedActionResult {
    pub fn new(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
        }
    }
}

/// Narrow seam for destructive actions after human confirmation.
pub trait AgentConfirmedActionExecutor: Send + Sync {
    fn execute_confirmed(
        &self,
        action: &isyncyou_agent::ToolAction,
    ) -> Result<ConfirmedActionResult, String>;
}

/// Narrow audit seam for confirmed agent actions. The live implementation writes the
/// same durable account run log used by Router audit paths; tests use an in-memory sink.
pub trait AgentAuditSink: Send + Sync {
    fn record_confirm(
        &self,
        action: &isyncyou_agent::ToolAction,
        status: &str,
        summary: &str,
    ) -> Result<(), String>;
}

struct StoreAgentAuditSink {
    cfg: Config,
}

impl StoreAgentAuditSink {
    fn store_path(&self, account: &str) -> Option<PathBuf> {
        self.cfg
            .accounts
            .iter()
            .find(|a| a.id == account)
            .or_else(|| self.cfg.accounts.first())
            .map(|a| a.archive_root.join(".isyncyou-store.db"))
    }
}

impl AgentAuditSink for StoreAgentAuditSink {
    fn record_confirm(
        &self,
        action: &isyncyou_agent::ToolAction,
        status: &str,
        summary: &str,
    ) -> Result<(), String> {
        let account = action.account();
        let path = self
            .store_path(account)
            .ok_or_else(|| format!("unknown account '{account}'"))?;
        let store = Store::open(path).map_err(|e| e.to_string())?;
        let now = unix_now();
        store
            .add_run(
                account,
                "audit:agent-confirm",
                &now,
                &now,
                status,
                &agent_audit_summary(summary),
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

fn agent_action_summary(action: &isyncyou_agent::ToolAction) -> String {
    let mut parts = vec![
        format!("op={}", action.op()),
        format!(
            "account={}",
            agent_ops::redact_agent_operation_text(action.account())
        ),
    ];
    if let Some(service) = action.service() {
        parts.push(format!(
            "service={}",
            agent_ops::redact_agent_operation_text(service)
        ));
    }
    parts.join(" ")
}

fn agent_audit_summary(summary: &str) -> String {
    const MAX: usize = 400;
    let mut out: String = summary.chars().take(MAX).collect();
    if summary.chars().count() > MAX {
        out.push_str("...");
    }
    out
}

fn agent_safe_executor_error(error: &str) -> &'static str {
    if error.contains("not_implemented") {
        "not_implemented"
    } else if error.contains("not_available_on_mobile") {
        "not_available_on_mobile"
    } else {
        "execution_failed"
    }
}

fn agent_safe_turn_error(error: &isyncyou_agent::AgentError) -> &'static str {
    match error {
        isyncyou_agent::AgentError::ToolArgs(_) => "assistant_tool_arguments_invalid",
        isyncyou_agent::AgentError::Provider(_) => "provider_request_failed",
        isyncyou_agent::AgentError::Transport(code) => match code.as_str() {
            "provider_connect_timed_out" => "provider_connect_timed_out",
            "provider_response_timed_out" => "provider_response_timed_out",
            "provider_stream_idle_timed_out" => "provider_stream_idle_timed_out",
            "provider_tls_failed" => "provider_tls_failed",
            "provider_name_resolution_failed" => "provider_name_resolution_failed",
            "provider_connect_failed" => "provider_connect_failed",
            "provider_stream_read_failed" => "provider_stream_read_failed",
            "provider_stream_ended_without_event" => "provider_stream_ended_without_event",
            "provider_response_read_failed" => "provider_response_read_failed",
            _ => "provider_transport_failed",
        },
    }
}

/// The agent's system prompt — app-/M365-scoped (the only tool is `isyncyou`).
const AGENT_SYSTEM_PROMPT: &str = "You are the iSyncYou in-app assistant. You help the user with \
their own Microsoft 365 data that iSyncYou manages — mail, OneDrive files and photos, calendar, \
contacts, tasks and notes — plus iSyncYou's backup and restore. Your only tool is `isyncyou`; you \
never touch anything outside the user's M365 domain. Read with the tool before answering, and \
ground factual claims in the returned source fields (`service`, `id`, and `path` or `source`). The app \
already renders every search hit as a rich, typed, clickable card (header + body + a link to the \
item), so DO NOT re-list the found items in your reply and DO NOT use markdown (no **bold**, no \
bullet lists) — answer in one or two short plain-language sentences about what you found. \
Destructive actions (backup, restore-cloud, live-write, share) are confirmed by \
the user out of band — propose them, never assume they ran.";

const AGENT_CONFIRM_TTL_MS: u64 = 120_000;
const AGENT_STREAM_UNOPENED_TTL_MS: u64 = 120_000;
static CONNECTIVITY_PROBES: isyncyou_agent::ProbeLimiter = isyncyou_agent::ProbeLimiter::new();

const MOBILE_CONNECTIVITY_SNAPSHOT_TTL_MS: u64 = 15_000;
const MOBILE_CONNECTIVITY_SNAPSHOT_LIMIT: usize = 8;

#[derive(Clone)]
struct MobileConnectivitySnapshot {
    snapshot: isyncyou_agent::AndroidNetworkSnapshot,
    purpose: isyncyou_agent::ConnectivityPurpose,
    guard_id: String,
    forced_observation: Option<isyncyou_agent::ProbeObservation>,
    expires_at_ms: u64,
}

struct MobileConnectivitySnapshotEntry {
    session_token: String,
    snapshot: MobileConnectivitySnapshot,
}

static MOBILE_CONNECTIVITY_SNAPSHOTS: OnceLock<
    Mutex<HashMap<String, MobileConnectivitySnapshotEntry>>,
> = OnceLock::new();
static ACTIVE_MOBILE_CONNECTIVITY_GUARDS: OnceLock<Mutex<std::collections::HashSet<String>>> =
    OnceLock::new();

fn active_mobile_connectivity_guards() -> &'static Mutex<std::collections::HashSet<String>> {
    ACTIVE_MOBILE_CONNECTIVITY_GUARDS.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

fn mobile_connectivity_snapshots(
) -> &'static Mutex<HashMap<String, MobileConnectivitySnapshotEntry>> {
    MOBILE_CONNECTIVITY_SNAPSHOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn snapshot_purpose_for_guard(reason: &str) -> Option<isyncyou_agent::ConnectivityPurpose> {
    match reason {
        "oauth" => Some(isyncyou_agent::ConnectivityPurpose::OAuthStart),
        "credential_refresh" => Some(isyncyou_agent::ConnectivityPurpose::Refresh),
        "agent_turn" => Some(isyncyou_agent::ConnectivityPurpose::TurnStart),
        _ => None,
    }
}

fn reap_mobile_connectivity_snapshots(
    entries: &mut HashMap<String, MobileConnectivitySnapshotEntry>,
    now_ms: u64,
) {
    entries.retain(|_, entry| entry.snapshot.expires_at_ms > now_ms);
}

/// Register a native-captured Android connectivity snapshot for exactly one subsequent
/// preflight in this app process. The caller is Kotlin/JNI, after it has already validated the
/// opaque foreground-guard id. WebView JavaScript sees only the returned random handle.
pub fn register_mobile_connectivity_snapshot(
    session_token: &str,
    guard_id: &str,
    guard_reason: &str,
    snapshot: isyncyou_agent::AndroidNetworkSnapshot,
    test_hook: Option<&str>,
) -> Result<String, String> {
    if session_token.is_empty() || guard_id.is_empty() || !snapshot.guard_ready {
        return Err("mobile connectivity snapshot is unavailable".into());
    }
    let Some(purpose) = snapshot_purpose_for_guard(guard_reason) else {
        return Err("mobile connectivity snapshot is unavailable".into());
    };
    #[cfg(feature = "agent-network-device-test-hooks")]
    let forced_observation = match test_hook {
        None | Some("") => None,
        Some("connect_timeout") => Some(isyncyou_agent::ProbeObservation::ConnectTimedOut),
        Some("tls_failed") => Some(isyncyou_agent::ProbeObservation::TlsFailed),
        Some("http_failed") => Some(isyncyou_agent::ProbeObservation::HttpStatus(500)),
        Some(_) => return Err("mobile connectivity snapshot is unavailable".into()),
    };
    #[cfg(not(feature = "agent-network-device-test-hooks"))]
    let forced_observation = {
        if test_hook.is_some_and(|value| !value.is_empty()) {
            return Err("mobile connectivity snapshot is unavailable".into());
        }
        None
    };
    let now_ms = unix_now_ms();
    let expires_at_ms = now_ms.saturating_add(MOBILE_CONNECTIVITY_SNAPSHOT_TTL_MS);
    let mut entries = mobile_connectivity_snapshots()
        .lock()
        .map_err(|_| "mobile connectivity snapshot is unavailable".to_string())?;
    reap_mobile_connectivity_snapshots(&mut entries, now_ms);
    if entries.len() >= MOBILE_CONNECTIVITY_SNAPSHOT_LIMIT {
        return Err("mobile connectivity snapshot is unavailable".into());
    }
    active_mobile_connectivity_guards()
        .lock()
        .map_err(|_| "mobile connectivity snapshot is unavailable".to_string())?
        .insert(guard_id.to_string());
    let id = mint_cap_token();
    entries.insert(
        id.clone(),
        MobileConnectivitySnapshotEntry {
            session_token: session_token.to_string(),
            snapshot: MobileConnectivitySnapshot {
                snapshot,
                purpose,
                guard_id: guard_id.to_string(),
                forced_observation,
                expires_at_ms,
            },
        },
    );
    Ok(id)
}

pub fn invalidate_mobile_connectivity_guard(guard_id: &str) {
    if guard_id.is_empty() {
        return;
    }
    if let Ok(mut entries) = mobile_connectivity_snapshots().lock() {
        entries.retain(|_, entry| entry.snapshot.guard_id != guard_id);
    }
    if let Ok(mut guards) = active_mobile_connectivity_guards().lock() {
        guards.remove(guard_id);
    }
}

struct ConsumedMobileConnectivitySnapshot {
    snapshot: isyncyou_agent::AndroidNetworkSnapshot,
    forced_observation: Option<isyncyou_agent::ProbeObservation>,
}

fn consume_mobile_connectivity_snapshot(
    snapshot_id: &str,
    session_token: Option<&str>,
    purpose: isyncyou_agent::ConnectivityPurpose,
) -> Result<ConsumedMobileConnectivitySnapshot, String> {
    let Some(session_token) = session_token.filter(|value| !value.is_empty()) else {
        return Err("mobile connectivity snapshot is unavailable".into());
    };
    let now_ms = unix_now_ms();
    let mut entries = mobile_connectivity_snapshots()
        .lock()
        .map_err(|_| "mobile connectivity snapshot is unavailable".to_string())?;
    reap_mobile_connectivity_snapshots(&mut entries, now_ms);
    let Some(entry) = entries.remove(snapshot_id) else {
        return Err("mobile connectivity snapshot is unavailable".into());
    };
    if entry.session_token != session_token || entry.snapshot.purpose != purpose {
        return Err("mobile connectivity snapshot is unavailable".into());
    }
    let guard_active = active_mobile_connectivity_guards()
        .lock()
        .map_err(|_| "mobile connectivity snapshot is unavailable".to_string())?
        .contains(&entry.snapshot.guard_id);
    if !guard_active {
        return Err("mobile connectivity snapshot is unavailable".into());
    }
    Ok(ConsumedMobileConnectivitySnapshot {
        snapshot: entry.snapshot.snapshot,
        forced_observation: entry.snapshot.forced_observation,
    })
}

struct AgentStreamSlot {
    rx: std::sync::mpsc::Receiver<String>,
    created_at_ms: u64,
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
enum OAuthAttempt {
    Claude {
        state: String,
        expires_at: std::time::Instant,
    },
    Codex {
        cancelled: Arc<AtomicBool>,
        expires_at: std::time::Instant,
    },
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
const OAUTH_ATTEMPT_TTL: std::time::Duration = std::time::Duration::from_secs(8 * 60);

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn reap_oauth_attempts(attempts: &mut HashMap<String, OAuthAttempt>) {
    let now = std::time::Instant::now();
    attempts.retain(|_, attempt| match attempt {
        OAuthAttempt::Claude { expires_at, .. } | OAuthAttempt::Codex { expires_at, .. } => {
            *expires_at > now
        }
    });
}

/// #639 T7: the typed reason a turn was refused before any turn state was created. It maps to a
/// closed wire code the router turns into a specific HTTP status (never a blanket 500).
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentStartTurnError {
    /// The selected product provider is not host-verified ready (no valid activated credential),
    /// and no experimental fallback applies. Closed wire code `product_not_ready` -> HTTP 409.
    ProductNotReady,
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
impl AgentStartTurnError {
    fn wire(self) -> &'static str {
        match self {
            Self::ProductNotReady => "product_not_ready",
        }
    }
}

/// The in-app agent handler (S-AG.6/#621). Drives a real turn: the product Claude/Codex
/// OAuth provider path when the user has connected an account, otherwise a deterministic
/// "not connected" message. Owns the stream hub + pending-action registry, so the model
/// never holds a capability token.
pub struct DaemonAgent {
    /// Source of each account's `archive_root` for the retrieval executor
    /// (`archive_root_for`); the restore path lands in #624.
    cfg: Config,
    hub: Arc<isyncyou_agent::AgentStreamHub>,
    pending: Arc<isyncyou_agent::PendingRegistry>,
    confirmed_executor: Arc<dyn AgentConfirmedActionExecutor>,
    audit_sink: Arc<dyn AgentAuditSink>,
    streams: Mutex<std::collections::HashMap<String, AgentStreamSlot>>,
    last_usage: Arc<Mutex<Option<isyncyou_agent::Usage>>>,
    // Read only by the gated product credential-refresh path; keep them without that feature so the
    // no-oauth-feature build (mobile --no-default-features) compiles.
    #[cfg_attr(
        not(any(
            feature = "agent-oauth-providers",
            feature = "agent-subscription-experimental"
        )),
        allow(dead_code)
    )]
    credential_now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
    #[cfg_attr(
        not(any(
            feature = "agent-oauth-providers",
            feature = "agent-subscription-experimental"
        )),
        allow(dead_code)
    )]
    credential_refresh_gate: Mutex<()>,
    /// #639: the single in-process product-runtime gate. One hold spans selection + model +
    /// refresh + activation-check + attestation + provider construction (and the status readiness
    /// read + the selection write), so status and a turn can never observe credential and
    /// activation from different revisions. Lock order: this gate BEFORE `credential_refresh_gate`.
    #[cfg_attr(
        not(any(
            feature = "agent-oauth-providers",
            feature = "agent-subscription-experimental"
        )),
        allow(dead_code)
    )]
    product_runtime_gate: Arc<Mutex<()>>,
    seq: AtomicU64,
    /// Directory holding the app OAuth credential store and an optional local
    /// `agent-oauth.json` policy assertion. Product builds reject any tuple that differs
    /// from the compiled official provider configuration.
    #[cfg_attr(
        not(any(
            feature = "agent-oauth-providers",
            feature = "agent-subscription-experimental"
        )),
        allow(dead_code)
    )]
    oauth_dir: PathBuf,
    /// Tracks in-flight device OAuth logins between start and the browser callback.
    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    oauth: isyncyou_agent::AgentOAuth,
    /// Opaque UI attempt ids map to private OAuth state or cancellation flags. They are
    /// process-local, short-lived, and never serialized into status/audit output.
    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    oauth_attempts: Arc<Mutex<HashMap<String, OAuthAttempt>>>,
    #[cfg(test)]
    test_provider_script: Option<TestProviderScript>,
}

impl DaemonAgent {
    pub fn new(cfg: Config, oauth_dir: PathBuf) -> Self {
        Self::new_with_policy(
            cfg,
            oauth_dir,
            AgentOperationPolicy::DesktopEnabled,
            Arc::new(Mutex::new(())),
        )
    }

    pub fn new_with_policy(
        cfg: Config,
        oauth_dir: PathBuf,
        operation_policy: AgentOperationPolicy,
        gate: Arc<Mutex<()>>,
    ) -> Self {
        #[cfg(any(
            feature = "agent-oauth-providers",
            feature = "agent-subscription-experimental"
        ))]
        let _ = std::fs::remove_file(oauth_dir.join(CODEX_CALLBACK_DIAGNOSTICS_FILE));
        let audit_sink = Arc::new(StoreAgentAuditSink { cfg: cfg.clone() });
        let confirmed_executor =
            agent_ops::confirmed_executor_for_policy(operation_policy, cfg.clone(), gate);
        let agent = Self {
            cfg,
            hub: Arc::new(isyncyou_agent::AgentStreamHub::new()),
            pending: Arc::new(isyncyou_agent::PendingRegistry::new()),
            confirmed_executor,
            audit_sink,
            streams: Mutex::new(std::collections::HashMap::new()),
            last_usage: Arc::new(Mutex::new(None)),
            credential_now_ms: Arc::new(now_ms),
            credential_refresh_gate: Mutex::new(()),
            product_runtime_gate: Arc::new(Mutex::new(())),
            seq: AtomicU64::new(0),
            oauth_dir,
            #[cfg(any(
                feature = "agent-oauth-providers",
                feature = "agent-subscription-experimental"
            ))]
            oauth: isyncyou_agent::AgentOAuth::new(),
            #[cfg(any(
                feature = "agent-oauth-providers",
                feature = "agent-subscription-experimental"
            ))]
            oauth_attempts: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(test)]
            test_provider_script: None,
        };
        // #639 T8: recover any product onboarding interrupted by a crash before the durable
        // authority (bundle + activation + terminal journal entry) was fully written.
        #[cfg(any(
            feature = "agent-oauth-providers",
            feature = "agent-subscription-experimental"
        ))]
        agent.recover_product_onboarding();
        agent
    }

    #[cfg(test)]
    fn with_test_confirm_components(
        cfg: Config,
        oauth_dir: PathBuf,
        executor: Arc<dyn AgentConfirmedActionExecutor>,
        audit_sink: Arc<dyn AgentAuditSink>,
    ) -> Self {
        let mut agent = Self::new(cfg, oauth_dir);
        agent.confirmed_executor = executor;
        agent.audit_sink = audit_sink;
        agent
    }

    #[cfg(test)]
    fn with_test_provider_script_and_confirm_components(
        cfg: Config,
        oauth_dir: PathBuf,
        script: Vec<Vec<isyncyou_agent::AssistantBlock>>,
        executor: Arc<dyn AgentConfirmedActionExecutor>,
        audit_sink: Arc<dyn AgentAuditSink>,
    ) -> Self {
        let mut agent = Self::with_test_confirm_components(cfg, oauth_dir, executor, audit_sink);
        agent.test_provider_script = Some(Arc::new(Mutex::new(Some(script))));
        agent
    }

    /// Resolve an account's archive root (holds `.isyncyou-store.db` + the on-disk body
    /// files) for the retrieval executor. Matches by account id, else the first account,
    /// else an empty path (an empty store simply yields no hits — never a panic).
    fn archive_root_for(&self, account: &str) -> std::path::PathBuf {
        self.cfg
            .accounts
            .iter()
            .find(|a| a.id == account)
            .or_else(|| self.cfg.accounts.first())
            .map(|a| a.archive_root.clone())
            .unwrap_or_default()
    }

    fn sweep_unopened_streams_locked(
        streams: &mut std::collections::HashMap<String, AgentStreamSlot>,
        now_ms: u64,
    ) -> usize {
        let before = streams.len();
        streams.retain(|_, slot| {
            now_ms
                <= slot
                    .created_at_ms
                    .saturating_add(AGENT_STREAM_UNOPENED_TTL_MS)
        });
        before - streams.len()
    }

    #[cfg(test)]
    fn sweep_unopened_streams_for_tests(&self, now_ms: u64) -> usize {
        let mut streams = self.streams.lock().unwrap();
        Self::sweep_unopened_streams_locked(&mut streams, now_ms)
    }

    #[cfg(test)]
    fn unopened_stream_count_for_tests(&self) -> usize {
        self.streams.lock().unwrap().len()
    }

    /// Test-only provider-SELECTION probe (#639 T7): resolve the selected provider directly from
    /// stored credentials, without the product-runtime readiness gate. Production turns go through
    /// [`resolve_turn_provider`] / [`product_runtime_gate`]; this stays for selection unit tests.
    #[cfg(test)]
    fn build_turn_provider(&self, system: &str) -> Box<dyn isyncyou_agent::LlmProvider + Send> {
        #[cfg(test)]
        if let Some(script) = &self.test_provider_script {
            if let Some(script) = script.lock().unwrap().take() {
                return Box::new(isyncyou_agent::FakeProvider::new(script));
            }
        }
        #[cfg(any(
            feature = "agent-oauth-providers",
            feature = "agent-subscription-experimental"
        ))]
        {
            // #639: the switcher selection drives which provider is built; there is NO
            // cross-provider fallback (an activated non-selected provider is never silently
            // substituted, fixing the old "falls back to the other" behavior).
            let selected = self.agent_settings().0;
            let built = match ProductProviderId::parse(&selected) {
                Some(ProductProviderId::Codex) => self.try_codex_provider(system),
                Some(ProductProviderId::Claude) => self.try_subscription_provider(system),
                None => Ok(None),
            };
            match built {
                Ok(Some(provider)) => return provider,
                Err(_) => return credential_resolution_error_provider(),
                Ok(None) => {}
            }
        }
        #[cfg(not(any(
            feature = "agent-oauth-providers",
            feature = "agent-subscription-experimental"
        )))]
        let _ = system;
        Box::new(isyncyou_agent::FakeProvider::new(vec![vec![
            isyncyou_agent::AssistantBlock::Text(
                "The AI assistant isn't connected yet — open the Assistant tab and connect your \
                 Claude account, then try again."
                    .to_string(),
            ),
        ]]))
    }

    /// #639 T7: resolve the provider a turn will use, applying the product-runtime gate. A test
    /// script (CI) and the no-product-feature build bypass the gate with a deterministic provider;
    /// the product build returns the closed error code on not-ready so `start_turn` can reject the
    /// turn before creating any turn state.
    fn resolve_turn_provider(
        &self,
        system: &str,
    ) -> Result<Box<dyn isyncyou_agent::LlmProvider + Send>, String> {
        #[cfg(test)]
        if let Some(script) = &self.test_provider_script {
            if let Some(script) = script.lock().unwrap().take() {
                return Ok(Box::new(isyncyou_agent::FakeProvider::new(script)));
            }
        }
        #[cfg(any(
            feature = "agent-oauth-providers",
            feature = "agent-subscription-experimental"
        ))]
        {
            self.product_runtime_gate(system)
                .map_err(|e| e.wire().to_string())
        }
        #[cfg(not(any(
            feature = "agent-oauth-providers",
            feature = "agent-subscription-experimental"
        )))]
        {
            let _ = system;
            Ok(Box::new(isyncyou_agent::FakeProvider::new(vec![vec![
                isyncyou_agent::AssistantBlock::Text(
                    "The AI assistant isn't connected yet — open the Assistant tab and connect \
                     your Claude account, then try again."
                        .to_string(),
                ),
            ]])))
        }
    }

    /// #639 T7: per-provider PRODUCT readiness, DECOUPLED from selection. `true` only when a valid
    /// **Active** V2 bundle is present (not needs-refresh / invalid / absent / experimental) AND a
    /// durable `ProductActivationV1` matches its credential generation + the official policy
    /// fingerprint + the harness contract version AND the shipped static harness still attests.
    /// An experimental local-CLI credential can never satisfy this (it has no bundle/activation).
    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn provider_ready(&self, provider: ProductProviderId) -> bool {
        let (present_valid, credential_id) = match provider {
            ProductProviderId::Claude => (
                matches!(
                    self.claude_product_credential_state(),
                    ProductCredentialState::PresentValid(_)
                ),
                SUBSCRIPTION_CREDENTIAL_ID,
            ),
            ProductProviderId::Codex => (
                matches!(
                    self.codex_product_credential_state(),
                    ProductCredentialState::PresentValid(_)
                ),
                CODEX_CREDENTIAL_ID,
            ),
        };
        if !present_valid {
            return false;
        }
        let generation = match load_product_bundle_meta(&self.oauth_dir, credential_id) {
            Some(meta) if meta.lifecycle == CredentialLifecycle::Active => meta.generation,
            _ => return false,
        };
        let activation = match load_product_activation(&self.oauth_dir, provider) {
            Some(activation) => activation,
            None => return false,
        };
        if !activation.matches(
            provider,
            &generation,
            &oauth_policy_fingerprint(provider),
            isyncyou_agent::HARNESS_CONTRACT_VERSION,
        ) {
            return false;
        }
        isyncyou_agent::attest_static_product_harness(harness_provider_for(provider)).is_ok()
    }

    /// #639 T9: whether an official OAuth attempt for `provider` is currently in flight.
    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn has_active_attempt(&self, provider: ProductProviderId) -> bool {
        self.oauth_attempts
            .lock()
            .map(|attempts| {
                attempts.values().any(|attempt| {
                    matches!(
                        (provider, attempt),
                        (ProductProviderId::Claude, OAuthAttempt::Claude { .. })
                            | (ProductProviderId::Codex, OAuthAttempt::Codex { .. })
                    )
                })
            })
            .unwrap_or(false)
    }

    /// #639 T9: the per-provider onboarding projection for the wizard — `{state, steps[]}`. It is
    /// derived from the DURABLE authority first (a ready provider reports all 8 steps complete
    /// regardless of the journal, so the projection survives journal TTL), then an in-flight attempt,
    /// then credential presence. It never leaks any token/account/OAuth value.
    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn provider_onboarding(&self, provider: ProductProviderId) -> serde_json::Value {
        let (state, completed) = if self.provider_ready(provider) {
            ("ready", ONBOARDING_SUCCESS_CHAIN.len())
        } else if self.has_active_attempt(provider) {
            ("in_progress", 0)
        } else {
            match self
                .product_credential_status(provider.wire())
                .unwrap_or("reconnect_required")
            {
                "reconnect_required" | "refresh_required" => ("reconnect_required", 0),
                _ => ("not_started", 0),
            }
        };
        let steps: Vec<serde_json::Value> = ONBOARDING_SUCCESS_CHAIN
            .iter()
            .enumerate()
            .map(|(i, step)| serde_json::json!({ "key": step.wire(), "complete": i < completed }))
            .collect();
        serde_json::json!({ "state": state, "steps": steps })
    }

    /// #639 T9: the full onboarding projection block for `status_json`. `selected_provider` is null
    /// for a corrupt/unknown settings record (fail-closed), never a default/alternative provider.
    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn onboarding_projection(&self) -> serde_json::Value {
        let selected = ProductProviderId::parse(&self.agent_settings().0);
        let selected_state = match selected {
            Some(provider) => self.provider_onboarding(provider)["state"].clone(),
            None => serde_json::Value::String("not_started".into()),
        };
        serde_json::json!({
            "selected_provider": selected.map(|p| p.wire()),
            "selected_state": selected_state,
            "providers": {
                "claude": self.provider_onboarding(ProductProviderId::Claude),
                "codex": self.provider_onboarding(ProductProviderId::Codex),
            },
        })
    }

    /// #639 T7 / #627: build a provider from the EXPERIMENTAL local-CLI credential only. Compiled
    /// solely under the experimental opt-in, it never reads the product store and only resolves on
    /// an `Absent` product state (a present-but-invalid bundle fails closed). It never sets product
    /// readiness/activation — it exists so the experimental turn stays available, walled off.
    #[cfg(feature = "agent-subscription-experimental")]
    fn try_experimental_only_provider(
        &self,
        provider: ProductProviderId,
        system: &str,
    ) -> Option<Box<dyn isyncyou_agent::LlmProvider + Send>> {
        match provider {
            ProductProviderId::Claude => {
                if !matches!(
                    self.claude_product_credential_state(),
                    ProductCredentialState::Absent
                ) {
                    return None;
                }
                let credential = self.experimental_claude_credential().ok()??;
                let provider = isyncyou_agent::SubscriptionProvider::new(
                    credential.access_token,
                    self.model_for("claude"),
                    system,
                    self.subscription_config(),
                )
                .ok()?;
                Some(Box::new(provider))
            }
            ProductProviderId::Codex => {
                if !matches!(
                    self.codex_product_credential_state(),
                    ProductCredentialState::Absent
                ) {
                    return None;
                }
                let credential = self.experimental_codex_credential().ok()??;
                let cfg = isyncyou_agent::CodexConfig {
                    account_id: credential.account_id,
                    model: self.model_for("codex"),
                    ..Default::default()
                };
                let provider =
                    isyncyou_agent::CodexProvider::new(credential.access_token, system, cfg)
                        .ok()?;
                Some(Box::new(provider))
            }
        }
    }

    /// #639 T7: the single atomic product-runtime gate for building a turn's provider. One hold of
    /// `product_runtime_gate` spans selection + readiness (activation + attestation) + provider
    /// construction — status and a turn can never read credential and activation from different
    /// revisions, and there is no second settings/credential read. It runs BEFORE any turn-id /
    /// stream-slot / archive resolution: a rejected turn creates none of that state. The PRODUCT
    /// path uses ONLY the selected provider and ONLY when host-verified ready (no fallback). #627
    /// experimental is a separate, compiled-in-only path that never confers product readiness.
    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn product_runtime_gate(
        &self,
        system: &str,
    ) -> Result<Box<dyn isyncyou_agent::LlmProvider + Send>, AgentStartTurnError> {
        let _gate = self
            .product_runtime_gate
            .lock()
            .map_err(|_| AgentStartTurnError::ProductNotReady)?;
        // Corrupt/unknown selection fails closed — never a default/alternative provider.
        let selected = match ProductProviderId::parse(&self.agent_settings().0) {
            Some(id) => id,
            None => return Err(AgentStartTurnError::ProductNotReady),
        };
        if self.provider_ready(selected) {
            let built = match selected {
                ProductProviderId::Claude => self.try_subscription_provider(system),
                ProductProviderId::Codex => self.try_codex_provider(system),
            };
            return match built {
                Ok(Some(provider)) => Ok(provider),
                // Ready said yes but construction raced/failed — fail closed, never fall back.
                _ => Err(AgentStartTurnError::ProductNotReady),
            };
        }
        #[cfg(feature = "agent-subscription-experimental")]
        if let Some(provider) = self.try_experimental_only_provider(selected, system) {
            return Ok(provider);
        }
        Err(AgentStartTurnError::ProductNotReady)
    }

    /// #639 T8: commit a successful official Claude OAuth atomically under the product-runtime gate:
    /// write the encrypted V2 credential (fresh generation) + activation, record the ordered success
    /// chain to the generation journal, then persist the selection. Holding the gate here means a
    /// concurrent status/turn read can never observe a half-written credential/activation revision.
    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn commit_claude_oauth_success(
        &self,
        token: &isyncyou_agent::oauth::RefreshedToken,
    ) -> Result<(), String> {
        let _gate = self
            .product_runtime_gate
            .lock()
            .map_err(|_| "product_busy".to_string())?;
        self.store_token(token)?;
        let generation = load_product_bundle_meta(&self.oauth_dir, SUBSCRIPTION_CREDENTIAL_ID)
            .map(|m| m.generation)
            .unwrap_or_default();
        record_onboarding_generation_transitions(
            &self.oauth_dir,
            &generation,
            &ONBOARDING_SUCCESS_CHAIN,
        );
        self.set_agent_settings("claude", DEFAULT_MODEL)?;
        Ok(())
    }

    /// #639 T8: startup crash-window recovery over the DURABLE authority (bundle + activation),
    /// never re-running OAuth. For each product provider with a valid Active V2 bundle:
    /// window 2 (bundle written, activation missing) -> re-attest + write the matching activation
    /// for the bundle's generation; window 2/3 -> ensure the generation journal carries the full
    /// success chain (the missing terminal transition is added). Run once at construction.
    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn recover_product_onboarding(&self) {
        let _gate = match self.product_runtime_gate.lock() {
            Ok(gate) => gate,
            Err(_) => return,
        };
        for provider in [ProductProviderId::Claude, ProductProviderId::Codex] {
            let credential_id = match provider {
                ProductProviderId::Claude => SUBSCRIPTION_CREDENTIAL_ID,
                ProductProviderId::Codex => CODEX_CREDENTIAL_ID,
            };
            let meta = match load_product_bundle_meta(&self.oauth_dir, credential_id) {
                Some(meta) if meta.lifecycle == CredentialLifecycle::Active => meta,
                _ => continue,
            };
            let policy = oauth_policy_fingerprint(provider);
            let activation_matches = load_product_activation(&self.oauth_dir, provider)
                .map(|activation| {
                    activation.matches(
                        provider,
                        &meta.generation,
                        &policy,
                        isyncyou_agent::HARNESS_CONTRACT_VERSION,
                    )
                })
                .unwrap_or(false);
            if !activation_matches
                && activate_product(&self.oauth_dir, provider, &meta.generation).is_err()
            {
                // A harness that no longer attests refuses activation; leave it not ready.
                continue;
            }
            record_onboarding_generation_transitions(
                &self.oauth_dir,
                &meta.generation,
                &ONBOARDING_SUCCESS_CHAIN,
            );
        }
    }
}

/// The app OAuth credential we persist: access token, refresh token, and the access
/// token's absolute expiry (ms since the Unix epoch), so the daemon can refresh without
/// reading local provider CLI files.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
#[derive(Clone)]
struct StoredCredential {
    access_token: String,
    refresh_token: String,
    /// Absolute expiry in ms since the Unix epoch; 0 = unknown.
    expires_at_ms: u64,
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
impl StoredCredential {
    /// Serialize to the JSON blob persisted in the credential store.
    fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "access_token": self.access_token,
            "refresh_token": self.refresh_token,
            "expires_at_ms": self.expires_at_ms,
        }))
        .unwrap_or_default()
    }

    /// Parse a stored JSON blob; `None` if it is not our blob shape (e.g. a bare token).
    fn from_json(raw: &[u8]) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_slice(raw).ok()?;
        let access_token = v.get("access_token")?.as_str()?.to_string();
        Some(Self {
            access_token,
            refresh_token: v
                .get("refresh_token")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            expires_at_ms: v.get("expires_at_ms").and_then(|x| x.as_u64()).unwrap_or(0),
        })
    }
}

/// Ms since the Unix epoch (0 on a clock error). Always available (the non-gated `DaemonAgent`
/// constructor seeds `credential_now_ms` with it), so the no-oauth-feature build compiles too.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// #639: the current schema version of a persisted product credential blob. A blob without it
/// (a legacy token-only blob from before #639) is treated as un-migratable and forces reconnect.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
const PRODUCT_CREDENTIAL_SCHEMA_VERSION: u32 = 2;

/// #639: whether a persisted product credential is usable, or has been marked (fail-closed) as
/// requiring a fresh official OAuth. A failed refresh persists `ReconnectRequired` so the next
/// status read reports reconnect rather than re-attempting a refresh in a loop.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum CredentialLifecycle {
    Active,
    ReconnectRequired { closed_code: String },
}

/// #639: the non-token metadata bound to a persisted product credential bundle (V2). It carries
/// the identity of the credential (`generation`), the official-OAuth policy it was minted under
/// (`policy_fingerprint`), and its `lifecycle`. `generation` is minted once at login and preserved
/// across refresh; a credential identity change rotates it via a fresh login only.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProductBundleMeta {
    generation: String,
    policy_fingerprint: String,
    lifecycle: CredentialLifecycle,
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
impl ProductBundleMeta {
    /// Fresh metadata for a brand-new login: a new random generation, the current official policy
    /// fingerprint for `provider`, and an `Active` lifecycle.
    fn fresh(provider: ProductProviderId) -> Self {
        Self {
            generation: uuid_v4(),
            policy_fingerprint: oauth_policy_fingerprint(provider),
            lifecycle: CredentialLifecycle::Active,
        }
    }

    /// Merge these metadata keys into a token-only JSON object (`token_json`) to form the V2 blob.
    /// Returns the token blob unchanged (best-effort) only if it is not a JSON object.
    fn to_blob(&self, token_json: Vec<u8>) -> Vec<u8> {
        let mut v: serde_json::Value = match serde_json::from_slice(&token_json) {
            Ok(serde_json::Value::Object(map)) => serde_json::Value::Object(map),
            _ => return token_json,
        };
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "schema_version".into(),
                serde_json::json!(PRODUCT_CREDENTIAL_SCHEMA_VERSION),
            );
            obj.insert(
                "credential_generation".into(),
                serde_json::json!(self.generation),
            );
            obj.insert(
                "oauth_policy_fingerprint".into(),
                serde_json::json!(self.policy_fingerprint),
            );
            let (lifecycle, code) = match &self.lifecycle {
                CredentialLifecycle::Active => ("active", String::new()),
                CredentialLifecycle::ReconnectRequired { closed_code } => {
                    ("reconnect_required", closed_code.clone())
                }
            };
            obj.insert("lifecycle".into(), serde_json::json!(lifecycle));
            obj.insert("reconnect_code".into(), serde_json::json!(code));
        }
        serde_json::to_vec(&v).unwrap_or(token_json)
    }

    /// Parse the V2 metadata from a stored blob. `None` for a legacy/incomplete blob (missing the
    /// schema version or the generation) — the caller treats that as un-migratable → reconnect.
    fn from_blob(raw: &[u8]) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_slice(raw).ok()?;
        if v.get("schema_version").and_then(|x| x.as_u64())
            != Some(PRODUCT_CREDENTIAL_SCHEMA_VERSION as u64)
        {
            return None;
        }
        let generation = v.get("credential_generation")?.as_str()?.to_string();
        if generation.is_empty() {
            return None;
        }
        let policy_fingerprint = v
            .get("oauth_policy_fingerprint")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let lifecycle = match v
            .get("lifecycle")
            .and_then(|x| x.as_str())
            .unwrap_or("active")
        {
            "active" => CredentialLifecycle::Active,
            _ => CredentialLifecycle::ReconnectRequired {
                closed_code: v
                    .get("reconnect_code")
                    .and_then(|x| x.as_str())
                    .unwrap_or("reconnect_required")
                    .to_string(),
            },
        };
        Some(Self {
            generation,
            policy_fingerprint,
            lifecycle,
        })
    }
}

/// #639: a random RFC-4122 v4 UUID string (from the crypto RNG) — the credential generation id.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn uuid_v4() -> String {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut b = [0u8; 16];
    if SystemRandom::new().fill(&mut b).is_err() {
        // Fail closed to a clearly-invalid, non-empty id rather than a guessable constant.
        b = *b"isyncyou-rngbad!";
    }
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15],
    )
}

/// #639: a stable fingerprint of the compiled official OAuth policy tuple for `provider`
/// (authorize/token endpoints, client id, redirect, scopes). Binding it into the activation record
/// means a credential minted under a different (e.g. overridden) policy can never read as ready.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn oauth_policy_fingerprint(provider: ProductProviderId) -> String {
    let tuple = match provider {
        ProductProviderId::Claude => {
            let c = isyncyou_agent::oauth::OAuthConfig::default();
            format!(
                "claude|{}|{}|{}|{}|{}",
                c.authorize_url,
                c.token_url,
                c.client_id,
                c.manual_redirect_url,
                c.scopes.join(",")
            )
        }
        ProductProviderId::Codex => {
            let c = isyncyou_agent::oauth::CodexOAuthConfig::default();
            format!(
                "codex|{}|{}|{}|{}|{}",
                c.authorize_url, c.token_url, c.client_id, c.redirect_uri, c.scope
            )
        }
    };
    let d = ring::digest::digest(&ring::digest::SHA256, tuple.as_bytes());
    d.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// #639: the iSyncYou harness contract version the runtime attestation enforces (T6). The
/// activation record binds it so a credential activated under an older harness contract cannot
/// read as ready after the contract changes without a re-attestation (T4/T8).
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
// #639: the activation/journal/lock storage below is wired into the runtime by T7 (gate),
// T8 (journal transitions) and T9 (status); it reads as dead in the lib target until then.
#[allow(dead_code)]
const HARNESS_CONTRACT_VERSION: u32 = 1;

/// #639: journal bounds — a hard cap the bounded reader enforces before allocation.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
const MAX_JOURNAL_PLAINTEXT_BYTES: usize = 65_536;
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
const MAX_JOURNAL_ENVELOPE_BYTES: usize = 98_304;
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
const MAX_JOURNAL_TRANSITIONS: usize = 32;

/// #639: the ordered product onboarding states (monotonic within one setup attempt). Ordering is
/// proven by recorded transitions in the journal, not by an ordinal — `ErrorRedacted` is a terminal
/// transition, never a "highest" state.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProductOnboardingState {
    NotStarted,
    OfficialSignInStarted,
    OfficialOauthCompleted,
    CredentialEncrypted,
    RetainedEnvelopeVerified,
    DefaultHarnessRemoved,
    M365ProfileActivated,
    IsyncyouToolConnected,
    SubscriptionIdentitySet,
    Ready,
    ErrorRedacted,
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
impl ProductOnboardingState {
    fn wire(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::OfficialSignInStarted => "official_sign_in_started",
            Self::OfficialOauthCompleted => "official_oauth_completed",
            Self::CredentialEncrypted => "credential_encrypted",
            Self::RetainedEnvelopeVerified => "retained_envelope_verified",
            Self::DefaultHarnessRemoved => "default_harness_removed",
            Self::M365ProfileActivated => "m365_profile_activated",
            Self::IsyncyouToolConnected => "isyncyou_tool_connected",
            Self::SubscriptionIdentitySet => "subscription_identity_set",
            Self::Ready => "ready",
            Self::ErrorRedacted => "error_redacted",
        }
    }
}

/// #639: the durable, authenticated product-activation record — the ONLY persisted readiness
/// authority. It binds the credential `generation`, the official-OAuth `policy_fingerprint`, and
/// the `harness_contract_version`; readiness additionally requires a valid Active V2 bundle and a
/// fresh runtime attestation (T6/T7). Stored in the encrypted CredentialStore (authenticated via
/// its AAD-bound class), id `activation:<provider>`.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProductActivationV1 {
    provider_id: String,
    credential_generation: String,
    oauth_policy_fingerprint: String,
    harness_contract_version: u32,
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
impl ProductActivationV1 {
    fn activation_store_id(provider: ProductProviderId) -> String {
        format!("activation:{}", provider.wire())
    }

    fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "provider_id": self.provider_id,
            "credential_generation": self.credential_generation,
            "oauth_policy_fingerprint": self.oauth_policy_fingerprint,
            "harness_contract_version": self.harness_contract_version,
        }))
        .unwrap_or_default()
    }

    fn from_json(raw: &[u8]) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_slice(raw).ok()?;
        if v.get("schema_version").and_then(|x| x.as_u64()) != Some(1) {
            return None;
        }
        Some(Self {
            provider_id: v.get("provider_id")?.as_str()?.to_string(),
            credential_generation: v.get("credential_generation")?.as_str()?.to_string(),
            oauth_policy_fingerprint: v.get("oauth_policy_fingerprint")?.as_str()?.to_string(),
            harness_contract_version: v.get("harness_contract_version")?.as_u64()? as u32,
        })
    }

    /// Whether this activation authorizes readiness for `provider` at the given credential
    /// `generation`, official `policy_fingerprint`, and harness `contract_version`. All four must
    /// match — a generation match alone is not enough.
    fn matches(
        &self,
        provider: ProductProviderId,
        generation: &str,
        policy_fingerprint: &str,
        contract_version: u32,
    ) -> bool {
        self.provider_id == provider.wire()
            && self.credential_generation == generation
            && self.oauth_policy_fingerprint == policy_fingerprint
            && self.harness_contract_version == contract_version
    }
}

/// Persist a product activation record (#639) under the dedicated activation secret class.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn store_product_activation(
    oauth_dir: &Path,
    provider: ProductProviderId,
    activation: &ProductActivationV1,
) -> Result<(), String> {
    agent_credential_store(oauth_dir)?
        .put(
            isyncyou_agent::SecretClass::ProductActivation,
            &ProductActivationV1::activation_store_id(provider),
            &isyncyou_agent::Secret::new(activation.to_json()),
        )
        .map_err(credential_store_error)
}

/// Load the product activation record (#639); `None` if absent/legacy/undecryptable.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn load_product_activation(
    oauth_dir: &Path,
    provider: ProductProviderId,
) -> Option<ProductActivationV1> {
    if !agent_credential_store_exists(oauth_dir) {
        return None;
    }
    let store = agent_credential_store(oauth_dir).ok()?;
    let secret = store
        .get_bounded(
            isyncyou_agent::SecretClass::ProductActivation,
            &ProductActivationV1::activation_store_id(provider),
            MAX_JOURNAL_ENVELOPE_BYTES,
            MAX_JOURNAL_PLAINTEXT_BYTES,
        )
        .ok()??;
    ProductActivationV1::from_json(secret.expose())
}

/// A single recorded onboarding transition (#639). It never carries OAuth state/verifier/code/url —
/// only the reached state, the credential generation it belongs to, and an optional closed error code.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct OnboardingTransition {
    state: ProductOnboardingState,
    generation: String,
    error_code: Option<String>,
}

/// The bounded, authenticated per-attempt onboarding transition journal (#639). It is for expiry,
/// crash recovery, and evidence only — never the readiness authority. Capped at
/// `MAX_JOURNAL_TRANSITIONS` transitions; the encrypted blob is bounded-read at load.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct OnboardingAttemptJournalV1 {
    transitions: Vec<OnboardingTransition>,
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
impl OnboardingAttemptJournalV1 {
    /// The store id for `attempt_id`: a domain-separated hash, never the raw attempt id. Used for
    /// the pre-credential (in-flight) phase, keyed by the opaque UI attempt.
    fn journal_store_id(attempt_id: &str) -> String {
        Self::hashed_store_id("isyncyou-onboarding-attempt-v1", attempt_id)
    }

    /// The store id for a credential `generation`: a distinct domain-separated hash. Used for the
    /// durable (post-credential) phase, so startup crash recovery can find the journal by the
    /// bundle's generation after the in-memory attempt id is gone (§6 windows 2/3, §9 projection).
    fn journal_store_id_for_generation(generation: &str) -> String {
        Self::hashed_store_id("isyncyou-onboarding-generation-v1", generation)
    }

    fn hashed_store_id(domain: &str, value: &str) -> String {
        let d = ring::digest::digest(
            &ring::digest::SHA256,
            format!("{domain}:{value}").as_bytes(),
        );
        format!(
            "journal:{}",
            d.as_ref()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        )
    }

    fn has_state(&self, state: ProductOnboardingState) -> bool {
        self.transitions.iter().any(|t| t.state == state)
    }

    /// Append a transition, keeping only the most recent `MAX_JOURNAL_TRANSITIONS` (compaction).
    fn push(&mut self, transition: OnboardingTransition) {
        self.transitions.push(transition);
        let len = self.transitions.len();
        if len > MAX_JOURNAL_TRANSITIONS {
            self.transitions.drain(0..len - MAX_JOURNAL_TRANSITIONS);
        }
    }

    fn to_json(&self) -> Vec<u8> {
        let arr: Vec<serde_json::Value> = self
            .transitions
            .iter()
            .map(|t| {
                serde_json::json!({
                    "state": t.state.wire(),
                    "generation": t.generation,
                    "error_code": t.error_code,
                })
            })
            .collect();
        serde_json::to_vec(&serde_json::json!({ "schema_version": 1, "transitions": arr }))
            .unwrap_or_default()
    }

    fn from_json(raw: &[u8]) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_slice(raw).ok()?;
        if v.get("schema_version").and_then(|x| x.as_u64()) != Some(1) {
            return None;
        }
        let mut transitions = Vec::new();
        for t in v.get("transitions").and_then(|x| x.as_array())? {
            let state = onboarding_state_from_wire(t.get("state").and_then(|x| x.as_str())?)?;
            transitions.push(OnboardingTransition {
                state,
                generation: t
                    .get("generation")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                error_code: t
                    .get("error_code")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string()),
            });
        }
        Some(Self { transitions })
    }
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn onboarding_state_from_wire(wire: &str) -> Option<ProductOnboardingState> {
    use ProductOnboardingState::*;
    Some(match wire {
        "not_started" => NotStarted,
        "official_sign_in_started" => OfficialSignInStarted,
        "official_oauth_completed" => OfficialOauthCompleted,
        "credential_encrypted" => CredentialEncrypted,
        "retained_envelope_verified" => RetainedEnvelopeVerified,
        "default_harness_removed" => DefaultHarnessRemoved,
        "m365_profile_activated" => M365ProfileActivated,
        "isyncyou_tool_connected" => IsyncyouToolConnected,
        "subscription_identity_set" => SubscriptionIdentitySet,
        "ready" => Ready,
        "error_redacted" => ErrorRedacted,
        _ => return None,
    })
}

/// Persist an onboarding journal at a precomputed store id (#639) — bounded, authenticated, atomic.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn store_onboarding_journal_at(
    oauth_dir: &Path,
    store_id: &str,
    journal: &OnboardingAttemptJournalV1,
) -> Result<(), String> {
    agent_credential_store(oauth_dir)?
        .put(
            isyncyou_agent::SecretClass::OnboardingAttemptJournal,
            store_id,
            &isyncyou_agent::Secret::new(journal.to_json()),
        )
        .map_err(credential_store_error)
}

/// Load an onboarding journal at a precomputed store id (#639) with hard size bounds.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn load_onboarding_journal_at(
    oauth_dir: &Path,
    store_id: &str,
) -> Option<OnboardingAttemptJournalV1> {
    if !agent_credential_store_exists(oauth_dir) {
        return None;
    }
    let store = agent_credential_store(oauth_dir).ok()?;
    let secret = store
        .get_bounded(
            isyncyou_agent::SecretClass::OnboardingAttemptJournal,
            store_id,
            MAX_JOURNAL_ENVELOPE_BYTES,
            MAX_JOURNAL_PLAINTEXT_BYTES,
        )
        .ok()??;
    OnboardingAttemptJournalV1::from_json(secret.expose())
}

/// Persist the onboarding attempt journal (#639), keyed by the opaque UI attempt id.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn store_onboarding_journal(
    oauth_dir: &Path,
    attempt_id: &str,
    journal: &OnboardingAttemptJournalV1,
) -> Result<(), String> {
    store_onboarding_journal_at(
        oauth_dir,
        &OnboardingAttemptJournalV1::journal_store_id(attempt_id),
        journal,
    )
}

/// Load the onboarding attempt journal (#639), keyed by the opaque UI attempt id.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn load_onboarding_journal(
    oauth_dir: &Path,
    attempt_id: &str,
) -> Option<OnboardingAttemptJournalV1> {
    load_onboarding_journal_at(
        oauth_dir,
        &OnboardingAttemptJournalV1::journal_store_id(attempt_id),
    )
}

/// The ordered onboarding chain recorded once a successful official OAuth is committed and the
/// product is activated (#639): official OAuth completed -> credential encrypted -> retained
/// envelope verified -> default harness removed -> M365 profile activated -> iSyncYou tool
/// connected -> subscription identity set -> ready.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
const ONBOARDING_SUCCESS_CHAIN: [ProductOnboardingState; 8] = [
    ProductOnboardingState::OfficialOauthCompleted,
    ProductOnboardingState::CredentialEncrypted,
    ProductOnboardingState::RetainedEnvelopeVerified,
    ProductOnboardingState::DefaultHarnessRemoved,
    ProductOnboardingState::M365ProfileActivated,
    ProductOnboardingState::IsyncyouToolConnected,
    ProductOnboardingState::SubscriptionIdentitySet,
    ProductOnboardingState::Ready,
];

/// Append an in-flight transition to the attempt-keyed journal (#639). Best-effort: the journal is
/// evidence/recovery only, never the readiness authority, so a write failure never blocks the flow.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn record_onboarding_attempt_transition(
    oauth_dir: &Path,
    attempt_id: &str,
    state: ProductOnboardingState,
    error_code: Option<String>,
) {
    let mut journal =
        load_onboarding_journal(oauth_dir, attempt_id).unwrap_or(OnboardingAttemptJournalV1 {
            transitions: vec![],
        });
    journal.push(OnboardingTransition {
        state,
        generation: String::new(),
        error_code,
    });
    let _ = store_onboarding_journal(oauth_dir, attempt_id, &journal);
}

/// Append one or more ordered transitions to the generation-keyed journal (#639), skipping any that
/// are already present so recovery is idempotent. Best-effort (evidence only).
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn record_onboarding_generation_transitions(
    oauth_dir: &Path,
    generation: &str,
    states: &[ProductOnboardingState],
) {
    let store_id = OnboardingAttemptJournalV1::journal_store_id_for_generation(generation);
    let mut journal =
        load_onboarding_journal_at(oauth_dir, &store_id).unwrap_or(OnboardingAttemptJournalV1 {
            transitions: vec![],
        });
    let mut changed = false;
    for &state in states {
        if !journal.has_state(state) {
            journal.push(OnboardingTransition {
                state,
                generation: generation.to_string(),
                error_code: None,
            });
            changed = true;
        }
    }
    if changed {
        let _ = store_onboarding_journal_at(oauth_dir, &store_id, &journal);
    }
}

/// Try to acquire the exclusive product-runtime lock (#639). `Ok(None)` -> another product runtime
/// holds it; the caller fails closed. Held for the returned guard's lifetime.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
#[allow(dead_code)]
fn try_acquire_product_runtime_lock(
    oauth_dir: &Path,
) -> std::io::Result<Option<isyncyou_agent::FileLock>> {
    isyncyou_agent::FileLock::try_acquire_exclusive(&oauth_dir.join(".product-runtime.lock"))
}

/// The Codex/ChatGPT credential we persist (access + refresh + ChatGPT account id + expiry).
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
#[derive(Clone)]
struct CodexStoredCredential {
    access_token: String,
    refresh_token: String,
    account_id: String,
    expires_at_ms: u64,
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderCredentialOrigin {
    ProductCredentialStore,
    ExperimentalLocalCli,
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
enum ProductCredentialState<T> {
    Absent,
    PresentValid(T),
    PresentNeedsRefresh(T),
    PresentInvalid,
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderKind {
    Claude,
    Codex,
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
enum ResolvedProviderCredential {
    Claude {
        origin: ProviderCredentialOrigin,
        credential: StoredCredential,
    },
    Codex {
        origin: ProviderCredentialOrigin,
        credential: CodexStoredCredential,
    },
    Unconfigured(ProviderKind),
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderCredentialResolutionError {
    ProductReconnectRequired,
    #[cfg(feature = "agent-subscription-experimental")]
    ExperimentalUnsupportedPlatform,
    #[cfg(feature = "agent-subscription-experimental")]
    ExperimentalCredentialRejected,
    ProviderUnavailable,
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
impl std::fmt::Display for ProviderCredentialResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self {
            Self::ProductReconnectRequired => "product_reconnect_required",
            #[cfg(feature = "agent-subscription-experimental")]
            Self::ExperimentalUnsupportedPlatform => "experimental_platform_unsupported",
            #[cfg(feature = "agent-subscription-experimental")]
            Self::ExperimentalCredentialRejected => "experimental_credential_rejected",
            Self::ProviderUnavailable => "provider_unavailable",
        };
        write!(f, "agent provider unavailable: {reason}")
    }
}

#[cfg(all(
    test,
    any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    )
))]
fn credential_resolution_error_provider() -> Box<dyn isyncyou_agent::LlmProvider + Send> {
    Box::new(CredentialResolutionErrorProvider)
}

#[cfg(all(
    test,
    any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    )
))]
struct CredentialResolutionErrorProvider;

#[cfg(all(
    test,
    any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    )
))]
impl isyncyou_agent::LlmProvider for CredentialResolutionErrorProvider {
    fn name(&self) -> &str {
        "credential-resolution-error"
    }

    fn next(
        &mut self,
        _history: &[isyncyou_agent::Message],
        _emit: &mut dyn FnMut(isyncyou_agent::StreamEvent),
    ) -> Result<Vec<isyncyou_agent::AssistantBlock>, isyncyou_agent::AgentError> {
        Err(isyncyou_agent::AgentError::Provider(
            "the connected provider must be reconnected".to_string(),
        ))
    }
}

#[cfg(all(test, feature = "agent-subscription-experimental"))]
fn resolve_product_or_local<T>(
    state: ProductCredentialState<T>,
    refresh: impl FnOnce(T) -> Result<T, ProviderCredentialResolutionError>,
    local: impl FnOnce() -> Result<Option<T>, ProviderCredentialResolutionError>,
) -> Result<Option<(ProviderCredentialOrigin, T)>, ProviderCredentialResolutionError> {
    match state {
        ProductCredentialState::Absent => local().map(|credential| {
            credential
                .map(|credential| (ProviderCredentialOrigin::ExperimentalLocalCli, credential))
        }),
        ProductCredentialState::PresentValid(credential) => Ok(Some((
            ProviderCredentialOrigin::ProductCredentialStore,
            credential,
        ))),
        ProductCredentialState::PresentNeedsRefresh(credential) => refresh(credential)
            .map(|credential| Some((ProviderCredentialOrigin::ProductCredentialStore, credential))),
        ProductCredentialState::PresentInvalid => {
            Err(ProviderCredentialResolutionError::ProductReconnectRequired)
        }
    }
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn credential_needs_refresh_at(access_token: &str, expires_at_ms: u64, now_ms: u64) -> bool {
    access_token.is_empty()
        || expires_at_ms == 0
        || expires_at_ms <= now_ms.saturating_add(5 * 60 * 1000)
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn product_credential_state_wire<T>(state: &ProductCredentialState<T>) -> &'static str {
    match state {
        ProductCredentialState::Absent => "unconfigured",
        ProductCredentialState::PresentValid(_) => "connected",
        ProductCredentialState::PresentNeedsRefresh(_) => "refresh_required",
        ProductCredentialState::PresentInvalid => "reconnect_required",
    }
}

/// Default-off device evidence seam. It can only be armed through the JNI-only mobile
/// test hook and forces one real Codex refresh attempt without changing the credential
/// clock or exposing credential metadata to WebView/HTTP callers.
#[cfg(feature = "agent-network-device-test-hooks")]
static FORCE_CODEX_REFRESH_FOR_DEVICE_TEST: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "agent-network-device-test-hooks")]
pub fn arm_codex_refresh_for_device_test() {
    FORCE_CODEX_REFRESH_FOR_DEVICE_TEST.store(true, Ordering::SeqCst);
}

#[cfg(feature = "agent-network-device-test-hooks")]
fn codex_refresh_for_device_test_is_armed() -> bool {
    FORCE_CODEX_REFRESH_FOR_DEVICE_TEST.load(Ordering::SeqCst)
}

#[cfg(feature = "agent-network-device-test-hooks")]
fn take_codex_refresh_for_device_test() -> bool {
    FORCE_CODEX_REFRESH_FOR_DEVICE_TEST.swap(false, Ordering::SeqCst)
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn classify_claude_product_credential_at(
    credential: StoredCredential,
    now_ms: u64,
) -> ProductCredentialState<StoredCredential> {
    if credential.access_token.is_empty() && credential.refresh_token.is_empty() {
        ProductCredentialState::PresentInvalid
    } else if credential_needs_refresh_at(
        &credential.access_token,
        credential.expires_at_ms,
        now_ms,
    ) {
        ProductCredentialState::PresentNeedsRefresh(credential)
    } else {
        ProductCredentialState::PresentValid(credential)
    }
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn classify_codex_product_credential_at(
    credential: CodexStoredCredential,
    now_ms: u64,
) -> ProductCredentialState<CodexStoredCredential> {
    if credential.account_id.trim().is_empty()
        || (credential.access_token.is_empty() && credential.refresh_token.is_empty())
    {
        ProductCredentialState::PresentInvalid
    } else if credential_needs_refresh_at(
        &credential.access_token,
        credential.expires_at_ms,
        now_ms,
    ) {
        ProductCredentialState::PresentNeedsRefresh(credential)
    } else {
        ProductCredentialState::PresentValid(credential)
    }
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn complete_claude_refresh(
    current: StoredCredential,
    refreshed: isyncyou_agent::oauth::RefreshedToken,
    now_ms: u64,
) -> Result<StoredCredential, ProviderCredentialResolutionError> {
    let refresh_token = if refreshed.refresh_token.is_empty() {
        current.refresh_token
    } else {
        refreshed.refresh_token
    };
    let lifetime_ms = refreshed
        .expires_in
        .checked_mul(1000)
        .filter(|lifetime| *lifetime > 0)
        .ok_or(ProviderCredentialResolutionError::ProductReconnectRequired)?;
    let expires_at_ms = now_ms
        .checked_add(lifetime_ms)
        .ok_or(ProviderCredentialResolutionError::ProductReconnectRequired)?;
    if refreshed.access_token.trim().is_empty() || refresh_token.trim().is_empty() {
        return Err(ProviderCredentialResolutionError::ProductReconnectRequired);
    }
    Ok(StoredCredential {
        access_token: refreshed.access_token,
        refresh_token,
        expires_at_ms,
    })
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn complete_codex_refresh(
    current: CodexStoredCredential,
    refreshed: isyncyou_agent::oauth::CodexTokens,
    now_ms: u64,
) -> Result<CodexStoredCredential, ProviderCredentialResolutionError> {
    let refresh_token = if refreshed.refresh_token.is_empty() {
        current.refresh_token
    } else {
        refreshed.refresh_token
    };
    let account_id = if refreshed.account_id.is_empty() {
        current.account_id
    } else if refreshed.account_id != current.account_id {
        // #639: the ChatGPT account identity changed under a refresh — never silently switch or
        // rotate the credential; force a fresh official OAuth (reconnect).
        return Err(ProviderCredentialResolutionError::ProductReconnectRequired);
    } else {
        refreshed.account_id
    };
    let lifetime_ms = refreshed
        .expires_in
        .checked_mul(1000)
        .filter(|lifetime| *lifetime > 0)
        .ok_or(ProviderCredentialResolutionError::ProductReconnectRequired)?;
    let expires_at_ms = now_ms
        .checked_add(lifetime_ms)
        .ok_or(ProviderCredentialResolutionError::ProductReconnectRequired)?;
    if refreshed.access_token.trim().is_empty()
        || refresh_token.trim().is_empty()
        || account_id.trim().is_empty()
    {
        return Err(ProviderCredentialResolutionError::ProductReconnectRequired);
    }
    Ok(CodexStoredCredential {
        access_token: refreshed.access_token,
        refresh_token,
        account_id,
        expires_at_ms,
    })
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
impl CodexStoredCredential {
    fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "access_token": self.access_token,
            "refresh_token": self.refresh_token,
            "account_id": self.account_id,
            "expires_at_ms": self.expires_at_ms,
        }))
        .unwrap_or_default()
    }
    fn from_json(raw: &[u8]) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_slice(raw).ok()?;
        Some(Self {
            access_token: v.get("access_token")?.as_str()?.to_string(),
            refresh_token: v
                .get("refresh_token")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            account_id: v
                .get("account_id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            expires_at_ms: v.get("expires_at_ms").and_then(|x| x.as_u64()).unwrap_or(0),
        })
    }
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
const SUBSCRIPTION_CREDENTIAL_ID: &str = "subscription";

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
const CODEX_CREDENTIAL_ID: &str = "codex";

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn credential_store_error(e: impl std::fmt::Display) -> String {
    let raw = e.to_string();
    let redacted = isyncyou_core::obs::redact(&raw);
    if redacted != raw {
        format!("agent credential store error: {redacted}")
    } else {
        "agent credential store error".to_string()
    }
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn agent_credential_config(oauth_dir: &Path) -> isyncyou_agent::CredentialStoreConfig {
    isyncyou_agent::CredentialStoreConfig::new(oauth_dir)
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn agent_credential_store(
    oauth_dir: &Path,
) -> Result<isyncyou_agent::AgentCredentialStore, String> {
    isyncyou_agent::CredentialStoreResolver::new(agent_credential_config(oauth_dir))
        .resolve()
        .map_err(credential_store_error)
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn agent_credential_store_exists(oauth_dir: &Path) -> bool {
    agent_credential_config(oauth_dir).store_dir().exists()
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn store_agent_credential_blob(oauth_dir: &Path, id: &str, bytes: Vec<u8>) -> Result<(), String> {
    let store = agent_credential_store(oauth_dir)?;
    store
        .put(
            isyncyou_agent::SecretClass::ProviderOAuthRefresh,
            id,
            &isyncyou_agent::Secret::new(bytes),
        )
        .map_err(credential_store_error)
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn load_agent_credential_blob(
    oauth_dir: &Path,
    id: &str,
) -> Result<Option<isyncyou_agent::Secret>, String> {
    if !agent_credential_store_exists(oauth_dir) {
        return Ok(None);
    }
    agent_credential_store(oauth_dir)?
        .get(isyncyou_agent::SecretClass::ProviderOAuthRefresh, id)
        .map_err(credential_store_error)
}

/// Persist a Codex credential to the encrypted store under `oauth_dir` (id `codex`).
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn store_codex_blob(oauth_dir: &Path, cred: &CodexStoredCredential) -> Result<(), String> {
    // A bare `store_codex_blob` is a fresh login: mint a new generation (#639), then activate the
    // product path for that generation (attest -> durable ProductActivationV1).
    let meta = ProductBundleMeta::fresh(ProductProviderId::Codex);
    store_codex_bundle(oauth_dir, cred, &meta)?;
    activate_product(oauth_dir, ProductProviderId::Codex, &meta.generation)
}

/// Persist a Codex credential with explicit V2 metadata (#639) — used by refresh to preserve the
/// credential `generation` and to persist a `ReconnectRequired` lifecycle fail-closed.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn store_codex_bundle(
    oauth_dir: &Path,
    cred: &CodexStoredCredential,
    meta: &ProductBundleMeta,
) -> Result<(), String> {
    store_agent_credential_blob(oauth_dir, CODEX_CREDENTIAL_ID, meta.to_blob(cred.to_json()))
}

/// Map a product provider to the agent crate's harness attestation discriminant (#639).
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn harness_provider_for(provider: ProductProviderId) -> isyncyou_agent::HarnessProvider {
    match provider {
        ProductProviderId::Claude => isyncyou_agent::HarnessProvider::Claude,
        ProductProviderId::Codex => isyncyou_agent::HarnessProvider::Codex,
    }
}

/// #639: turn a freshly persisted, successful-OAuth credential into product readiness. This is the
/// `attestation -> ProductActivation` step of the onboarding contract: attest the SHIPPED harness
/// against `HARNESS_CONTRACT_VERSION`, then persist a durable `ProductActivationV1` binding this
/// credential `generation` to the official policy fingerprint and the harness contract. A harness
/// that has drifted from the contract refuses activation (fail-closed) — the provider stays not
/// ready. A refresh keeps the same generation and does NOT re-activate (the activation still
/// matches); only a fresh login mints a new generation and activates it.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn activate_product(
    oauth_dir: &Path,
    provider: ProductProviderId,
    generation: &str,
) -> Result<(), String> {
    isyncyou_agent::attest_static_product_harness(harness_provider_for(provider))
        .map_err(|_| "harness_attestation_failed".to_string())?;
    let activation = ProductActivationV1 {
        provider_id: provider.wire().to_string(),
        credential_generation: generation.to_string(),
        oauth_policy_fingerprint: oauth_policy_fingerprint(provider),
        harness_contract_version: isyncyou_agent::HARNESS_CONTRACT_VERSION,
    };
    store_product_activation(oauth_dir, provider, &activation)
}

/// Load the V2 metadata for a persisted product credential (#639); `None` if absent or legacy.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn load_product_bundle_meta(oauth_dir: &Path, id: &str) -> Option<ProductBundleMeta> {
    match load_agent_credential_blob(oauth_dir, id) {
        Ok(Some(secret)) => ProductBundleMeta::from_blob(secret.expose()),
        _ => None,
    }
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn store_agent_provider_selection(
    oauth_dir: &Path,
    provider: &str,
    model: &str,
) -> Result<(), String> {
    let known = match ProductProviderId::parse(provider) {
        Some(ProductProviderId::Claude) => CLAUDE_MODELS,
        Some(ProductProviderId::Codex) => CODEX_MODELS,
        None => return Err("unknown provider".into()),
    };
    if !known.iter().any(|(id, _)| *id == model) {
        return Err("unknown model for provider".into());
    }
    std::fs::create_dir_all(oauth_dir).map_err(|e| e.to_string())?;
    let blob = serde_json::to_vec(&serde_json::json!({
        "provider": provider,
        "model": model,
    }))
    .map_err(|e| e.to_string())?;
    std::fs::write(oauth_dir.join("agent-settings.json"), blob).map_err(|e| e.to_string())
}

/// Minimal percent-decode for the loopback callback query (`+`→space, `%XX`→byte).
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hi = (b[i + 1] as char).to_digit(16);
                let lo = (b[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
const CODEX_OK_HTML: &str = "<!doctype html><meta charset=utf-8><title>ChatGPT connected</title>\
<body style=\"font-family:system-ui;background:#0b0d12;color:#e8eaf0;display:flex;min-height:100vh;\
align-items:center;justify-content:center;margin:0\"><div style=text-align:center><h1>Connected</h1>\
<p style=color:#9aa3b2>ChatGPT is now linked. Close this tab and return to iSyncYou.</p></div>";

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
const CODEX_ERR_HTML: &str = "<!doctype html><meta charset=utf-8><title>Sign-in failed</title>\
<body style=\"font-family:system-ui;background:#0b0d12;color:#e8eaf0;display:flex;min-height:100vh;\
align-items:center;justify-content:center;margin:0\"><div style=text-align:center><h1>Sign-in failed</h1>\
<p style=color:#9aa3b2>Please return to iSyncYou and try connecting ChatGPT again.</p></div>";

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
const CODEX_CALLBACK_DIAGNOSTICS_FILE: &str = "codex-debug.txt";

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
/// One-shot loopback callback server for the Codex OAuth (OpenAI registers the fixed
/// `:1455` redirect). Waits for the browser to hit `/auth/callback?code=&state=`, verifies
/// the CSRF `state`, exchanges the code, and persists the credential. The background
/// thread uses the same bounded lifetime as its owning OAuth attempt. It never persists
/// callback diagnostics or target data.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
struct CodexCallbackContext {
    oauth_dir: std::path::PathBuf,
    cfg: isyncyou_agent::oauth::CodexOAuthConfig,
    verifier: String,
    want_state: String,
    attempt_id: String,
    cancelled: Arc<AtomicBool>,
    attempts: Arc<Mutex<HashMap<String, OAuthAttempt>>>,
    /// #639 T8: the shared product-runtime gate. The callback holds it while it writes the
    /// credential + activation + journal so a concurrent status/turn read cannot observe a
    /// half-written revision; it is NOT held during the browser sign-in wait.
    product_runtime_gate: Arc<Mutex<()>>,
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn codex_callback_serve(listener: std::net::TcpListener, context: CodexCallbackContext) {
    codex_callback_serve_until(
        listener,
        context,
        std::time::Instant::now() + OAUTH_ATTEMPT_TTL,
    );
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn codex_callback_serve_until(
    listener: std::net::TcpListener,
    context: CodexCallbackContext,
    deadline: std::time::Instant,
) {
    use std::io::{Read, Write};
    let CodexCallbackContext {
        oauth_dir,
        cfg,
        verifier,
        want_state,
        attempt_id,
        cancelled,
        attempts,
        product_runtime_gate,
    } = context;
    if listener.set_nonblocking(true).is_err() {
        return;
    }
    let mut completed = false;
    while std::time::Instant::now() < deadline && !cancelled.load(Ordering::Acquire) {
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
            Err(_) => break,
        };
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(15)));
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        let first_line = req.lines().next().unwrap_or("");
        let mut parts = first_line.split_whitespace();
        let method = parts.next().unwrap_or("");
        let target = parts.next().unwrap_or("");
        if method != "GET" || !target.starts_with("/auth/callback") {
            let _ = stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
            continue; // ignore favicon/others, keep waiting for the real callback
        }
        let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
        let (mut code, mut state) = (String::new(), String::new());
        for pair in query.split('&') {
            match pair.split_once('=') {
                Some(("code", v)) => code = pct_decode(v),
                Some(("state", v)) => state = pct_decode(v),
                _ => {}
            }
        }
        let ok = if state == want_state && !code.is_empty() {
            match isyncyou_agent::http::HttpTransport::shared()
                .map_err(|e| e.to_string())
                .and_then(|http| {
                    isyncyou_agent::oauth::codex_exchange(&http, &cfg, &code, &verifier)
                        .map_err(|e| e.to_string())
                }) {
                Ok(tok) => {
                    let expires_at_ms = if tok.expires_in > 0 {
                        now_ms() + tok.expires_in * 1000
                    } else {
                        0
                    };
                    // #639 T8: hold the shared product-runtime gate across the credential write +
                    // activation + journal so status/turn cannot read a half-written revision.
                    let _gate = product_runtime_gate.lock();
                    let stored = store_codex_blob(
                        &oauth_dir,
                        &CodexStoredCredential {
                            access_token: tok.access_token,
                            refresh_token: tok.refresh_token,
                            account_id: tok.account_id,
                            expires_at_ms,
                        },
                    )
                    .and_then(|_| {
                        store_agent_provider_selection(
                            &oauth_dir,
                            "codex",
                            &isyncyou_agent::CodexConfig::default().model,
                        )
                    })
                    .is_ok();
                    if stored {
                        let generation = load_product_bundle_meta(&oauth_dir, CODEX_CREDENTIAL_ID)
                            .map(|meta| meta.generation)
                            .unwrap_or_default();
                        record_onboarding_generation_transitions(
                            &oauth_dir,
                            &generation,
                            &ONBOARDING_SUCCESS_CHAIN,
                        );
                        completed = true;
                    }
                    stored
                }
                Err(_) => false,
            }
        } else {
            false
        };
        let body = if ok { CODEX_OK_HTML } else { CODEX_ERR_HTML };
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes());
        break;
    }
    // #639 T8: an attempt that ended without completing and was not user-cancelled (timeout /
    // stale callback / lost verifier) is a terminal, redacted transition — never resumed.
    if !completed && !cancelled.load(Ordering::Acquire) {
        record_onboarding_attempt_transition(
            &oauth_dir,
            &attempt_id,
            ProductOnboardingState::ErrorRedacted,
            Some("interrupted".to_string()),
        );
    }
    // A stale callback thread may only clear the exact attempt it owns. A newer login
    // therefore cannot be erased by a late timeout/cancellation path.
    let mut attempts = attempts.lock().unwrap();
    if matches!(attempts.get(&attempt_id), Some(OAuthAttempt::Codex { cancelled: current, .. }) if Arc::ptr_eq(current, &cancelled))
    {
        attempts.remove(&attempt_id);
    }
}

/// Product Claude/Codex OAuth runtime (#623). `agent-oauth-providers` owns the app OAuth
/// credential path; `agent-subscription-experimental` layers the local CLI fallback/capture
/// surface tracked by #627.
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
impl DaemonAgent {
    /// A human-facing success page shown in the **system browser** after the callback.
    const OAUTH_SUCCESS_HTML: &'static str = "<!doctype html><html><head><meta charset=utf-8>\
<meta name=viewport content=\"width=device-width,initial-scale=1\">\
<title>iSyncYou connected</title><style>body{font-family:system-ui;background:#0b0d12;color:#e8eaf0;\
display:flex;min-height:100vh;align-items:center;justify-content:center;margin:0}\
.c{text-align:center;max-width:22rem;padding:2rem}h1{font-size:1.4rem;margin:.5rem 0}\
p{color:#9aa3b2;line-height:1.5}</style></head><body><div class=c>\
<h1>Connected</h1><p>This device is now authorized. You can close this tab and return to iSyncYou.</p>\
</div></body></html>";

    /// Load the compiled official Claude OAuth recipe. A local `agent-oauth.json` may
    /// assert the same tuple for development diagnostics, but it cannot override any
    /// endpoint, client, scope, or redirect used by the product flow.
    fn load_oauth_config(&self) -> Result<isyncyou_agent::OAuthConfig, String> {
        let path = self.oauth_dir.join("agent-oauth.json");
        if path.exists() {
            let source = std::fs::read_to_string(&path)
                .map_err(|_| "OAuth recipe is unavailable".to_string())?;
            let candidate: isyncyou_agent::OAuthConfig =
                serde_json::from_str(&source).map_err(|_| "OAuth recipe is invalid".to_string())?;
            let official = isyncyou_agent::OAuthConfig::default();
            if candidate.authorize_url != official.authorize_url
                || candidate.token_url != official.token_url
                || candidate.client_id != official.client_id
                || candidate.scopes != official.scopes
                || candidate.manual_redirect_url != official.manual_redirect_url
            {
                return Err("OAuth recipe does not match the official product policy".to_string());
            }
            Ok(official)
        } else {
            Ok(isyncyou_agent::OAuthConfig::default())
        }
    }

    /// Persist a subscription credential (access + refresh + expiry) at rest under a
    /// device-local key, so the daemon can refresh the access token itself.
    fn store_credential(&self, cred: &StoredCredential) -> Result<(), String> {
        // A bare `store_credential` is a fresh login: mint a new generation (#639), then activate
        // the product path for that generation (attest -> durable ProductActivationV1).
        let meta = ProductBundleMeta::fresh(ProductProviderId::Claude);
        self.store_claude_bundle(cred, &meta)?;
        activate_product(&self.oauth_dir, ProductProviderId::Claude, &meta.generation)
    }

    /// Persist a Claude credential with explicit V2 metadata (#639) — used by refresh to preserve
    /// the credential `generation` and to persist a `ReconnectRequired` lifecycle fail-closed.
    fn store_claude_bundle(
        &self,
        cred: &StoredCredential,
        meta: &ProductBundleMeta,
    ) -> Result<(), String> {
        store_agent_credential_blob(
            &self.oauth_dir,
            SUBSCRIPTION_CREDENTIAL_ID,
            meta.to_blob(cred.to_json()),
        )
    }

    /// Persist the FULL token set from the OAuth code exchange (access + refresh + expiry) so
    /// `fresh_access_token` can self-refresh before the ~8h subscription token expires
    /// (LIVE-verified 2026-07-01 — without the refresh token the client "connection-lost"s
    /// every ~8h with no way to renew).
    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn store_token(&self, token: &isyncyou_agent::oauth::RefreshedToken) -> Result<(), String> {
        let expires_at_ms = if token.expires_in > 0 {
            now_ms() + token.expires_in * 1000
        } else {
            0
        };
        self.store_credential(&StoredCredential {
            access_token: token.access_token.clone(),
            refresh_token: token.refresh_token.clone(),
            expires_at_ms,
        })
    }

    /// The persisted provider+model selection (the switcher), falling back to the env
    /// override then the in-repo default. Stored next to the credential store.
    fn agent_settings(&self) -> (String, String) {
        let default_provider = if std::env::var("ISYNCYOU_AGENT_PROVIDER").as_deref() == Ok("codex")
        {
            "codex"
        } else {
            "claude"
        };
        if let Ok(s) = std::fs::read_to_string(self.oauth_dir.join("agent-settings.json")) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                let provider = v
                    .get("provider")
                    .and_then(|x| x.as_str())
                    .unwrap_or(default_provider)
                    .to_string();
                let model = v
                    .get("model")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                return (provider, model);
            }
        }
        (default_provider.to_string(), String::new())
    }

    /// The model to use for `provider`: the current selection if it names that provider,
    /// else that provider's default (env override for Claude, in-repo default otherwise).
    fn model_for(&self, provider: &str) -> String {
        let (sel_provider, sel_model) = self.agent_settings();
        if provider == sel_provider && !sel_model.is_empty() {
            return sel_model;
        }
        match ProductProviderId::parse(provider) {
            Some(ProductProviderId::Codex) => isyncyou_agent::CodexConfig::default().model,
            _ => {
                std::env::var("ISYNCYOU_AGENT_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string())
            }
        }
    }

    /// Persist the switcher selection after validating it against the offered models.
    fn set_agent_settings(&self, provider: &str, model: &str) -> Result<(), String> {
        store_agent_provider_selection(&self.oauth_dir, provider, model)
    }

    fn claude_product_credential_state(&self) -> ProductCredentialState<StoredCredential> {
        match load_agent_credential_blob(&self.oauth_dir, SUBSCRIPTION_CREDENTIAL_ID) {
            Ok(None) => ProductCredentialState::Absent,
            Ok(Some(secret)) => {
                // #639: a legacy/incomplete blob (no V2 meta) or one marked ReconnectRequired is
                // un-migratable and fails closed to reconnect — no silent upgrade, no refresh loop.
                match ProductBundleMeta::from_blob(secret.expose()) {
                    Some(meta) if meta.lifecycle == CredentialLifecycle::Active => {
                        StoredCredential::from_json(secret.expose())
                            .map(|c| {
                                classify_claude_product_credential_at(c, (self.credential_now_ms)())
                            })
                            .unwrap_or(ProductCredentialState::PresentInvalid)
                    }
                    _ => ProductCredentialState::PresentInvalid,
                }
            }
            Err(_) => ProductCredentialState::PresentInvalid,
        }
    }

    fn refresh_claude_product_credential_unlocked(
        &self,
        credential: StoredCredential,
    ) -> Result<StoredCredential, ProviderCredentialResolutionError> {
        if credential.refresh_token.is_empty() {
            return Err(ProviderCredentialResolutionError::ProductReconnectRequired);
        }
        let config = self
            .load_oauth_config()
            .map_err(|_| ProviderCredentialResolutionError::ProductReconnectRequired)?;
        let http = isyncyou_agent::http::HttpTransport::shared()
            .map_err(|_| ProviderCredentialResolutionError::ProductReconnectRequired)?;
        // #639: a refresh is not a new login — preserve the credential generation.
        let generation = load_product_bundle_meta(&self.oauth_dir, SUBSCRIPTION_CREDENTIAL_ID)
            .map(|m| m.generation)
            .unwrap_or_else(uuid_v4);
        let policy_fingerprint = oauth_policy_fingerprint(ProductProviderId::Claude);
        let outcome = isyncyou_agent::oauth::refresh(&http, &config, &credential.refresh_token)
            .map_err(|_| ProviderCredentialResolutionError::ProductReconnectRequired)
            .and_then(|refreshed| {
                complete_claude_refresh(credential.clone(), refreshed, (self.credential_now_ms)())
            });
        match outcome {
            Ok(refreshed_credential) => {
                self.store_claude_bundle(
                    &refreshed_credential,
                    &ProductBundleMeta {
                        generation,
                        policy_fingerprint,
                        lifecycle: CredentialLifecycle::Active,
                    },
                )
                .map_err(|_| ProviderCredentialResolutionError::ProductReconnectRequired)?;
                Ok(refreshed_credential)
            }
            Err(e) => {
                // #639: persist ReconnectRequired (fail-closed) so the next status reports reconnect
                // instead of re-attempting a refresh loop on the same expired credential.
                let _ = self.store_claude_bundle(
                    &credential,
                    &ProductBundleMeta {
                        generation,
                        policy_fingerprint,
                        lifecycle: CredentialLifecycle::ReconnectRequired {
                            closed_code: "refresh_failed".into(),
                        },
                    },
                );
                Err(e)
            }
        }
    }

    fn experimental_claude_credential(
        &self,
    ) -> Result<Option<StoredCredential>, ProviderCredentialResolutionError> {
        #[cfg(feature = "agent-subscription-experimental")]
        {
            match local_cli_fallback::load_claude_from_process() {
                Ok(credential) => Ok(Some(StoredCredential {
                    access_token: credential.access_token,
                    refresh_token: String::new(),
                    expires_at_ms: 0,
                })),
                Err(error) if error.is_absent() => Ok(None),
                Err(error) if error.is_unsupported_platform() => {
                    Err(ProviderCredentialResolutionError::ExperimentalUnsupportedPlatform)
                }
                Err(_) => Err(ProviderCredentialResolutionError::ExperimentalCredentialRejected),
            }
        }
        #[cfg(not(feature = "agent-subscription-experimental"))]
        {
            Ok(None)
        }
    }

    fn resolve_claude_credential(
        &self,
    ) -> Result<ResolvedProviderCredential, ProviderCredentialResolutionError> {
        let _refresh = self
            .credential_refresh_gate
            .lock()
            .map_err(|_| ProviderCredentialResolutionError::ProductReconnectRequired)?;
        match self.claude_product_credential_state() {
            ProductCredentialState::PresentValid(credential) => {
                Ok(ResolvedProviderCredential::Claude {
                    origin: ProviderCredentialOrigin::ProductCredentialStore,
                    credential,
                })
            }
            ProductCredentialState::PresentNeedsRefresh(credential) => {
                let credential = self.refresh_claude_product_credential_unlocked(credential)?;
                Ok(ResolvedProviderCredential::Claude {
                    origin: ProviderCredentialOrigin::ProductCredentialStore,
                    credential,
                })
            }
            ProductCredentialState::PresentInvalid => {
                Err(ProviderCredentialResolutionError::ProductReconnectRequired)
            }
            ProductCredentialState::Absent => Ok(match self.experimental_claude_credential()? {
                Some(credential) => ResolvedProviderCredential::Claude {
                    origin: ProviderCredentialOrigin::ExperimentalLocalCli,
                    credential,
                },
                None => ResolvedProviderCredential::Unconfigured(ProviderKind::Claude),
            }),
        }
    }

    /// The minimized Claude request config. Local client account/device metadata is not
    /// imported: #627 supplies credentials only and leaves all default-client harness
    /// state outside iSyncYou.
    fn subscription_config(&self) -> isyncyou_agent::SubscriptionConfig {
        isyncyou_agent::SubscriptionConfig::default()
    }

    /// Build the Claude provider from one origin-bound credential bundle.
    fn try_subscription_provider(
        &self,
        system: &str,
    ) -> Result<
        Option<Box<dyn isyncyou_agent::LlmProvider + Send>>,
        ProviderCredentialResolutionError,
    > {
        let credential = match self.resolve_claude_credential()? {
            ResolvedProviderCredential::Claude { origin, credential } => {
                let _credential_origin = origin;
                credential
            }
            ResolvedProviderCredential::Unconfigured(ProviderKind::Claude) => return Ok(None),
            _ => return Err(ProviderCredentialResolutionError::ProviderUnavailable),
        };
        let p = isyncyou_agent::SubscriptionProvider::new(
            credential.access_token,
            self.model_for("claude"),
            system,
            self.subscription_config(),
        )
        .map_err(|_| ProviderCredentialResolutionError::ProviderUnavailable)?;
        Ok(Some(Box::new(p)))
    }

    fn codex_product_credential_state(&self) -> ProductCredentialState<CodexStoredCredential> {
        match load_agent_credential_blob(&self.oauth_dir, CODEX_CREDENTIAL_ID) {
            Ok(None) => ProductCredentialState::Absent,
            Ok(Some(secret)) => {
                // #639: legacy/incomplete or ReconnectRequired -> fail closed to reconnect.
                match ProductBundleMeta::from_blob(secret.expose()) {
                    Some(meta) if meta.lifecycle == CredentialLifecycle::Active => {
                        CodexStoredCredential::from_json(secret.expose())
                            .map(|c| {
                                classify_codex_product_credential_at(c, (self.credential_now_ms)())
                            })
                            .unwrap_or(ProductCredentialState::PresentInvalid)
                    }
                    _ => ProductCredentialState::PresentInvalid,
                }
            }
            Err(_) => ProductCredentialState::PresentInvalid,
        }
    }

    fn refresh_codex_product_credential_unlocked(
        &self,
        credential: CodexStoredCredential,
    ) -> Result<CodexStoredCredential, ProviderCredentialResolutionError> {
        if credential.refresh_token.is_empty() {
            return Err(ProviderCredentialResolutionError::ProductReconnectRequired);
        }
        let config = isyncyou_agent::oauth::CodexOAuthConfig::default();
        let http = isyncyou_agent::http::HttpTransport::shared()
            .map_err(|_| ProviderCredentialResolutionError::ProductReconnectRequired)?;
        // #639: preserve the credential generation across a refresh (a refresh is not a new login).
        let generation = load_product_bundle_meta(&self.oauth_dir, CODEX_CREDENTIAL_ID)
            .map(|m| m.generation)
            .unwrap_or_else(uuid_v4);
        let policy_fingerprint = oauth_policy_fingerprint(ProductProviderId::Codex);
        let outcome =
            isyncyou_agent::oauth::codex_refresh(&http, &config, &credential.refresh_token)
                .map_err(|_| ProviderCredentialResolutionError::ProductReconnectRequired)
                .and_then(|refreshed| {
                    complete_codex_refresh(
                        credential.clone(),
                        refreshed,
                        (self.credential_now_ms)(),
                    )
                });
        match outcome {
            Ok(refreshed_credential) => {
                store_codex_bundle(
                    &self.oauth_dir,
                    &refreshed_credential,
                    &ProductBundleMeta {
                        generation,
                        policy_fingerprint,
                        lifecycle: CredentialLifecycle::Active,
                    },
                )
                .map_err(|_| ProviderCredentialResolutionError::ProductReconnectRequired)?;
                Ok(refreshed_credential)
            }
            Err(e) => {
                // #639: persist ReconnectRequired (fail-closed) — no refresh loop.
                let _ = store_codex_bundle(
                    &self.oauth_dir,
                    &credential,
                    &ProductBundleMeta {
                        generation,
                        policy_fingerprint,
                        lifecycle: CredentialLifecycle::ReconnectRequired {
                            closed_code: "refresh_failed".into(),
                        },
                    },
                );
                Err(e)
            }
        }
    }

    fn experimental_codex_credential(
        &self,
    ) -> Result<Option<CodexStoredCredential>, ProviderCredentialResolutionError> {
        #[cfg(feature = "agent-subscription-experimental")]
        {
            match local_cli_fallback::load_codex_from_process() {
                Ok(credential) => Ok(Some(CodexStoredCredential {
                    access_token: credential.access_token,
                    refresh_token: String::new(),
                    account_id: credential.account_id,
                    expires_at_ms: 0,
                })),
                Err(error) if error.is_absent() => Ok(None),
                Err(error) if error.is_unsupported_platform() => {
                    Err(ProviderCredentialResolutionError::ExperimentalUnsupportedPlatform)
                }
                Err(_) => Err(ProviderCredentialResolutionError::ExperimentalCredentialRejected),
            }
        }
        #[cfg(not(feature = "agent-subscription-experimental"))]
        {
            Ok(None)
        }
    }

    fn resolve_codex_credential(
        &self,
    ) -> Result<ResolvedProviderCredential, ProviderCredentialResolutionError> {
        let _refresh = self
            .credential_refresh_gate
            .lock()
            .map_err(|_| ProviderCredentialResolutionError::ProductReconnectRequired)?;
        match self.codex_product_credential_state() {
            ProductCredentialState::PresentValid(credential) => {
                Ok(ResolvedProviderCredential::Codex {
                    origin: ProviderCredentialOrigin::ProductCredentialStore,
                    credential,
                })
            }
            ProductCredentialState::PresentNeedsRefresh(credential) => {
                let credential = self.refresh_codex_product_credential_unlocked(credential)?;
                Ok(ResolvedProviderCredential::Codex {
                    origin: ProviderCredentialOrigin::ProductCredentialStore,
                    credential,
                })
            }
            ProductCredentialState::PresentInvalid => {
                Err(ProviderCredentialResolutionError::ProductReconnectRequired)
            }
            ProductCredentialState::Absent => Ok(match self.experimental_codex_credential()? {
                Some(credential) => ResolvedProviderCredential::Codex {
                    origin: ProviderCredentialOrigin::ExperimentalLocalCli,
                    credential,
                },
                None => ResolvedProviderCredential::Unconfigured(ProviderKind::Codex),
            }),
        }
    }

    /// Side-effect-free product status. Local CLI fallback is intentionally absent here: it is
    /// not product readiness, and this method must never perform a network refresh.
    fn product_credential_status(&self, provider: &str) -> Result<&'static str, String> {
        match ProductProviderId::parse(provider) {
            Some(ProductProviderId::Claude) => Ok(product_credential_state_wire(
                &self.claude_product_credential_state(),
            )),
            Some(ProductProviderId::Codex) => {
                #[cfg(feature = "agent-network-device-test-hooks")]
                if codex_refresh_for_device_test_is_armed()
                    && !matches!(
                        self.codex_product_credential_state(),
                        ProductCredentialState::Absent | ProductCredentialState::PresentInvalid
                    )
                {
                    return Ok("refresh_required");
                }
                Ok(product_credential_state_wire(
                    &self.codex_product_credential_state(),
                ))
            }
            None => Err("unknown provider".into()),
        }
    }

    /// The only explicit product refresh entry point. A refresh never reads the experimental
    /// local CLI source, and the encrypted store is updated only by the existing atomic writer
    /// after a complete finite credential response has been validated.
    fn refresh_product_credential(&self, provider: &str) -> Result<&'static str, String> {
        let _refresh = self
            .credential_refresh_gate
            .lock()
            .map_err(|_| "reconnect_required".to_string())?;
        match ProductProviderId::parse(provider) {
            Some(ProductProviderId::Claude) => match self.claude_product_credential_state() {
                ProductCredentialState::PresentValid(_) => Ok("connected"),
                ProductCredentialState::PresentNeedsRefresh(credential) => self
                    .refresh_claude_product_credential_unlocked(credential)
                    .map(|_| "connected")
                    .map_err(|_| "reconnect_required".into()),
                ProductCredentialState::Absent | ProductCredentialState::PresentInvalid => {
                    Err("reconnect_required".into())
                }
            },
            Some(ProductProviderId::Codex) => {
                #[cfg(feature = "agent-network-device-test-hooks")]
                let force_refresh = take_codex_refresh_for_device_test();
                #[cfg(not(feature = "agent-network-device-test-hooks"))]
                let force_refresh = false;
                match self.codex_product_credential_state() {
                    ProductCredentialState::PresentValid(credential) if force_refresh => self
                        .refresh_codex_product_credential_unlocked(credential)
                        .map(|_| "connected")
                        .map_err(|_| "reconnect_required".into()),
                    ProductCredentialState::PresentValid(_) => Ok("connected"),
                    ProductCredentialState::PresentNeedsRefresh(credential) => self
                        .refresh_codex_product_credential_unlocked(credential)
                        .map(|_| "connected")
                        .map_err(|_| "reconnect_required".into()),
                    ProductCredentialState::Absent | ProductCredentialState::PresentInvalid => {
                        Err("reconnect_required".into())
                    }
                }
            }
            None => Err("unknown provider".into()),
        }
    }

    /// Start the Codex/ChatGPT OAuth flow: bind OpenAI's fixed
    /// loopback port, spawn a one-shot callback server (exchanges + stores on success),
    /// and return the authorize URL for the system browser. The app polls
    /// `/api/v1/agent/status` for `codex:true`.
    fn codex_oauth_start(&self) -> Result<isyncyou_webui::AgentOAuthStartResponse, String> {
        let cfg = isyncyou_agent::oauth::CodexOAuthConfig::default();
        let (verifier, challenge) = isyncyou_agent::oauth::pkce().map_err(|e| e.to_string())?;
        let state = isyncyou_agent::oauth::rand_state().map_err(|e| e.to_string())?;
        let url = isyncyou_agent::oauth::codex_build_authorize_url(&cfg, &challenge, &state);
        let attempt_id = mint_cap_token();
        let cancelled = Arc::new(AtomicBool::new(false));
        // The registered callback port and active-attempt check are owned together so a
        // second UI start cannot create a competing listener.
        let listener = {
            let mut attempts = self.oauth_attempts.lock().unwrap();
            reap_oauth_attempts(&mut attempts);
            if attempts
                .values()
                .any(|attempt| matches!(attempt, OAuthAttempt::Codex { .. }))
            {
                return Err("ChatGPT sign-in is already in progress".into());
            }
            let listener = std::net::TcpListener::bind(("127.0.0.1", 1455))
                .map_err(|_| "could not open the ChatGPT sign-in port".to_string())?;
            attempts.insert(
                attempt_id.clone(),
                OAuthAttempt::Codex {
                    cancelled: Arc::clone(&cancelled),
                    expires_at: std::time::Instant::now() + OAUTH_ATTEMPT_TTL,
                },
            );
            listener
        };
        // #639 T8: record the official sign-in start into the attempt-keyed journal.
        record_onboarding_attempt_transition(
            &self.oauth_dir,
            &attempt_id,
            ProductOnboardingState::OfficialSignInStarted,
            None,
        );
        let oauth_dir = self.oauth_dir.clone();
        let attempts = Arc::clone(&self.oauth_attempts);
        let callback_attempt_id = attempt_id.clone();
        let product_runtime_gate = Arc::clone(&self.product_runtime_gate);
        std::thread::spawn(move || {
            codex_callback_serve(
                listener,
                CodexCallbackContext {
                    oauth_dir,
                    cfg,
                    verifier,
                    want_state: state,
                    attempt_id: callback_attempt_id,
                    cancelled,
                    attempts,
                    product_runtime_gate,
                },
            )
        });
        Ok(isyncyou_webui::AgentOAuthStartResponse {
            authorize_url: url,
            attempt_id,
        })
    }

    /// Build the Codex (ChatGPT) provider if credentials are available.
    fn try_codex_provider(
        &self,
        instructions: &str,
    ) -> Result<
        Option<Box<dyn isyncyou_agent::LlmProvider + Send>>,
        ProviderCredentialResolutionError,
    > {
        let credential = match self.resolve_codex_credential()? {
            ResolvedProviderCredential::Codex { origin, credential } => {
                let _credential_origin = origin;
                credential
            }
            ResolvedProviderCredential::Unconfigured(ProviderKind::Codex) => return Ok(None),
            _ => return Err(ProviderCredentialResolutionError::ProviderUnavailable),
        };
        let cfg = isyncyou_agent::CodexConfig {
            account_id: credential.account_id,
            model: self.model_for("codex"),
            ..Default::default()
        };
        let provider =
            isyncyou_agent::CodexProvider::new(credential.access_token, instructions, cfg)
                .map_err(|_| ProviderCredentialResolutionError::ProviderUnavailable)?;
        Ok(Some(Box::new(provider)))
    }
}

impl isyncyou_webui::AgentHandler for DaemonAgent {
    fn connectivity_preflight(
        &self,
        request: isyncyou_webui::AgentConnectivityPreflightRequest,
    ) -> Result<isyncyou_webui::AgentConnectivityPreflightResponse, String> {
        self.connectivity_preflight_with_session(request, None)
    }

    fn connectivity_preflight_with_session(
        &self,
        request: isyncyou_webui::AgentConnectivityPreflightRequest,
        session_token: Option<&str>,
    ) -> Result<isyncyou_webui::AgentConnectivityPreflightResponse, String> {
        let provider = isyncyou_agent::ConnectivityProvider::parse(&request.provider)
            .ok_or_else(|| "unknown provider".to_string())?;
        let purpose = isyncyou_agent::ConnectivityPurpose::parse(&request.purpose)
            .ok_or_else(|| "unknown connectivity purpose".to_string())?;
        let preflight = match CONNECTIVITY_PROBES.try_acquire() {
            None => isyncyou_agent::classify(None, None),
            Some(_permit) => {
                // Do not consume the single-use mobile handle until the request has passed
                // every local admission check and will actually run a probe.
                if session_token.is_some() && request.snapshot_id.is_none() {
                    return Err("mobile connectivity snapshot is required".into());
                }
                let consumed_snapshot = match request.snapshot_id.as_deref() {
                    Some(snapshot_id) => Some(consume_mobile_connectivity_snapshot(
                        snapshot_id,
                        session_token,
                        purpose,
                    )?),
                    None => None,
                };
                let snapshot = consumed_snapshot.as_ref().map(|value| value.snapshot);
                let forced_observation = consumed_snapshot
                    .as_ref()
                    .and_then(|value| value.forced_observation);
                #[cfg(any(
                    feature = "agent-oauth-providers",
                    feature = "agent-subscription-experimental"
                ))]
                {
                    let observation = forced_observation.unwrap_or_else(|| {
                        isyncyou_agent::http::HttpTransport::shared()
                            .ok()
                            .and_then(|http| {
                                http.probe(isyncyou_agent::target_for(provider, purpose))
                                    .ok()
                            })
                            .unwrap_or(isyncyou_agent::ProbeObservation::ConnectFailed)
                    });
                    isyncyou_agent::classify(snapshot, Some(observation))
                }
                #[cfg(not(any(
                    feature = "agent-oauth-providers",
                    feature = "agent-subscription-experimental"
                )))]
                {
                    let _ = (provider, purpose, forced_observation);
                    isyncyou_agent::classify(
                        snapshot,
                        Some(isyncyou_agent::ProbeObservation::ConnectFailed),
                    )
                }
            }
        };
        let (status, settings_hint) = match preflight.code {
            isyncyou_agent::ConnectivityPreflightCode::Ready => ("ready", "none"),
            isyncyou_agent::ConnectivityPreflightCode::NoValidatedNetwork => {
                ("action_required", "internet_panel")
            }
            isyncyou_agent::ConnectivityPreflightCode::RestrictedMeteredBackground => {
                ("action_required", "background_data")
            }
            isyncyou_agent::ConnectivityPreflightCode::ForegroundGuardUnavailable => {
                ("action_required", "app_details")
            }
            _ => ("unavailable", "none"),
        };
        Ok(isyncyou_webui::AgentConnectivityPreflightResponse {
            status: status.into(),
            code: preflight.code.wire().into(),
            retryable: preflight.retryable,
            settings_hint: settings_hint.into(),
        })
    }

    fn credential_refresh(&self, provider: &str) -> Result<String, String> {
        #[cfg(any(
            feature = "agent-oauth-providers",
            feature = "agent-subscription-experimental"
        ))]
        {
            self.refresh_product_credential(provider)
                .map(str::to_string)
        }
        #[cfg(not(any(
            feature = "agent-oauth-providers",
            feature = "agent-subscription-experimental"
        )))]
        {
            let _ = provider;
            Err("reconnect_required".into())
        }
    }

    fn start_turn(&self, account: &str, prompt: &str) -> Result<String, String> {
        // #639 T7: resolve + gate the provider BEFORE any turn-id / stream-slot / archive
        // resolution. A not-ready product turn returns the closed error here and creates no turn
        // state, no stream, no archive, and makes no provider call.
        let system = format!("{AGENT_SYSTEM_PROMPT}\n\nActive account: {account}.");
        let mut provider = self.resolve_turn_provider(&system)?;

        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let turn_id = format!("turn-{n}-{}", unix_now());
        let rx_events = self.hub.open(&turn_id, 256);
        let (tx_str, rx_str) = std::sync::mpsc::channel::<String>();
        // Forward hub StreamEvents -> JSON strings until the turn closes.
        std::thread::spawn(move || {
            while let Ok(ev) = rx_events.recv() {
                if tx_str.send(agent_event_json(&ev)).is_err() {
                    break;
                }
            }
        });
        let now_ms = unix_now_ms();
        {
            let mut streams = self.streams.lock().unwrap();
            Self::sweep_unopened_streams_locked(&mut streams, now_ms);
            streams.insert(
                turn_id.clone(),
                AgentStreamSlot {
                    rx: rx_str,
                    created_at_ms: now_ms,
                },
            );
        }
        // Run the turn on a background thread streaming events into the hub.
        let hub = self.hub.clone();
        let tid = turn_id.clone();
        let prompt = prompt.to_string();
        // Resolve the account's archive root now (reads config on this thread), so the
        // turn thread can build the real store-backed retrieval executor for it.
        let account_id = account.to_string();
        let archive_root = self.archive_root_for(&account_id);
        let pending = self.pending.clone();
        let last_usage = Arc::clone(&self.last_usage);
        std::thread::spawn(move || {
            let exec = make_executor(&account_id, archive_root);
            let mut history = vec![isyncyou_agent::Message::user(prompt)];
            let outcome = isyncyou_agent::run_turn(
                provider.as_mut(),
                exec.as_ref(),
                &mut history,
                &mut |ev| {
                    hub.emit(&tid, ev);
                },
            );
            if let Some(usage) = provider.last_usage() {
                *last_usage.lock().unwrap() = Some(usage);
            }
            match outcome {
                Ok(isyncyou_agent::TurnOutcome::Final { .. }) => {}
                Ok(isyncyou_agent::TurnOutcome::PendingConfirmation { action, .. }) => {
                    let action = *action;
                    let preview = match agent_ops::preview_for_pending_action(&action) {
                        Ok(preview) => preview,
                        Err(e) => {
                            let _ = hub.emit(
                                &tid,
                                isyncyou_agent::StreamEvent::Error(
                                    agent_ops::redact_agent_operation_text(&e),
                                ),
                            );
                            let _ = hub.emit(
                                &tid,
                                isyncyou_agent::StreamEvent::done(
                                    isyncyou_agent::DoneReason::Error,
                                ),
                            );
                            hub.close(&tid);
                            return;
                        }
                    };
                    let _risk = preview.risk.as_str();
                    match pending.register(
                        action,
                        preview.text,
                        unix_now_ms(),
                        AGENT_CONFIRM_TTL_MS,
                    ) {
                        Ok((pending_action, token)) => {
                            let _ = hub.emit(
                                &tid,
                                isyncyou_agent::StreamEvent::ConfirmationRequired {
                                    id: pending_action.id,
                                    action: Box::new(pending_action.action),
                                    preview: pending_action.preview,
                                    action_hash: pending_action.action_hash,
                                    risk: pending_action.risk,
                                    expires_at_ms: pending_action.expires_at_ms,
                                    token,
                                },
                            );
                            let _ = hub.emit(
                                &tid,
                                isyncyou_agent::StreamEvent::done(
                                    isyncyou_agent::DoneReason::PendingConfirmation,
                                ),
                            );
                        }
                        Err(e) => {
                            let _ = e;
                            let _ = hub.emit(
                                &tid,
                                isyncyou_agent::StreamEvent::Error(
                                    "confirmation_unavailable".into(),
                                ),
                            );
                            let _ = hub.emit(
                                &tid,
                                isyncyou_agent::StreamEvent::done(
                                    isyncyou_agent::DoneReason::Error,
                                ),
                            );
                        }
                    }
                }
                Err(e) => {
                    let _ = hub.emit(
                        &tid,
                        isyncyou_agent::StreamEvent::Error(agent_safe_turn_error(&e).into()),
                    );
                    let _ = hub.emit(
                        &tid,
                        isyncyou_agent::StreamEvent::done(isyncyou_agent::DoneReason::Error),
                    );
                }
            }
            hub.close(&tid);
        });
        Ok(turn_id)
    }

    fn pending_binding(
        &self,
        pending_id: &str,
        action_hash: &str,
    ) -> Result<isyncyou_webui::AgentPendingBinding, String> {
        let binding = self
            .pending
            .binding(pending_id, action_hash, unix_now_ms())
            .map_err(|e| format!("{e:?}"))?;
        Ok(isyncyou_webui::AgentPendingBinding {
            op: binding.op,
            account: binding.account,
            service: binding.service,
            item: binding.item,
        })
    }

    fn confirm(&self, pending_id: &str, token: &str, action_hash: &str) -> Result<String, String> {
        let action = self
            .pending
            .confirm(pending_id, token, action_hash, unix_now_ms())
            .map_err(|e| format!("{e:?}"))?;
        agent_ops::preview_for_pending_action(&action).map_err(|e| {
            format!(
                "invalid_confirmed_action: {}",
                agent_ops::redact_agent_operation_text(&e)
            )
        })?;
        let action_summary = agent_action_summary(&action);
        self.audit_sink
            .record_confirm(&action, "started", &action_summary)?;
        match self.confirmed_executor.execute_confirmed(&action) {
            Ok(result) => {
                let safe_summary = agent_ops::redact_agent_operation_text(&result.summary);
                self.audit_sink
                    .record_confirm(&action, "ok", &format!("{action_summary} ok"))?;
                serde_json::to_string(&serde_json::json!({
                    "status": "ok",
                    "op": action.op(),
                    "summary": safe_summary,
                }))
                .map_err(|e| e.to_string())
            }
            Err(e) => {
                let safe = agent_safe_executor_error(&e);
                self.audit_sink.record_confirm(
                    &action,
                    "error",
                    &format!("{action_summary} error={safe}"),
                )?;
                Err(format!("{} failed: {safe}", action.op()))
            }
        }
    }

    fn cancel(&self, turn_id: &str) {
        self.hub.cancel(turn_id);
    }

    fn open_stream(&self, turn_id: &str) -> Option<std::sync::mpsc::Receiver<String>> {
        self.streams.lock().unwrap().remove(turn_id).map(|s| s.rx)
    }

    /// Begin the Claude device OAuth login: PKCE + state, with the
    /// manual (copy-paste) redirect — claude.ai shows a code instead of redirecting to a
    /// loopback server. The app opens the returned URL in the system browser. Robust on
    /// mobile (no loopback host/port/IPv6 fragility).
    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn oauth_start_with_attempt(
        &self,
        provider: &str,
        redirect_uri: &str,
    ) -> Result<isyncyou_webui::AgentOAuthStartResponse, String> {
        match ProductProviderId::parse(provider) {
            Some(ProductProviderId::Codex) => self.codex_oauth_start(),
            Some(ProductProviderId::Claude) => {
                let mut attempts = self.oauth_attempts.lock().unwrap();
                reap_oauth_attempts(&mut attempts);
                if attempts
                    .values()
                    .any(|attempt| matches!(attempt, OAuthAttempt::Claude { .. }))
                {
                    return Err("Claude sign-in is already in progress".into());
                }
                let cfg = self.load_oauth_config()?;
                // Loopback-primary (matches the real claude client): use the client's loopback
                // redirect when supplied; fall back to the manual (copy-paste) redirect otherwise.
                let redirect = if redirect_uri.is_empty() {
                    cfg.manual_redirect_url.as_str()
                } else {
                    redirect_uri
                };
                let started = self
                    .oauth
                    .start(&cfg, redirect)
                    .map_err(|e| e.to_string())?;
                let attempt_id = mint_cap_token();
                attempts.insert(
                    attempt_id.clone(),
                    OAuthAttempt::Claude {
                        state: started.state,
                        expires_at: std::time::Instant::now() + OAUTH_ATTEMPT_TTL,
                    },
                );
                // #639 T8: record the official sign-in start into the attempt-keyed journal.
                record_onboarding_attempt_transition(
                    &self.oauth_dir,
                    &attempt_id,
                    ProductOnboardingState::OfficialSignInStarted,
                    None,
                );
                Ok(isyncyou_webui::AgentOAuthStartResponse {
                    authorize_url: started.authorize_url,
                    attempt_id,
                })
            }
            None => Err("unknown provider".into()),
        }
    }

    fn oauth_start(&self, provider: &str, redirect_uri: &str) -> Result<String, String> {
        self.oauth_start_with_attempt(provider, redirect_uri)
            .map(|result| result.authorize_url)
    }

    // Gated like the other OAuth methods: without the product/experimental features the trait's
    // default (login-not-enabled) applies, so the app-host compiles without agent-oauth-providers.
    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn oauth_cancel(&self, provider: &str, attempt_id: &str) -> Result<(), String> {
        let attempt = self.oauth_attempts.lock().unwrap().remove(attempt_id);
        match (provider, attempt) {
            ("claude", Some(OAuthAttempt::Claude { state, .. })) => {
                let _ = self.oauth.cancel(&state);
                // #639 T8: a cancelled in-flight attempt is a terminal, redacted transition.
                record_onboarding_attempt_transition(
                    &self.oauth_dir,
                    attempt_id,
                    ProductOnboardingState::ErrorRedacted,
                    Some("cancelled".to_string()),
                );
                Ok(())
            }
            ("codex", Some(OAuthAttempt::Codex { cancelled, .. })) => {
                cancelled.store(true, Ordering::Release);
                record_onboarding_attempt_transition(
                    &self.oauth_dir,
                    attempt_id,
                    ProductOnboardingState::ErrorRedacted,
                    Some("cancelled".to_string()),
                );
                Ok(())
            }
            (_, Some(attempt)) => {
                self.oauth_attempts
                    .lock()
                    .unwrap()
                    .insert(attempt_id.to_string(), attempt);
                Err("oauth attempt does not match provider".into())
            }
            _ => Err("oauth attempt is not active".into()),
        }
    }

    /// Complete the Claude manual login: the operator pastes the
    /// `code#state` shown by claude.ai. Look up the PKCE verifier by state, exchange, and
    /// persist the token.
    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn oauth_complete(&self, attempt_id: &str, pasted: &str) -> Result<String, String> {
        let (code, state_opt) = isyncyou_agent::oauth::parse_pasted_code(pasted);
        let state = state_opt.ok_or("the pasted code is missing its #state part")?;
        // #639 T9: bind the pasted code to the named attempt — the attempt must exist, be a Claude
        // attempt, and its server-side state must equal the embedded #state (no cross-attempt reuse).
        {
            let attempts = self.oauth_attempts.lock().unwrap();
            let bound = matches!(
                attempts.get(attempt_id),
                Some(OAuthAttempt::Claude { state: pending, .. }) if pending == &state
            );
            if !bound {
                return Err("the pasted code does not match this sign-in".into());
            }
        }
        let (verifier, redirect_uri) = self
            .oauth
            .take(&state)
            .ok_or("unknown or expired login — start the login again")?;
        self.oauth_attempts.lock().unwrap().retain(|id, attempt| {
            id != attempt_id
                && !matches!(attempt, OAuthAttempt::Claude { state: pending, .. } if pending == &state)
        });
        let cfg = self.load_oauth_config()?;
        let http = isyncyou_agent::http::HttpTransport::shared()
            .map_err(|_| "provider_connect_failed".to_string())?;
        let token =
            isyncyou_agent::oauth::exchange(&http, &cfg, &code, &verifier, &redirect_uri, &state)
                .map_err(|_| "oauth_exchange_failed".to_string())?;
        self.commit_claude_oauth_success(&token)?;
        Ok("connected".to_string())
    }

    /// The loopback callback path (kept for the auto flow); exchange
    /// the code with the stored verifier + state and persist the token, then show a page.
    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn oauth_callback(&self, code: &str, state: &str) -> Result<String, String> {
        let (verifier, redirect_uri) = self
            .oauth
            .take(state)
            .ok_or("unknown or expired login state")?;
        self.oauth_attempts.lock().unwrap().retain(|_, attempt| {
            !matches!(attempt, OAuthAttempt::Claude { state: pending, .. } if pending == state)
        });
        let cfg = self.load_oauth_config()?;
        let http = isyncyou_agent::http::HttpTransport::shared()
            .map_err(|_| "provider_connect_failed".to_string())?;
        let token =
            isyncyou_agent::oauth::exchange(&http, &cfg, code, &verifier, &redirect_uri, state)
                .map_err(|_| "oauth_exchange_failed".to_string())?;
        self.commit_claude_oauth_success(&token)?;
        Ok(Self::OAUTH_SUCCESS_HTML.to_string())
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn status_json(&self) -> String {
        let _ = self.oauth.reap_expired();
        if let Ok(mut attempts) = self.oauth_attempts.lock() {
            reap_oauth_attempts(&mut attempts);
        }
        let claude_state = self
            .product_credential_status("claude")
            .unwrap_or("reconnect_required");
        let codex_state = self
            .product_credential_status("codex")
            .unwrap_or("reconnect_required");
        let reconnect_required =
            claude_state == "reconnect_required" || codex_state == "reconnect_required";
        let (sel_provider, _) = self.agent_settings();
        // #639 T7: readiness is host-verified (valid Active bundle + a matching durable activation
        // + a passing static harness attestation), NOT credential presence, and is decoupled from
        // selection. A held product-runtime gate gives a snapshot consistent with a concurrent
        // turn build. An experimental local credential can never qualify.
        let (claude, codex) = {
            let _gate = self.product_runtime_gate.lock();
            (
                self.provider_ready(ProductProviderId::Claude),
                self.provider_ready(ProductProviderId::Codex),
            )
        };
        // The selected provider alone drives `connected` + `provider`; there is no fall-back to
        // the other provider, and an unparseable selection reads not-connected (fail-closed).
        let selected_id = ProductProviderId::parse(&sel_provider);
        let connected = match selected_id {
            Some(ProductProviderId::Claude) => claude,
            Some(ProductProviderId::Codex) => codex,
            None => false,
        };
        let provider = match selected_id {
            Some(id) => id.wire(),
            None => "",
        };
        let model = if provider.is_empty() {
            String::new()
        } else {
            self.model_for(provider)
        };
        let list = |models: &[(&str, &str)]| -> serde_json::Value {
            models
                .iter()
                .map(|(id, label)| serde_json::json!({ "id": id, "label": label }))
                .collect()
        };
        let mut status = serde_json::json!({
            "connected": connected,
            "enabled": true,
            "provider": provider,
            "selected_provider": sel_provider,
            "model": model,
            "claude": claude,
            "codex": codex,
            "credential_state": { "claude": claude_state, "codex": codex_state },
            "reconnect_required": reconnect_required,
            "models": { "claude": list(CLAUDE_MODELS), "codex": list(CODEX_MODELS) },
        });
        if let Some(usage) = self.last_usage.lock().unwrap().as_ref() {
            status["usage"] = usage.to_public_json();
        }
        // #639 T9: the per-provider onboarding projection drives the first-run wizard. It survives
        // journal TTL (a ready provider reports all steps complete from the durable activation).
        status["onboarding"] = self.onboarding_projection();
        status.to_string()
    }

    /// Persist the switcher's provider+model selection (validated against the offered lists).
    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn set_model(&self, provider: &str, model: &str) -> Result<(), String> {
        self.set_agent_settings(provider, model)
    }
}

/// Web-UI archive integrity verify (#528): re-hash every archived body and
/// persist per-item status. Local-only (reads on-disk bodies, writes the store),
/// so it needs no token/network and is always available.
pub struct DaemonVerify {
    cfg: Config,
}
impl isyncyou_webui::VerifyHandler for DaemonVerify {
    fn verify(&self, account: &str) -> Result<String, String> {
        isyncyou_engine::verify_account(&self.cfg, account).map(|r| r.summary())
    }
}

/// Web-UI mutable settings (#559): persist the cloud-poll interval to the config
/// file AND update the live value the sync loop reads, so a change takes effect
/// without a daemon restart.
pub struct DaemonSettings {
    config_path: PathBuf,
    live_interval: Arc<AtomicU64>,
}
impl isyncyou_webui::SettingsHandler for DaemonSettings {
    fn set_poll_interval_secs(&self, secs: u64) -> Result<(), String> {
        let secs = secs.clamp(1, 3600);
        // apply to the running loop immediately, then persist for the next start
        self.live_interval.store(secs, Ordering::Relaxed);
        let mut cfg = Config::load(&self.config_path)?;
        cfg.sync.poll_interval_secs = secs;
        cfg.save(&self.config_path)
    }
}

/// Web-UI OneDrive per-folder mode (#651): reads the account's mode policy **fresh** from
/// the config file (so a prior POST is reflected — the Router holds `config` by value) and
/// persists a folder set/clear back to it (`load → mutate → validate → save`, like
/// `DaemonSettings`).
pub struct DaemonOneDriveMode {
    config_path: PathBuf,
}
impl isyncyou_webui::OneDriveModeHandler for DaemonOneDriveMode {
    fn modes(&self, account: &str) -> Result<OneDriveModes, String> {
        Ok(Config::load(&self.config_path)?
            .onedrive_modes
            .get(account)
            .cloned()
            .unwrap_or_default())
    }
    fn set_folder(
        &self,
        account: &str,
        folder_id: &str,
        mode: Option<OneDriveMode>,
    ) -> Result<(), String> {
        let mut cfg = Config::load(&self.config_path)?;
        let modes = cfg.onedrive_modes.entry(account.to_string()).or_default();
        match mode {
            Some(m) => {
                modes.folder_modes.insert(folder_id.to_string(), m);
            }
            None => {
                modes.folder_modes.remove(folder_id);
            }
        }
        cfg.validate().map_err(|errs| errs.join("; "))?;
        cfg.save(&self.config_path)
    }
}

/// Web-UI live-mail write (#561): each verb resolves the full write token
/// (`Mail.ReadWrite` + `Mail.Send`) from the cached `login --write` and pushes the
/// change to Microsoft 365 via the engine `MailWriter`. Trait calls are fully
/// qualified so they hit the engine layer, never the inherent `GraphClient`
/// methods that share their names. The UI for these lands in #563.
pub struct DaemonMailWrite {
    cfg: Config,
}
impl isyncyou_webui::MailWriteHandler for DaemonMailWrite {
    #[allow(clippy::too_many_arguments)]
    fn send(
        &self,
        account: &str,
        subject: &str,
        body_html: &str,
        to: &[String],
        cc: &[String],
        bcc: &[String],
        importance: Option<&str>,
        request_read_receipt: bool,
    ) -> Result<(), String> {
        let w = isyncyou_engine::mail_writer(&self.cfg, account)?;
        isyncyou_engine::MailWriter::send_new(
            &w,
            subject,
            body_html,
            to,
            cc,
            bcc,
            importance,
            request_read_receipt,
        )
    }
    fn reply(
        &self,
        account: &str,
        message_id: &str,
        comment: &str,
        all: bool,
    ) -> Result<(), String> {
        let w = isyncyou_engine::mail_writer(&self.cfg, account)?;
        isyncyou_engine::MailWriter::reply(&w, message_id, comment, all)
    }
    fn forward(
        &self,
        account: &str,
        message_id: &str,
        comment: &str,
        to: &[String],
    ) -> Result<(), String> {
        let w = isyncyou_engine::mail_writer(&self.cfg, account)?;
        isyncyou_engine::MailWriter::forward(&w, message_id, comment, to)
    }
    fn reply_html(
        &self,
        account: &str,
        message_id: &str,
        body_html: &str,
        all: bool,
    ) -> Result<(), String> {
        let w = isyncyou_engine::mail_writer(&self.cfg, account)?;
        isyncyou_engine::MailWriter::reply_html(&w, message_id, body_html, all)
    }
    fn forward_html(
        &self,
        account: &str,
        message_id: &str,
        body_html: &str,
        to: &[String],
    ) -> Result<(), String> {
        let w = isyncyou_engine::mail_writer(&self.cfg, account)?;
        isyncyou_engine::MailWriter::forward_html(&w, message_id, body_html, to)
    }
    fn move_to(
        &self,
        account: &str,
        message_id: &str,
        destination_id: &str,
    ) -> Result<String, String> {
        let w = isyncyou_engine::mail_writer(&self.cfg, account)?;
        isyncyou_engine::MailWriter::move_to(&w, message_id, destination_id)
    }
    fn set_read(&self, account: &str, message_id: &str, is_read: bool) -> Result<(), String> {
        let w = isyncyou_engine::mail_writer(&self.cfg, account)?;
        isyncyou_engine::MailWriter::set_read(&w, message_id, is_read)
    }
    fn set_flag(
        &self,
        account: &str,
        message_id: &str,
        flag_status: &str,
        due: Option<&str>,
        tz: &str,
    ) -> Result<(), String> {
        let w = isyncyou_engine::mail_writer(&self.cfg, account)?;
        isyncyou_engine::MailWriter::set_flag(&w, message_id, flag_status, due, tz)
    }
    fn set_categories(
        &self,
        account: &str,
        message_id: &str,
        categories: &[String],
    ) -> Result<(), String> {
        let w = isyncyou_engine::mail_writer(&self.cfg, account)?;
        isyncyou_engine::MailWriter::set_categories(&w, message_id, categories)
    }
    fn create_draft(
        &self,
        account: &str,
        subject: &str,
        body_html: &str,
        to: &[String],
    ) -> Result<String, String> {
        let w = isyncyou_engine::mail_writer(&self.cfg, account)?;
        isyncyou_engine::MailWriter::create_draft(&w, subject, body_html, to)
    }
    fn send_draft(&self, account: &str, message_id: &str) -> Result<(), String> {
        let w = isyncyou_engine::mail_writer(&self.cfg, account)?;
        isyncyou_engine::MailWriter::send_draft(&w, message_id)
    }
}

/// Web-UI live-calendar write (#565 B7): resolves the restore-scope write token
/// and performs create/update/delete/respond. Fully qualified so the inherent
/// GraphClient methods that share names aren't shadowed.
pub struct DaemonCalendarWrite {
    cfg: Config,
}
impl isyncyou_webui::CalendarWriteHandler for DaemonCalendarWrite {
    fn create(&self, account: &str, event: &serde_json::Value) -> Result<String, String> {
        let w = isyncyou_engine::calendar_writer(&self.cfg, account)?;
        isyncyou_engine::CalendarWriter::create_event(&w, event)
    }
    fn update(
        &self,
        account: &str,
        event_id: &str,
        event: &serde_json::Value,
    ) -> Result<(), String> {
        let w = isyncyou_engine::calendar_writer(&self.cfg, account)?;
        isyncyou_engine::CalendarWriter::update_event(&w, event_id, event)
    }
    fn delete(&self, account: &str, event_id: &str) -> Result<(), String> {
        let w = isyncyou_engine::calendar_writer(&self.cfg, account)?;
        isyncyou_engine::CalendarWriter::delete_event(&w, event_id)
    }
    fn respond(
        &self,
        account: &str,
        event_id: &str,
        response: &str,
        comment: &str,
    ) -> Result<(), String> {
        let w = isyncyou_engine::calendar_writer(&self.cfg, account)?;
        isyncyou_engine::CalendarWriter::respond(&w, event_id, response, comment)
    }
}

/// Web-UI live-contact write (#566 A5): resolves the restore-scope write token
/// and performs create/update/delete. Fully qualified so the inherent GraphClient
/// methods that share names aren't shadowed.
pub struct DaemonContactWrite {
    cfg: Config,
}
impl isyncyou_webui::ContactWriteHandler for DaemonContactWrite {
    fn create(&self, account: &str, contact: &serde_json::Value) -> Result<String, String> {
        let w = isyncyou_engine::contact_writer(&self.cfg, account)?;
        isyncyou_engine::ContactWriter::create_contact(&w, contact)
    }
    fn update(
        &self,
        account: &str,
        contact_id: &str,
        contact: &serde_json::Value,
    ) -> Result<(), String> {
        let w = isyncyou_engine::contact_writer(&self.cfg, account)?;
        isyncyou_engine::ContactWriter::update_contact(&w, contact_id, contact)
    }
    fn delete(&self, account: &str, contact_id: &str) -> Result<(), String> {
        let w = isyncyou_engine::contact_writer(&self.cfg, account)?;
        isyncyou_engine::ContactWriter::delete_contact(&w, contact_id)
    }
}

/// Web-UI live-ToDo write (#567 B6): resolves the restore-scope write token and
/// performs the task/checklist/list verbs. Fully qualified so the inherent
/// GraphClient methods that share names aren't shadowed.
pub struct DaemonTaskWrite {
    cfg: Config,
}
impl isyncyou_webui::TaskWriteHandler for DaemonTaskWrite {
    fn create(
        &self,
        account: &str,
        list_id: &str,
        task: &serde_json::Value,
    ) -> Result<String, String> {
        let w = isyncyou_engine::task_writer(&self.cfg, account)?;
        isyncyou_engine::TaskWriter::create(&w, list_id, task)
    }
    fn update(
        &self,
        account: &str,
        list_id: &str,
        task_id: &str,
        task: &serde_json::Value,
    ) -> Result<(), String> {
        let w = isyncyou_engine::task_writer(&self.cfg, account)?;
        isyncyou_engine::TaskWriter::update(&w, list_id, task_id, task)
    }
    fn complete(&self, account: &str, list_id: &str, task_id: &str) -> Result<(), String> {
        let w = isyncyou_engine::task_writer(&self.cfg, account)?;
        isyncyou_engine::TaskWriter::complete(&w, list_id, task_id)
    }
    fn delete(&self, account: &str, list_id: &str, task_id: &str) -> Result<(), String> {
        let w = isyncyou_engine::task_writer(&self.cfg, account)?;
        isyncyou_engine::TaskWriter::delete(&w, list_id, task_id)
    }
    fn checklist_add(
        &self,
        account: &str,
        list_id: &str,
        task_id: &str,
        title: &str,
    ) -> Result<String, String> {
        let w = isyncyou_engine::task_writer(&self.cfg, account)?;
        isyncyou_engine::TaskWriter::checklist_add(&w, list_id, task_id, title)
    }
    fn checklist_toggle(
        &self,
        account: &str,
        list_id: &str,
        task_id: &str,
        item_id: &str,
        checked: bool,
    ) -> Result<(), String> {
        let w = isyncyou_engine::task_writer(&self.cfg, account)?;
        isyncyou_engine::TaskWriter::checklist_toggle(&w, list_id, task_id, item_id, checked)
    }
    fn checklist_delete(
        &self,
        account: &str,
        list_id: &str,
        task_id: &str,
        item_id: &str,
    ) -> Result<(), String> {
        let w = isyncyou_engine::task_writer(&self.cfg, account)?;
        isyncyou_engine::TaskWriter::checklist_delete(&w, list_id, task_id, item_id)
    }
    fn list_create(&self, account: &str, name: &str) -> Result<String, String> {
        let w = isyncyou_engine::task_writer(&self.cfg, account)?;
        isyncyou_engine::TaskWriter::list_create(&w, name)
    }
    fn list_delete(&self, account: &str, list_id: &str) -> Result<(), String> {
        let w = isyncyou_engine::task_writer(&self.cfg, account)?;
        isyncyou_engine::TaskWriter::list_delete(&w, list_id)
    }
}

/// Web-UI live-OneNote write (#568): resolves the restore-scope write token and
/// performs create-in-section / delete / append. Fully qualified so the inherent
/// GraphClient methods that share names aren't shadowed.
pub struct DaemonOneNoteWrite {
    cfg: Config,
}
impl isyncyou_webui::OneNoteWriteHandler for DaemonOneNoteWrite {
    fn create(&self, account: &str, section_id: &str, html: &[u8]) -> Result<String, String> {
        let w = isyncyou_engine::page_writer(&self.cfg, account)?;
        isyncyou_engine::PageWriter::create(&w, section_id, html)
    }
    fn delete(&self, account: &str, page_id: &str) -> Result<(), String> {
        let w = isyncyou_engine::page_writer(&self.cfg, account)?;
        isyncyou_engine::PageWriter::delete(&w, page_id)
    }
    fn append(&self, account: &str, page_id: &str, text: &str) -> Result<(), String> {
        let w = isyncyou_engine::page_writer(&self.cfg, account)?;
        isyncyou_engine::PageWriter::append(&w, page_id, text)
    }
}

/// Per-login progress, shared between the HTTP poll handler and the background
/// device-code thread (#68).
#[derive(Default)]
pub struct LoginState {
    device: Option<isyncyou_graph::auth::flow::DeviceCode>,
    done: bool,
    error: Option<String>,
}

static LOGIN_SEQ: AtomicU64 = AtomicU64::new(1);

/// Account-auth handler (#68): a device-code sign-in runs to completion in a
/// background thread (so the HTTP handler returns the code at once and the UI
/// polls), writing the account's write-token cache on success. Sign-out clears the
/// cached tokens. Re-authenticates an account already present in the config.
pub struct DaemonAccountAuth {
    cfg: Config,
    logins: Mutex<std::collections::HashMap<u64, Arc<Mutex<LoginState>>>>,
}
impl isyncyou_webui::AccountAuthHandler for DaemonAccountAuth {
    fn start_login(&self, account: &str) -> Result<serde_json::Value, String> {
        let cache = isyncyou_engine::auth::write_token_cache_path(&self.cfg, account)
            .ok_or_else(|| format!("no account '{account}' in config"))?;
        let id = LOGIN_SEQ.fetch_add(1, Ordering::SeqCst);
        let state = Arc::new(Mutex::new(LoginState::default()));
        self.logins.lock().unwrap().insert(id, state.clone());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let st = state.clone();
        std::thread::spawn(move || {
            let present = |dc: &isyncyou_graph::auth::flow::DeviceCode| {
                st.lock().unwrap().device = Some(dc.clone());
            };
            match isyncyou_graph::auth::flow::device_code_login(
                isyncyou_engine::auth::WRITE_CLIENT,
                isyncyou_engine::auth::RESTORE_SCOPES,
                now,
                present,
            ) {
                Ok(tokens) => match tokens.save(&cache) {
                    Ok(()) => st.lock().unwrap().done = true,
                    Err(e) => st.lock().unwrap().error = Some(format!("save token: {e}")),
                },
                Err(e) => st.lock().unwrap().error = Some(e),
            }
        });
        // Wait briefly for the device code — start_device_code is the first network
        // call inside device_code_login, so it lands within a second or two.
        for _ in 0..100 {
            {
                let s = state.lock().unwrap();
                if let Some(dc) = &s.device {
                    return Ok(serde_json::json!({
                        "login_id": id.to_string(),
                        "user_code": dc.user_code,
                        "verification_uri": dc.verification_uri,
                        "message": dc.message,
                    }));
                }
                if let Some(e) = &s.error {
                    return Err(e.clone());
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err("device-code did not start in time".into())
    }

    fn poll_login(&self, login_id: &str) -> serde_json::Value {
        let Ok(id) = login_id.parse::<u64>() else {
            return serde_json::json!({ "state": "error", "error": "bad login id" });
        };
        let state = self.logins.lock().unwrap().get(&id).cloned();
        let Some(state) = state else {
            return serde_json::json!({ "state": "error", "error": "unknown login id" });
        };
        let s = state.lock().unwrap();
        if let Some(e) = &s.error {
            serde_json::json!({ "state": "error", "error": e })
        } else if s.done {
            serde_json::json!({ "state": "done" })
        } else {
            serde_json::json!({ "state": "pending" })
        }
    }

    fn sign_out(&self, account: &str) -> Result<serde_json::Value, String> {
        let n = isyncyou_engine::auth::sign_out(&self.cfg, account)?;
        Ok(serde_json::json!({ "removed": n, "message": format!("Signed out of {account}") }))
    }
}

/// Push notifications (#576): stores registered device FCM tokens and sends FCM v1
/// messages via a Google service-account. The PushProvider abstraction (ADR-006) is
/// FCM here; a self-hosted ntfy/UnifiedPush provider is the documented alternative.
/// The service-account path comes from `ISYNCYOU_FCM_SA` (push disabled if unset);
/// tokens persist as JSON next to the first account's archive.
#[derive(Clone)]
pub struct DaemonPush {
    tokens_path: PathBuf,
    sa_path: Option<PathBuf>,
}
impl DaemonPush {
    pub fn new(cfg: &Config) -> Self {
        let tokens_path = cfg
            .accounts
            .first()
            .map(|a| a.archive_root.join(".isyncyou-push-tokens.json"))
            .unwrap_or_else(|| PathBuf::from(".isyncyou-push-tokens.json"));
        let sa_path = std::env::var_os("ISYNCYOU_FCM_SA").map(PathBuf::from);
        DaemonPush {
            tokens_path,
            sa_path,
        }
    }
    fn load_tokens(&self) -> Vec<String> {
        std::fs::read_to_string(&self.tokens_path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .unwrap_or_default()
    }
    /// Send one notification to every registered device. Returns how many succeeded.
    /// Best-effort: a missing service-account or a dead token never fails a caller.
    pub fn notify(&self, title: &str, body: &str) -> usize {
        let Some(sa_path) = &self.sa_path else {
            return 0;
        };
        let Ok(sa) = std::fs::read_to_string(sa_path)
            .map_err(|e| e.to_string())
            .and_then(|j| isyncyou_graph::push::ServiceAccount::from_json(&j))
        else {
            eprintln!("isyncyoud: push disabled — service-account unreadable");
            return 0;
        };
        let now = unix_now().parse::<u64>().unwrap_or(0);
        let mut sent = 0;
        for t in self.load_tokens() {
            match isyncyou_graph::push::fcm_send(&sa, &t, title, body, now) {
                Ok(_) => sent += 1,
                Err(e) => eprintln!("isyncyoud: push to a device failed: {e}"),
            }
        }
        sent
    }
}
impl isyncyou_webui::PushHandler for DaemonPush {
    fn register(&self, token: &str) -> Result<(), String> {
        let mut toks = self.load_tokens();
        if !toks.iter().any(|t| t == token) {
            toks.push(token.to_string());
            std::fs::write(
                &self.tokens_path,
                serde_json::to_vec(&toks).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    fn send_test(&self) -> Result<serde_json::Value, String> {
        let n = self.notify("iSyncYou", "Test notification");
        Ok(serde_json::json!({ "sent": n, "registered": self.load_tokens().len() }))
    }
}

/// Web-UI outbound sharing (#494/#722): route OneDrive link/invite mutations through
/// the crash-safe cloud-write ledger. Only OneDrive drive items are shareable.
pub struct DaemonShare {
    cfg: Config,
}
impl isyncyou_webui::ShareHandler for DaemonShare {
    fn share(
        &self,
        account: &str,
        service: &str,
        id: &str,
        link_type: &str,
        scope: &str,
    ) -> Result<String, String> {
        if service != "onedrive" {
            return Err(format!(
                "sharing is only supported for OneDrive items, not '{service}'"
            ));
        }
        isyncyou_engine::share_link_via_ledger(&self.cfg, account, id, link_type, scope)
    }
    fn invite(
        &self,
        account: &str,
        service: &str,
        id: &str,
        emails: &[String],
        role: &str,
    ) -> Result<String, String> {
        if service != "onedrive" {
            return Err(format!(
                "sharing is only supported for OneDrive items, not '{service}'"
            ));
        }
        let role = if role == "write" { "write" } else { "read" };
        isyncyou_engine::invite_via_ledger(&self.cfg, account, id, emails, role)
    }
}

/// Live OneDrive info for the web UI (#564): the drive quota (and, in #564 A4,
/// per-item permissions). Resolves the cached sync token (covers the `/me/drive`
/// read) and calls Graph. Read-only — no capability token.
pub struct DaemonOneDriveInfo {
    cfg: Config,
}
impl isyncyou_webui::OneDriveInfoHandler for DaemonOneDriveInfo {
    fn drive_quota(&self, account: &str) -> Result<serde_json::Value, String> {
        let token = isyncyou_engine::auth::resolve_cached_sync_token(&self.cfg, account)?;
        isyncyou_graph::GraphClient::new(token)
            .drive_quota()
            .map_err(|e| e.to_string())
    }
    fn permissions(&self, account: &str, id: &str) -> Result<serde_json::Value, String> {
        let token = isyncyou_engine::auth::resolve_cached_sync_token(&self.cfg, account)?;
        let perms = isyncyou_graph::GraphClient::new(token)
            .list_permissions(id)
            .map_err(|e| e.to_string())?;
        Ok(serde_json::Value::Array(
            perms
                .into_iter()
                .map(|(pid, roles, link, grantee)| {
                    serde_json::json!({ "id": pid, "roles": roles, "link": link, "grantee": grantee })
                })
                .collect(),
        ))
    }
}

/// Live OneDrive folder listing for the web UI (#648, Mode 1 online): a folder's
/// children read straight from Graph (fully paged, no store write) via the engine's
/// `OneDriveLister`. Resolves the read-capable (mobile-friendly) token. Read-only —
/// no capability token.
pub struct DaemonOneDriveList {
    cfg: Config,
}
impl isyncyou_webui::OneDriveListHandler for DaemonOneDriveList {
    fn children(&self, account: &str, folder: &str) -> Result<Vec<serde_json::Value>, String> {
        let client = isyncyou_engine::onedrive_lister(&self.cfg, account)?;
        isyncyou_engine::OneDriveLister::list_children(&client, folder)
    }
}

/// Live OneDrive cloud-write handler (#654): create / rename / move / delete over the
/// crash-safe operation ledger. Delegates to the engine ledger drivers (each opens the
/// account store, resolves the write token, and records the idempotent intent BEFORE the
/// Graph call, so a crash mid-op is recovered without a double effect). On mobile `delete`
/// is additionally biometric-gated by the router; the cap token is the CSRF gate.
pub struct DaemonOneDriveWrite {
    cfg: Config,
}
impl DaemonOneDriveWrite {
    pub fn new(cfg: Config) -> Self {
        Self { cfg }
    }
}
impl isyncyou_webui::OneDriveWriteHandler for DaemonOneDriveWrite {
    fn create_folder(&self, account: &str, parent_id: &str, name: &str) -> Result<String, String> {
        isyncyou_engine::create_folder_via_ledger(&self.cfg, account, parent_id, name)
    }
    fn rename(&self, account: &str, id: &str, new_name: &str) -> Result<(), String> {
        isyncyou_engine::rename_via_ledger(&self.cfg, account, id, new_name)
    }
    fn move_item(
        &self,
        account: &str,
        id: &str,
        new_parent_id: Option<&str>,
        new_name: &str,
    ) -> Result<(), String> {
        isyncyou_engine::move_via_ledger(&self.cfg, account, id, new_parent_id, new_name)
    }
    fn delete(&self, account: &str, id: &str) -> Result<(), String> {
        isyncyou_engine::delete_via_ledger(&self.cfg, account, id)
    }
    // #657: an in-app upload/replace carries its bytes in the request body, but the crash-safe
    // cloud-write ledger reads the body from a local path (like the offline writeback). Stage the
    // bytes under the account-private cache root, through the body-envelope writer, then route
    // through #655's ledger so an in-app write gets the same intent-first crash safety without
    // leaving Android plaintext in a process/global temp directory.
    fn upload(
        &self,
        account: &str,
        parent_id: &str,
        name: &str,
        bytes: &[u8],
    ) -> Result<String, String> {
        let tmp = TempBody::write(&self.cfg, account, bytes)?;
        isyncyou_engine::upload_via_ledger(&self.cfg, account, parent_id, name, tmp.path())
    }
    fn replace(&self, account: &str, id: &str, etag: &str, bytes: &[u8]) -> Result<(), String> {
        let tmp = TempBody::write(&self.cfg, account, bytes)?;
        // Replace is etag-guarded: a 412 is a terminal keep-both conflict, never a blind clobber.
        match isyncyou_engine::replace_via_ledger(&self.cfg, account, id, etag, tmp.path())? {
            isyncyou_engine::WriteOutcome::Applied(_) => Ok(()),
            isyncyou_engine::WriteOutcome::Conflict => Err(
                "replace conflict: the file changed in OneDrive since it was listed — kept both, not overwritten"
                    .into(),
            ),
        }
    }
}

/// A short-lived account-private staging file holding an in-app upload/replace body (#657).
/// The cloud-write ledger reads the body from a local path (fresh, and on crash recovery), so a
/// WebUI request's in-memory bytes are staged here and removed on drop — even on an error path.
/// On Android, the active body key makes [`isyncyou_core::envelope::write_body_atomic`] persist a
/// sealed envelope instead of plaintext; desktop keeps its no-key plaintext compatibility.
struct TempBody(PathBuf);
impl TempBody {
    const DIR: &'static str = "upload-staging";
    const PREFIX: &'static str = "isyncyou-upload-";
    const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

    fn write(cfg: &Config, account: &str, bytes: &[u8]) -> Result<Self, String> {
        let acc = cfg
            .accounts
            .iter()
            .find(|a| a.id == account)
            .ok_or_else(|| format!("no account '{account}'"))?;
        Self::write_in_dir(&acc.effective_cache_root().join(Self::DIR), bytes)
    }

    fn write_in_dir(dir: &std::path::Path, bytes: &[u8]) -> Result<Self, String> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        std::fs::create_dir_all(dir).map_err(|e| format!("create upload staging: {e}"))?;
        Self::cleanup_stale(dir);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("{}{}-{n}.bin", Self::PREFIX, std::process::id()));
        isyncyou_core::envelope::write_body_atomic(&path, bytes)
            .map_err(|e| format!("stage upload body: {e}"))?;
        Ok(Self(path))
    }

    fn cleanup_stale(dir: &std::path::Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !name.starts_with(Self::PREFIX) {
                continue;
            }
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|age| age >= Self::STALE_AFTER);
            if stale {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    fn path(&self) -> &std::path::Path {
        self.0.as_path()
    }
}
impl Drop for TempBody {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn onedrive_ancestry<'a>(by_id: &HashMap<&'a str, &'a Item>, it: &'a Item) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut cur = it;
    for _ in 0..256 {
        let Some(parent) = cur.parent_remote_id.as_deref() else {
            break;
        };
        out.push(parent);
        match by_id.get(parent) {
            Some(next) => cur = next,
            None => break,
        }
    }
    out
}

fn onedrive_effective_mode(
    cfg: &Config,
    account: &str,
    by_id: &HashMap<&str, &Item>,
    it: &Item,
) -> OneDriveMode {
    let modes = cfg.onedrive_modes.get(account).cloned().unwrap_or_default();
    let ancestry = onedrive_ancestry(by_id, it);
    modes.effective_mode(&it.remote_id, &ancestry)
}

const OFFLINE_BULK_FILE_THRESHOLD: usize = 2;
const OFFLINE_LARGE_BYTE_THRESHOLD: u64 = 256 * 1024 * 1024;

fn onedrive_ancestry_with_self<'a>(
    by_id: &HashMap<&'a str, &'a Item>,
    it: &'a Item,
) -> Vec<&'a str> {
    let mut out = Vec::with_capacity(1 + 8);
    out.push(it.remote_id.as_str());
    out.extend(onedrive_ancestry(by_id, it));
    out
}

fn onedrive_offline_scope_owner(
    modes: Option<&OneDriveModes>,
    ancestry: &[&str],
) -> Option<String> {
    let scopes = isyncyou_connectors::scopes_from_modes(modes);
    let offline_ids: BTreeSet<&str> = scopes
        .iter()
        .filter(|s| s.mode == isyncyou_connectors::Mode::Offline)
        .map(|s| s.folder_id.as_str())
        .collect();
    isyncyou_connectors::owning_scope(ancestry, &offline_ids).map(str::to_string)
}

fn classify_onedrive_move_risk_from_items(
    modes: Option<&OneDriveModes>,
    by_id: &HashMap<&str, &Item>,
    source_id: &str,
    destination_parent_id: &str,
) -> OneDriveMoveRisk {
    let Some(source) = by_id.get(source_id) else {
        return OneDriveMoveRisk::Unknown {
            reason: "missing_source".into(),
        };
    };
    let source_ancestry = onedrive_ancestry_with_self(by_id, source);
    let Some(source_scope) = onedrive_offline_scope_owner(modes, &source_ancestry) else {
        return OneDriveMoveRisk::Low;
    };

    let destination_scope = if destination_parent_id.is_empty() {
        None
    } else {
        let Some(destination) = by_id.get(destination_parent_id) else {
            return OneDriveMoveRisk::Unknown {
                reason: "missing_destination".into(),
            };
        };
        let destination_ancestry = onedrive_ancestry_with_self(by_id, destination);
        onedrive_offline_scope_owner(modes, &destination_ancestry)
    };

    if destination_scope.as_deref() == Some(source_scope.as_str()) {
        OneDriveMoveRisk::Low
    } else {
        OneDriveMoveRisk::MoveOutOfProtected {
            source_scope,
            destination_scope,
        }
    }
}

fn offline_mode_risk(
    requires_confirmation: bool,
    file_count: usize,
    known_bytes: u64,
    unknown_size_files: usize,
    reason: &str,
) -> OfflineModeRisk {
    OfflineModeRisk {
        requires_confirmation,
        file_count,
        known_bytes,
        unknown_size_files,
        reason: reason.into(),
    }
}

fn estimate_onedrive_offline_mode_risk_from_items(
    by_id: &HashMap<&str, &Item>,
    folder_id: &str,
) -> OfflineModeRisk {
    let Some(folder) = by_id.get(folder_id) else {
        return offline_mode_risk(true, 0, 0, 0, "unknown_folder");
    };
    if folder.item_type != "folder" {
        return offline_mode_risk(true, 0, 0, 0, "not_folder");
    }

    let mut file_count = 0usize;
    let mut known_bytes = 0u64;
    let mut unknown_size_files = 0usize;
    for item in by_id.values().copied() {
        if item.item_type != "file" {
            continue;
        }
        let ancestry = onedrive_ancestry(by_id, item);
        if !ancestry.contains(&folder_id) {
            continue;
        }
        file_count += 1;
        match item.size {
            Some(size) if size >= 0 => known_bytes = known_bytes.saturating_add(size as u64),
            _ => unknown_size_files += 1,
        }
    }

    if file_count >= OFFLINE_BULK_FILE_THRESHOLD {
        offline_mode_risk(
            true,
            file_count,
            known_bytes,
            unknown_size_files,
            "bulk_files",
        )
    } else if known_bytes >= OFFLINE_LARGE_BYTE_THRESHOLD {
        offline_mode_risk(
            true,
            file_count,
            known_bytes,
            unknown_size_files,
            "large_bytes",
        )
    } else if unknown_size_files > 0 && file_count > 0 {
        offline_mode_risk(
            true,
            file_count,
            known_bytes,
            unknown_size_files,
            "unknown_size",
        )
    } else {
        offline_mode_risk(false, file_count, known_bytes, unknown_size_files, "small")
    }
}

fn onedrive_body_bytes(
    acc: &isyncyou_core::AccountConfig,
    by_id: &HashMap<&str, &Item>,
    it: &Item,
) -> Result<Option<Vec<u8>>, String> {
    if it.body_state.as_deref() != Some("available") {
        return Ok(None);
    }
    let Some(rel) = isyncyou_connectors::local_rel_path(by_id, it) else {
        return Ok(None);
    };
    let root = if it.body_location.as_deref() == Some("cache") {
        acc.effective_cache_root()
    } else {
        acc.sync_root.clone()
    };
    let path = root.join(rel);
    let body = if isyncyou_core::envelope::body_envelope_required_for_process() {
        isyncyou_core::envelope::read_sealed_body_required(&path)
    } else {
        isyncyou_core::envelope::read_body(&path)
    };
    match body {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read cached OneDrive body: {e}")),
    }
}

/// Live on-demand OneDrive content fetch for the web UI (#649, Mode 1 online), plus
/// Mode-2 lazy body caching (#660): local bodies win first, sync-mode misses download into
/// `cache_root`, and online-mode misses stay live/no-store.
pub struct DaemonOneDriveOpen {
    config_path: PathBuf,
    progress: isyncyou_connectors::SharedProgress,
}
impl DaemonOneDriveOpen {
    fn cfg(&self) -> Result<Config, String> {
        Config::load(&self.config_path).map_err(|e| format!("load config: {e}"))
    }
}
impl isyncyou_webui::OneDriveOpenHandler for DaemonOneDriveOpen {
    fn download(&self, account: &str, id: &str) -> Result<Vec<u8>, String> {
        let cfg = self.cfg()?;
        let acc = cfg
            .accounts
            .iter()
            .find(|a| a.id == account)
            .ok_or_else(|| format!("no account '{account}'"))?;
        let store = isyncyou_store::Store::open(acc.archive_root.join(".isyncyou-store.db")).ok();
        if let Some(store) = store.as_ref() {
            let items = store
                .items_by_service(account, "onedrive")
                .map_err(|e| format!("query OneDrive store: {e}"))?;
            let by_id: HashMap<&str, &Item> =
                items.iter().map(|it| (it.remote_id.as_str(), it)).collect();
            if let Some(it) = by_id.get(id) {
                if let Some(bytes) = onedrive_body_bytes(acc, &by_id, it)? {
                    return Ok(bytes);
                }
                if it.item_type == "file"
                    && onedrive_effective_mode(&cfg, account, &by_id, it) == OneDriveMode::Sync
                {
                    let Some(rel) = isyncyou_connectors::local_rel_path(&by_id, it) else {
                        return Err("sync-mode open: no local path".into());
                    };
                    let full = acc.effective_cache_root().join(&rel);
                    if let Some(parent) = full.parent() {
                        std::fs::create_dir_all(parent)
                            .map_err(|e| format!("create cache parent: {e}"))?;
                    }
                    let client = isyncyou_engine::onedrive_lister(&cfg, account)?;
                    store
                        .set_content_state(
                            account,
                            "onedrive",
                            id,
                            Some("cached"),
                            Some("cache"),
                            Some("downloading"),
                            None,
                        )
                        .map_err(|e| format!("mark sync download: {e}"))?;
                    let name = rel.file_name().and_then(|s| s.to_str()).unwrap_or("file");
                    let total = it.size.unwrap_or(0).max(0) as u64;
                    self.progress.begin(id, name, total);
                    let downloaded = client
                        .download_content_with_progress(id, &mut |done| {
                            self.progress.advance(id, done);
                        })
                        .map_err(|e| e.to_string());
                    match downloaded {
                        Ok(bytes) => {
                            let result = (|| {
                                isyncyou_core::envelope::write_body_atomic(&full, &bytes)
                                    .map_err(|e| format!("write cache body: {e}"))?;
                                store
                                    .set_sync_state(account, "onedrive", id, "clean")
                                    .map_err(|e| format!("mark sync clean: {e}"))?;
                                store
                                    .set_content_state(
                                        account,
                                        "onedrive",
                                        id,
                                        Some("cached"),
                                        Some("cache"),
                                        Some("available"),
                                        Some(&unix_now()),
                                    )
                                    .map_err(|e| format!("mark cache available: {e}"))?;
                                Ok::<(), String>(())
                            })();
                            self.progress.finish(id);
                            result?;
                            return Ok(bytes);
                        }
                        Err(e) => {
                            let _ = store.set_content_state(
                                account,
                                "onedrive",
                                id,
                                Some("cached"),
                                Some("cache"),
                                Some("failed"),
                                None,
                            );
                            self.progress.finish(id);
                            return Err(e);
                        }
                    }
                }
            }
        }
        let client = isyncyou_engine::onedrive_lister(&cfg, account)?;
        isyncyou_graph::GraphClient::download_content(&client, id).map_err(|e| e.to_string())
    }
}

impl DaemonRestore {
    /// Construct the desktop restore handler. Mobile uses a queued job handler.
    pub fn new(cfg: Config) -> Self {
        Self { cfg }
    }
}
impl DaemonShare {
    /// Construct the outbound-share handler (daemon-only).
    pub fn new(cfg: Config) -> Self {
        Self { cfg }
    }
}

/// Build the shared "live companion" router (#89): read endpoints + live-write
/// handlers + account-auth + settings + the SSE bus. The desktop daemon extends the
/// result with restore/share/push/sync-control; the standalone mobile client uses it
/// as-is. `gate` serializes store access against an external syncer (daemon only).
/// Re-export so callers of [`build_live_router`] can name the progress-tracker type without a
/// direct `isyncyou-connectors` dependency (the mobile crate has none).
pub use isyncyou_connectors::SharedProgress;

/// Bridges the engine's in-flight transfer tracker (the connectors [`SharedProgress`] the
/// offline pass writes) to the WebUI [`TransferProgress`] endpoint (#655 / S-OM.9). Read-only:
/// `transfers()` snapshots the shared set. `cancel` is a no-op in #655 (the offline pass is
/// synchronous per-file); true cancellation is #656.
///
/// [`SharedProgress`]: isyncyou_connectors::SharedProgress
/// [`TransferProgress`]: isyncyou_webui::TransferProgress
pub struct DaemonTransfer {
    progress: isyncyou_connectors::SharedProgress,
}

impl isyncyou_webui::TransferProgress for DaemonTransfer {
    fn transfers(&self) -> Vec<isyncyou_webui::TransferState> {
        self.progress
            .snapshot()
            .into_iter()
            .map(|s| {
                // #659: a paused transfer lives in the pause-set (not the slot), so derive the flag.
                let paused = self.progress.is_paused_id(&s.id);
                isyncyou_webui::TransferState {
                    id: s.id,
                    name: s.name,
                    bytes_done: s.bytes_done,
                    bytes_total: s.bytes_total,
                    retry_after_secs: s.retry_after_secs,
                    paused,
                }
            })
            .collect()
    }
    fn cancel(&self, id: &str) -> bool {
        // Best-effort, queue-deep cancel (#656): flag the id so the materialize pass skips it
        // before its next file boundary. Always accepted (a download already in flight still
        // completes; the skip applies to the not-yet-started queue).
        self.progress.request_cancel(id);
        true
    }
    fn pause(&self, id: &str) -> bool {
        // #659 queue-deep pause: a persistent skip (unlike cancel, not auto-consumed) the
        // materialize pass re-checks before each file until resumed. An in-flight download
        // still completes; the skip applies to the not-yet-started queue.
        self.progress.request_pause(id);
        true
    }
    fn resume(&self, id: &str) -> bool {
        self.progress.resume(id);
        true
    }
    fn retry(&self, id: &str) -> bool {
        // #659: re-queue a paused/backed-off/failed transfer — clear any pause + 429 backoff so
        // the next materialize pass re-attempts it (queue-deep; a failed item is re-downloaded next
        // pass because the loop re-attempts any non-materialized item).
        self.progress.retry_now(id);
        true
    }
}

/// Live OneDrive **local-body management** for the web UI (#659): free-up / download-now / conflict
/// list+resolve / offline→online cleanup, over the engine wrappers (each opens the account store).
/// Reloads the config fresh from disk on each call so the cleanup enumerates the *just-persisted*
/// folder modes (the mode POST saves before this runs); free-up/download-now/resolve address one
/// item by id. Shares the engine's [`SharedProgress`] so a download-now surfaces in the transfers
/// panel. On mobile keep-mine + cleanup are additionally biometric-gated by the router.
pub struct DaemonOneDriveManage {
    config_path: PathBuf,
    progress: isyncyou_connectors::SharedProgress,
}
impl DaemonOneDriveManage {
    fn cfg(&self) -> Result<Config, String> {
        Config::load(&self.config_path).map_err(|e| format!("load config: {e}"))
    }
}
impl isyncyou_webui::OneDriveManageHandler for DaemonOneDriveManage {
    fn free_up(&self, account: &str, id: &str) -> Result<(), String> {
        isyncyou_engine::free_up_for(&self.cfg()?, account, id).map(|_| ())
    }
    fn download_now(
        &self,
        account: &str,
        id: &str,
    ) -> Result<isyncyou_webui::OneDriveDownloadNowResult, String> {
        let cfg = self.cfg()?;
        let token = isyncyou_engine::auth::resolve_cached_sync_token(&cfg, account)?;
        // An explicit user "download now" is a deliberate single-item action → bypass the
        // background wifi/charging/storage-floor policy the bulk offline pass throttles on.
        let dev = isyncyou_core::policy::DeviceState::always_on(u64::MAX);
        let now = unix_now();
        let result = isyncyou_engine::download_now_for_with_target(
            &cfg,
            account,
            id,
            token,
            dev,
            &now,
            &self.progress,
        )?;
        let target = match result.target {
            isyncyou_connectors::DownloadBodyTarget::Cache => "cache",
            isyncyou_connectors::DownloadBodyTarget::Sync => "sync",
        };
        Ok(isyncyou_webui::OneDriveDownloadNowResult {
            downloaded: result.downloaded,
            target: target.to_string(),
        })
    }
    fn list_conflicts(&self, account: &str) -> Result<serde_json::Value, String> {
        let items = isyncyou_engine::list_conflicts_for(&self.cfg()?, account)?;
        Ok(serde_json::Value::Array(
            items
                .into_iter()
                .map(|it| {
                    serde_json::json!({
                        "id": it.remote_id,
                        "name": it.name,
                        // The write-orphan column stores the keep-both copy's file name.
                        "conflict_copy": it.conflict_state,
                        "content_state": it.content_state,
                        "body_state": it.body_state,
                    })
                })
                .collect(),
        ))
    }
    fn resolve_conflict(&self, account: &str, id: &str, resolution: &str) -> Result<(), String> {
        let cfg = self.cfg()?;
        let res = isyncyou_connectors::ConflictResolution::parse(resolution)
            .ok_or_else(|| format!("unknown resolution '{resolution}'"))?;
        // A keep-mine resolve deletes the cloud copy → needs the write token; keep-both / keep-cloud
        // are local-only but resolve_conflict_for takes the client uniformly (unused for those).
        let token = isyncyou_engine::auth::resolve_cached_sync_token(&cfg, account)?;
        isyncyou_engine::resolve_conflict_for(&cfg, account, id, res, token)
    }
    fn cleanup_offline_to_online(&self, account: &str) -> Result<serde_json::Value, String> {
        let report = isyncyou_engine::cleanup_offline_to_online_for(&self.cfg()?, account)?;
        Ok(serde_json::json!({ "freed": report.freed, "kept": report.kept }))
    }
}

pub struct DaemonOneDriveRisk {
    config_path: PathBuf,
}
impl DaemonOneDriveRisk {
    fn cfg(&self) -> Result<Config, String> {
        Config::load(&self.config_path).map_err(|e| format!("load config: {e}"))
    }

    fn items_for<'a>(
        cfg: &'a Config,
        account: &str,
    ) -> Result<(Option<&'a OneDriveModes>, Vec<Item>), String> {
        let acc = cfg
            .accounts
            .iter()
            .find(|a| a.id == account)
            .ok_or_else(|| format!("no account '{account}'"))?;
        let store = isyncyou_store::Store::open(acc.archive_root.join(".isyncyou-store.db"))
            .map_err(|e| format!("open OneDrive store: {e}"))?;
        let items = store
            .items_by_service(account, "onedrive")
            .map_err(|e| format!("query OneDrive store: {e}"))?;
        Ok((cfg.onedrive_modes.get(account), items))
    }
}
impl isyncyou_webui::OneDriveRiskHandler for DaemonOneDriveRisk {
    fn move_risk(
        &self,
        account: &str,
        item_id: &str,
        destination_parent_id: &str,
    ) -> Result<OneDriveMoveRisk, String> {
        let cfg = self.cfg()?;
        let (modes, items) = Self::items_for(&cfg, account)?;
        let by_id: HashMap<&str, &Item> =
            items.iter().map(|it| (it.remote_id.as_str(), it)).collect();
        Ok(classify_onedrive_move_risk_from_items(
            modes,
            &by_id,
            item_id,
            destination_parent_id,
        ))
    }

    fn offline_mode_risk(&self, account: &str, folder_id: &str) -> Result<OfflineModeRisk, String> {
        let cfg = self.cfg()?;
        let (_modes, items) = Self::items_for(&cfg, account)?;
        let by_id: HashMap<&str, &Item> =
            items.iter().map(|it| (it.remote_id.as_str(), it)).collect();
        Ok(estimate_onedrive_offline_mode_risk_from_items(
            &by_id, folder_id,
        ))
    }
}

pub fn build_live_router(
    cfg: Config,
    gate: Option<Arc<Mutex<()>>>,
    events: Arc<isyncyou_webui::EventBus>,
    config_path: PathBuf,
    live_interval: Arc<AtomicU64>,
    progress: isyncyou_connectors::SharedProgress,
    agent_policy: AgentOperationPolicy,
) -> isyncyou_webui::Router {
    // Agent OAuth credentials live next to the config file (on mobile, app-private
    // filesDir). #627-only local fallback/capture uses the same directory for overrides.
    let oauth_dir = config_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let agent_gate = gate.clone().unwrap_or_else(|| Arc::new(Mutex::new(())));
    let base = match gate {
        Some(g) => isyncyou_webui::Router::with_gate(cfg.clone(), g),
        None => isyncyou_webui::Router::new(cfg.clone()),
    };
    base.with_onedrive_info(Arc::new(DaemonOneDriveInfo { cfg: cfg.clone() }))
        .with_onedrive_list(Arc::new(DaemonOneDriveList { cfg: cfg.clone() }))
        .with_onedrive_open(Arc::new(DaemonOneDriveOpen {
            config_path: config_path.clone(),
            progress: progress.clone(),
        }))
        .with_verify(
            Arc::new(DaemonVerify { cfg: cfg.clone() }),
            mint_cap_token(),
        )
        .with_settings(
            Arc::new(DaemonSettings {
                config_path: config_path.clone(),
                live_interval,
            }),
            mint_cap_token(),
        )
        // #651: OneDrive per-folder mode read/set, wired in the shared builder so both
        // desktop and mobile get it (like with_onedrive_write below).
        .with_onedrive_mode(
            Arc::new(DaemonOneDriveMode {
                config_path: config_path.clone(),
            }),
            mint_cap_token(),
        )
        // #723: OneDrive mobile biometric risk classifier. The router only calls it when the
        // Android biometric gate is active, so desktop avoids this config/store I/O entirely.
        .with_onedrive_risk(Arc::new(DaemonOneDriveRisk {
            config_path: config_path.clone(),
        }) as Arc<dyn isyncyou_webui::OneDriveRiskHandler>)
        .with_mail_write(
            Arc::new(DaemonMailWrite { cfg: cfg.clone() }),
            mint_cap_token(),
        )
        .with_calendar_write(
            Arc::new(DaemonCalendarWrite { cfg: cfg.clone() }),
            mint_cap_token(),
        )
        .with_contact_write(
            Arc::new(DaemonContactWrite { cfg: cfg.clone() }),
            mint_cap_token(),
        )
        .with_task_write(
            Arc::new(DaemonTaskWrite { cfg: cfg.clone() }),
            mint_cap_token(),
        )
        .with_onenote_write(
            Arc::new(DaemonOneNoteWrite { cfg: cfg.clone() }),
            mint_cap_token(),
        )
        .with_account_auth(
            Arc::new(DaemonAccountAuth {
                cfg: cfg.clone(),
                logins: Mutex::new(std::collections::HashMap::new()),
            }),
            mint_cap_token(),
        )
        .with_agent(
            Arc::new(DaemonAgent::new_with_policy(
                cfg.clone(),
                oauth_dir,
                agent_policy,
                agent_gate,
            )) as Arc<dyn isyncyou_webui::AgentHandler>,
            mint_cap_token(),
        )
        // #onedrive-mobile 0.9: outbound sharing is wired here (was daemon-only) so the
        // mobile profile gets it too. On mobile it is additionally biometric-gated (op
        // "share" is in the per-action-token catalogue); the cap token is the CSRF gate.
        // restore-cloud is added by the mobile full-node job wrapper (#625), not here.
        .with_share(
            Arc::new(DaemonShare::new(cfg.clone())) as Arc<dyn isyncyou_webui::ShareHandler>,
            mint_cap_token(),
        )
        // #654: OneDrive cloud-write (create/rename/move/delete) over the operation ledger,
        // wired here so both desktop and mobile get it; on mobile `delete` is biometric-gated.
        .with_onedrive_write(
            Arc::new(DaemonOneDriveWrite::new(cfg.clone()))
                as Arc<dyn isyncyou_webui::OneDriveWriteHandler>,
            mint_cap_token(),
        )
        // #659: OneDrive local-body management (free-up / download-now / conflict list+resolve /
        // offline→online cleanup), wired here so both desktop and mobile get it; on mobile keep-mine
        // + cleanup are biometric-gated. Reloads the config fresh per call (fresh modes for cleanup).
        .with_onedrive_manage(
            Arc::new(DaemonOneDriveManage {
                config_path: config_path.clone(),
                progress: progress.clone(),
            }) as Arc<dyn isyncyou_webui::OneDriveManageHandler>,
            mint_cap_token(),
        )
        // #655: in-flight offline-transfer progress (the engine's SharedProgress) surfaced at
        // GET /api/v1/onedrive/transfers. Empty on desktop (the offline pass is mobile-only).
        .with_transfers(
            Arc::new(DaemonTransfer { progress }) as Arc<dyn isyncyou_webui::TransferProgress>,
            mint_cap_token(),
        )
        .with_events(events)
}

/// Attach the #625 mobile full-node job surface to a shared live router.
///
/// Backup and restore-cloud are queue-only on mobile: the HTTP request thread
/// only creates durable `mobile_jobs`; the worker/recovery path performs the
/// cloud mutation under a job lease. Each route still receives its own cap token
/// and is additionally per-action biometric-token gated by `with_biometric_gate`
/// in `crates/mobile`.
pub fn with_mobile_full_node_jobs(
    router: isyncyou_webui::Router,
    mobile_jobs: Arc<MobileJobRuntime>,
) -> isyncyou_webui::Router {
    let restore: Arc<dyn isyncyou_webui::RestoreHandler> = mobile_jobs.clone();
    let backup: Arc<dyn isyncyou_webui::BackupHandler> = mobile_jobs.clone();
    let jobs: Arc<dyn isyncyou_webui::MobileJobHandler> = mobile_jobs;
    router
        .with_restore(restore, mint_cap_token())
        .with_backup(backup, mint_cap_token())
        .with_mobile_jobs(jobs, mint_cap_token())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "agent-network-device-test-hooks")]
    use isyncyou_webui::AgentHandler;
    use isyncyou_webui::ApiRequest;
    use std::sync::{Mutex as StdMutex, OnceLock as StdOnceLock};

    static ENVELOPE_REQUIREMENT_TEST_LOCK: StdOnceLock<StdMutex<()>> = StdOnceLock::new();
    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    static APP_HOST_CREDENTIAL_ENV_TEST_LOCK: StdOnceLock<StdMutex<()>> = StdOnceLock::new();

    struct EnvelopeRequirementGuard {
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvelopeRequirementGuard {
        fn new() -> Self {
            let guard = ENVELOPE_REQUIREMENT_TEST_LOCK
                .get_or_init(|| StdMutex::new(()))
                .lock()
                .unwrap();
            isyncyou_core::envelope::reset_body_envelope_requirement_for_tests();
            isyncyou_core::envelope::reset_body_keys_for_tests();
            Self { _guard: guard }
        }
    }

    impl Drop for EnvelopeRequirementGuard {
        fn drop(&mut self) {
            isyncyou_core::envelope::reset_body_keys_for_tests();
            isyncyou_core::envelope::reset_body_envelope_requirement_for_tests();
        }
    }

    #[derive(Clone)]
    struct RecordingConfirmedExecutor {
        calls: Arc<StdMutex<Vec<isyncyou_agent::ToolAction>>>,
        result: Arc<StdMutex<Result<ConfirmedActionResult, String>>>,
        order: Arc<StdMutex<Vec<String>>>,
    }

    impl RecordingConfirmedExecutor {
        fn ok(summary: &str, order: Arc<StdMutex<Vec<String>>>) -> Self {
            Self {
                calls: Arc::new(StdMutex::new(Vec::new())),
                result: Arc::new(StdMutex::new(Ok(ConfirmedActionResult::new(summary)))),
                order,
            }
        }

        fn err(error: &str, order: Arc<StdMutex<Vec<String>>>) -> Self {
            Self {
                calls: Arc::new(StdMutex::new(Vec::new())),
                result: Arc::new(StdMutex::new(Err(error.to_string()))),
                order,
            }
        }

        fn set_error(&self, error: impl Into<String>) {
            *self.result.lock().unwrap() = Err(error.into());
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl AgentConfirmedActionExecutor for RecordingConfirmedExecutor {
        fn execute_confirmed(
            &self,
            action: &isyncyou_agent::ToolAction,
        ) -> Result<ConfirmedActionResult, String> {
            self.order.lock().unwrap().push("execute".to_string());
            self.calls.lock().unwrap().push(action.clone());
            self.result.lock().unwrap().clone()
        }
    }

    #[derive(Clone)]
    struct RecordingAuditSink {
        events: Arc<StdMutex<Vec<(String, String, String)>>>,
        order: Arc<StdMutex<Vec<String>>>,
        fail_start: bool,
    }

    impl RecordingAuditSink {
        fn new(order: Arc<StdMutex<Vec<String>>>) -> Self {
            Self {
                events: Arc::new(StdMutex::new(Vec::new())),
                order,
                fail_start: false,
            }
        }

        fn failing_start(order: Arc<StdMutex<Vec<String>>>) -> Self {
            Self {
                fail_start: true,
                ..Self::new(order)
            }
        }

        fn events(&self) -> Vec<(String, String, String)> {
            self.events.lock().unwrap().clone()
        }
    }

    impl AgentAuditSink for RecordingAuditSink {
        fn record_confirm(
            &self,
            action: &isyncyou_agent::ToolAction,
            status: &str,
            summary: &str,
        ) -> Result<(), String> {
            self.order.lock().unwrap().push(format!("audit:{status}"));
            if self.fail_start && status == "started" {
                return Err("audit_start_failed".to_string());
            }
            self.events.lock().unwrap().push((
                action.op().to_string(),
                status.to_string(),
                summary.to_string(),
            ));
            Ok(())
        }
    }

    fn backup_action() -> isyncyou_agent::ToolAction {
        isyncyou_agent::parse_action(
            &serde_json::json!({"op":"backup","account":"me","services":["mail"]}),
        )
        .unwrap()
    }

    fn temp_agent_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "isy-apphost-agent-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn production_source_before_final_test_module(source: &str) -> &str {
        let marker = "\n#[cfg(test)]\nmod tests {";
        let final_tests = source
            .rfind(marker)
            .expect("app-host must keep one final cfg(test) module");
        &source[..final_tests]
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    struct AppHostCredentialEnvGuard {
        _guard: std::sync::MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
        root: PathBuf,
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    impl AppHostCredentialEnvGuard {
        fn new() -> Self {
            let guard = APP_HOST_CREDENTIAL_ENV_TEST_LOCK
                .get_or_init(|| StdMutex::new(()))
                .lock()
                .unwrap();
            let saved = Self::env_keys()
                .iter()
                .copied()
                .map(|key| (key, std::env::var_os(key)))
                .collect();
            let root = apphost_credential_test_root("provider-test-env");
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let this = Self {
                _guard: guard,
                saved,
                root,
            };
            this.isolate_provider_env();
            this
        }

        fn env_keys() -> &'static [&'static str] {
            &[
                "HOME",
                "CODEX_HOME",
                "CLAUDE_CONFIG_DIR",
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "OPENAI_API_KEY",
                "ISYNCYOU_AGENT_CRED_KEY",
                "ISYNCYOU_AGENT_PROVIDER",
                "ISYNCYOU_AGENT_MODEL",
            ]
        }

        fn isolate_provider_env(&self) {
            let home = self.root.join("home");
            let codex = self.root.join("codex-home");
            let claude = self.root.join("claude-config");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::create_dir_all(&codex).unwrap();
            std::fs::create_dir_all(&claude).unwrap();
            std::env::set_var("HOME", &home);
            std::env::set_var("CODEX_HOME", &codex);
            std::env::set_var("CLAUDE_CONFIG_DIR", &claude);
            for key in [
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "OPENAI_API_KEY",
                "ISYNCYOU_AGENT_CRED_KEY",
                "ISYNCYOU_AGENT_PROVIDER",
                "ISYNCYOU_AGENT_MODEL",
            ] {
                std::env::remove_var(key);
            }
        }

        fn set_home(&self, home: &Path) {
            std::env::set_var("HOME", home);
        }

        fn use_home_fallbacks(&self, home: &Path) {
            self.set_home(home);
            std::env::remove_var("CLAUDE_CONFIG_DIR");
            std::env::remove_var("CODEX_HOME");
        }

        fn set_claude_config_dir(&self, root: &Path) {
            std::env::set_var("CLAUDE_CONFIG_DIR", root);
        }

        #[cfg(feature = "agent-subscription-experimental")]
        fn set_codex_home(&self, root: &Path) {
            std::env::set_var("CODEX_HOME", root);
        }
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    impl Drop for AppHostCredentialEnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn apphost_credential_test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "isy-apphost-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn write_local_cli_fixture(path: &Path, value: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, value).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn assert_product_credential_turn_fails_closed(agent: &DaemonAgent) {
        // The credential still resolves to the error provider for the selection probe (unchanged).
        assert_eq!(
            agent.build_turn_provider("system").name(),
            "credential-resolution-error"
        );
        // #639 T7: a not-ready product credential fails closed AT THE GATE — start_turn returns the
        // closed `product_not_ready` code and creates NO turn state (no stream slot, no provider
        // call, no streamed error/done turn). This is the source of the router's 409.
        let err = isyncyou_webui::AgentHandler::start_turn(agent, "me", "hello").unwrap_err();
        assert_eq!(err, "product_not_ready");
        assert_eq!(agent.unopened_stream_count_for_tests(), 0);
        assert!(isyncyou_webui::AgentHandler::open_stream(agent, "turn-0-0").is_none());
    }

    #[test]
    fn agent_confirm_audits_once_and_calls_executor_once() {
        let order = Arc::new(StdMutex::new(Vec::new()));
        let executor = RecordingConfirmedExecutor::ok("backup accepted", order.clone());
        let audit = RecordingAuditSink::new(order.clone());
        let root = temp_agent_root("confirm-ok");
        let agent = DaemonAgent::with_test_confirm_components(
            Config::default(),
            root.clone(),
            Arc::new(executor.clone()),
            Arc::new(audit.clone()),
        );
        let (pending, token) = agent
            .pending
            .register(
                backup_action(),
                "backup mail",
                unix_now_ms(),
                AGENT_CONFIRM_TTL_MS,
            )
            .unwrap();

        let result = isyncyou_webui::AgentHandler::confirm(
            &agent,
            &pending.id,
            &token,
            &pending.action_hash,
        )
        .unwrap();
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["status"], "ok");
        assert_eq!(result["op"], "backup");
        assert_eq!(result["summary"], "backup accepted");
        assert_eq!(executor.call_count(), 1);
        assert_eq!(
            order.lock().unwrap().as_slice(),
            ["audit:started", "execute", "audit:ok"]
        );
        let events = audit.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].1, "started");
        assert_eq!(events[1].1, "ok");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn agent_confirm_with_desktop_policy_uses_real_operation_executor() {
        let order = Arc::new(StdMutex::new(Vec::new()));
        let audit = RecordingAuditSink::new(order);
        let root = temp_agent_root("confirm-desktop-policy");
        let mut agent = DaemonAgent::new_with_policy(
            Config::default(),
            root.clone(),
            AgentOperationPolicy::DesktopEnabled,
            Arc::new(Mutex::new(())),
        );
        agent.audit_sink = Arc::new(audit.clone());
        let (pending, token) = agent
            .pending
            .register(
                backup_action(),
                "backup mail",
                unix_now_ms(),
                AGENT_CONFIRM_TTL_MS,
            )
            .unwrap();

        let err = isyncyou_webui::AgentHandler::confirm(
            &agent,
            &pending.id,
            &token,
            &pending.action_hash,
        )
        .unwrap_err();

        assert_eq!(err, "backup failed: execution_failed");
        let events = audit.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].1, "started");
        assert_eq!(events[1].1, "error");
        assert!(!events[1].2.contains("not_available_on_mobile"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn agent_confirm_revalidates_action_before_execution() {
        let order = Arc::new(StdMutex::new(Vec::new()));
        let executor = RecordingConfirmedExecutor::ok("must not run", order.clone());
        let audit = RecordingAuditSink::new(order);
        let root = temp_agent_root("confirm-revalidate");
        let agent = DaemonAgent::with_test_confirm_components(
            Config::default(),
            root.clone(),
            Arc::new(executor.clone()),
            Arc::new(audit.clone()),
        );
        let action = isyncyou_agent::parse_action(&serde_json::json!({
            "op": "live-write",
            "account": "me",
            "service": "mail",
            "target": "drafts",
            "change": {
                "body_html": "<html>raw-body-sentinel recipient@example.com</html>",
                "to": ["recipient@example.com"]
            }
        }))
        .unwrap();
        let (pending, token) = agent
            .pending
            .register(
                action,
                "invalid live write",
                unix_now_ms(),
                AGENT_CONFIRM_TTL_MS,
            )
            .unwrap();

        let err = isyncyou_webui::AgentHandler::confirm(
            &agent,
            &pending.id,
            &token,
            &pending.action_hash,
        )
        .unwrap_err();

        assert!(err.contains("invalid_confirmed_action"));
        assert!(err.contains("missing verb"));
        assert!(!err.contains("raw-body-sentinel"));
        assert!(!err.contains("recipient@example.com"));
        assert_eq!(executor.call_count(), 0);
        assert!(audit.events().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn agent_confirm_read_class_action_cannot_execute_in_confirm_path() {
        let order = Arc::new(StdMutex::new(Vec::new()));
        let executor = RecordingConfirmedExecutor::ok("must not run", order.clone());
        let audit = RecordingAuditSink::new(order);
        let root = temp_agent_root("confirm-read-class");
        let agent = DaemonAgent::with_test_confirm_components(
            Config::default(),
            root.clone(),
            Arc::new(executor.clone()),
            Arc::new(audit.clone()),
        );
        let action = isyncyou_agent::parse_action(&serde_json::json!({
            "op": "search",
            "account": "me",
            "services": ["mail"],
            "query": "status"
        }))
        .unwrap();
        let (pending, token) = agent
            .pending
            .register(
                action,
                "search should not confirm",
                unix_now_ms(),
                AGENT_CONFIRM_TTL_MS,
            )
            .unwrap();

        let err = isyncyou_webui::AgentHandler::confirm(
            &agent,
            &pending.id,
            &token,
            &pending.action_hash,
        )
        .unwrap_err();

        assert!(err.contains("invalid_confirmed_action"));
        assert!(err.contains("not_confirmable: search"));
        assert_eq!(executor.call_count(), 0);
        assert!(audit.events().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn agent_confirm_result_summary_is_json_and_redacted() {
        let order = Arc::new(StdMutex::new(Vec::new()));
        let executor = RecordingConfirmedExecutor::ok(
            "backup wrote https://tenant.example/item?code=secret for owner@example.com",
            order.clone(),
        );
        let audit = RecordingAuditSink::new(order);
        let root = temp_agent_root("confirm-json-redacted");
        let agent = DaemonAgent::with_test_confirm_components(
            Config::default(),
            root.clone(),
            Arc::new(executor.clone()),
            Arc::new(audit.clone()),
        );
        let (pending, token) = agent
            .pending
            .register(
                backup_action(),
                "backup mail",
                unix_now_ms(),
                AGENT_CONFIRM_TTL_MS,
            )
            .unwrap();

        let result = isyncyou_webui::AgentHandler::confirm(
            &agent,
            &pending.id,
            &token,
            &pending.action_hash,
        )
        .unwrap();
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        let summary = result["summary"].as_str().unwrap();

        assert_eq!(result["status"], "ok");
        assert_eq!(result["op"], "backup");
        assert!(summary.contains("<redacted-url>"));
        assert!(summary.contains("<redacted-email>"));
        assert!(!summary.contains("tenant.example"));
        assert!(!summary.contains("owner@example.com"));
        assert_eq!(executor.call_count(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn agent_confirm_replay_rejected_and_executor_not_called_twice() {
        let order = Arc::new(StdMutex::new(Vec::new()));
        let executor = RecordingConfirmedExecutor::ok("backup accepted", order.clone());
        let audit = RecordingAuditSink::new(order);
        let root = temp_agent_root("confirm-replay");
        let agent = DaemonAgent::with_test_confirm_components(
            Config::default(),
            root.clone(),
            Arc::new(executor.clone()),
            Arc::new(audit.clone()),
        );
        let (pending, token) = agent
            .pending
            .register(
                backup_action(),
                "backup mail",
                unix_now_ms(),
                AGENT_CONFIRM_TTL_MS,
            )
            .unwrap();

        isyncyou_webui::AgentHandler::confirm(&agent, &pending.id, &token, &pending.action_hash)
            .unwrap();
        let replay = isyncyou_webui::AgentHandler::confirm(
            &agent,
            &pending.id,
            &token,
            &pending.action_hash,
        )
        .unwrap_err();
        assert!(replay.contains("NotFound"));
        assert_eq!(executor.call_count(), 1);
        assert_eq!(audit.events().len(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn agent_confirm_executor_error_is_audited_without_revealing_token() {
        let order = Arc::new(StdMutex::new(Vec::new()));
        let executor = RecordingConfirmedExecutor::err("placeholder", order.clone());
        let audit = RecordingAuditSink::new(order.clone());
        let root = temp_agent_root("confirm-exec-error");
        let agent = DaemonAgent::with_test_confirm_components(
            Config::default(),
            root.clone(),
            Arc::new(executor.clone()),
            Arc::new(audit.clone()),
        );
        let (pending, token) = agent
            .pending
            .register(
                backup_action(),
                "backup mail",
                unix_now_ms(),
                AGENT_CONFIRM_TTL_MS,
            )
            .unwrap();
        executor.set_error(format!(
            "provider token leaked? token={token} cap=cap-secret"
        ));

        let err = isyncyou_webui::AgentHandler::confirm(
            &agent,
            &pending.id,
            &token,
            &pending.action_hash,
        )
        .unwrap_err();
        assert_eq!(err, "backup failed: execution_failed");
        assert_eq!(executor.call_count(), 1);
        assert_eq!(
            order.lock().unwrap().as_slice(),
            ["audit:started", "execute", "audit:error"]
        );
        let audit_text = serde_json::to_string(&audit.events()).unwrap();
        assert!(!audit_text.contains(&token));
        assert!(!audit_text.contains("cap-secret"));
        assert!(!audit_text.contains("provider token leaked"));
        assert!(audit_text.contains("execution_failed"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn agent_restore_cloud_error_is_redacted_in_api_and_audit() {
        let order = Arc::new(StdMutex::new(Vec::new()));
        let executor = RecordingConfirmedExecutor::err(
            "restore failed for https://tenant.example/path?code=oauth-code and user@example.com",
            order.clone(),
        );
        let audit = RecordingAuditSink::new(order);
        let root = temp_agent_root("confirm-restore-redaction");
        let agent = DaemonAgent::with_test_confirm_components(
            Config::default(),
            root.clone(),
            Arc::new(executor),
            Arc::new(audit.clone()),
        );
        let action = isyncyou_agent::parse_action(&serde_json::json!({
            "op": "restore-cloud",
            "account": "owner@example.com",
            "service": "mail",
            "id": "https://tenant.example/item?code=secret user@example.com"
        }))
        .unwrap();
        let (pending, token) = agent
            .pending
            .register(action, "restore cloud", unix_now_ms(), AGENT_CONFIRM_TTL_MS)
            .unwrap();

        let err = isyncyou_webui::AgentHandler::confirm(
            &agent,
            &pending.id,
            &token,
            &pending.action_hash,
        )
        .unwrap_err();

        assert_eq!(err, "restore-cloud failed: execution_failed");
        assert!(!err.contains("tenant.example"));
        assert!(!err.contains("user@example.com"));
        let audit_text = serde_json::to_string(&audit.events()).unwrap();
        assert!(!audit_text.contains("tenant.example"));
        assert!(!audit_text.contains("owner@example.com"));
        assert!(!audit_text.contains("user@example.com"));
        assert!(!audit_text.contains("oauth-code"));
        assert!(!audit_text.contains("restore failed for"));
        assert!(!audit_text.contains("<redacted-url>"));
        assert!(audit_text.contains("<redacted-email>"));
        assert!(audit_text.contains("execution_failed"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn agent_share_invite_audit_summary_redacts_recipient_emails() {
        let action = isyncyou_agent::parse_action(&serde_json::json!({
            "op": "share",
            "account": "owner@example.com",
            "service": "onedrive",
            "id": "item-1",
            "mode": "invite",
            "recipients": ["recipient@example.com"],
            "role": "read"
        }))
        .unwrap();

        let summary = agent_action_summary(&action);

        assert!(summary.contains("op=share"));
        assert!(summary.contains("service=onedrive"));
        assert!(!summary.contains("item-1"));
        assert!(!summary.contains("owner@example.com"));
        assert!(!summary.contains("recipient@example.com"));
        assert!(summary.contains("<redacted-email>"));
    }

    #[test]
    fn agent_live_write_audit_summary_omits_target_identifiers() {
        let action = isyncyou_agent::parse_action(&serde_json::json!({
            "op": "live-write",
            "account": "me",
            "service": "todo",
            "target": "private-task-id",
            "change": { "verb": "complete", "list_id": "private-list-id" }
        }))
        .unwrap();

        let summary = agent_action_summary(&action);

        assert!(summary.contains("op=live-write"));
        assert!(summary.contains("service=todo"));
        assert!(!summary.contains("private-task-id"));
        assert!(!summary.contains("private-list-id"));
    }

    #[test]
    fn agent_live_write_body_html_is_not_audited() {
        let action = isyncyou_agent::parse_action(&serde_json::json!({
            "op": "live-write",
            "account": "owner@example.com",
            "service": "mail",
            "change": {
                "verb": "create_draft",
                "subject": "private subject",
                "body_html": "<html>raw-body-sentinel recipient@example.com</html>",
                "to": ["recipient@example.com"]
            }
        }))
        .unwrap();

        let summary = agent_action_summary(&action);

        assert!(summary.contains("op=live-write"));
        assert!(summary.contains("service=mail"));
        assert!(!summary.contains("owner@example.com"));
        assert!(!summary.contains("recipient@example.com"));
        assert!(!summary.contains("raw-body-sentinel"));
        assert!(!summary.contains("private subject"));
        assert!(summary.contains("<redacted-email>"));
    }

    #[test]
    fn agent_confirm_destructive_binding_redacts_live_write_change() {
        let order = Arc::new(StdMutex::new(Vec::new()));
        let executor = RecordingConfirmedExecutor::ok("must not run", order.clone());
        let audit = RecordingAuditSink::new(order);
        let root = temp_agent_root("confirm-binding-redacts-live-write");
        let agent = DaemonAgent::with_test_confirm_components(
            Config::default(),
            root.clone(),
            Arc::new(executor),
            Arc::new(audit),
        );
        let action = isyncyou_agent::parse_action(&serde_json::json!({
            "op": "live-write",
            "account": "owner@example.com",
            "service": "mail",
            "target": "msg-1",
            "change": {
                "verb": "create_draft",
                "subject": "private subject",
                "body_html": "<html>raw-body-sentinel recipient@example.com</html>",
                "to": ["recipient@example.com"]
            }
        }))
        .unwrap();
        let (pending, _token) = agent
            .pending
            .register(action, "live write", unix_now_ms(), AGENT_CONFIRM_TTL_MS)
            .unwrap();

        let binding = isyncyou_webui::AgentHandler::pending_binding(
            &agent,
            &pending.id,
            &pending.action_hash,
        )
        .unwrap();
        let text = serde_json::to_string(&serde_json::json!({
            "op": binding.op,
            "account": binding.account,
            "service": binding.service,
            "item": binding.item,
        }))
        .unwrap();

        assert!(text.contains("live-write"));
        assert!(text.contains("mail"));
        assert!(text.contains(&pending.id));
        assert!(text.contains(&pending.action_hash));
        assert!(!text.contains("raw-body-sentinel"));
        assert!(!text.contains("recipient@example.com"));
        assert!(!text.contains("private subject"));
        assert!(!text.contains("create_draft"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn agent_confirm_unknown_or_expired_pending_does_not_audit_execution() {
        let order = Arc::new(StdMutex::new(Vec::new()));
        let executor = RecordingConfirmedExecutor::ok("backup accepted", order.clone());
        let audit = RecordingAuditSink::new(order);
        let root = temp_agent_root("confirm-invalid");
        let agent = DaemonAgent::with_test_confirm_components(
            Config::default(),
            root.clone(),
            Arc::new(executor.clone()),
            Arc::new(audit.clone()),
        );

        let unknown =
            isyncyou_webui::AgentHandler::confirm(&agent, "missing", "token", "hash").unwrap_err();
        assert!(unknown.contains("NotFound"));
        let (pending, token) = agent
            .pending
            .register(backup_action(), "backup mail", 0, 1)
            .unwrap();
        let expired = isyncyou_webui::AgentHandler::confirm(
            &agent,
            &pending.id,
            &token,
            &pending.action_hash,
        )
        .unwrap_err();
        assert!(expired.contains("Expired"));
        assert_eq!(executor.call_count(), 0);
        assert!(audit.events().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn agent_confirm_audit_start_failure_does_not_execute() {
        let order = Arc::new(StdMutex::new(Vec::new()));
        let executor = RecordingConfirmedExecutor::ok("backup accepted", order.clone());
        let audit = RecordingAuditSink::failing_start(order.clone());
        let root = temp_agent_root("confirm-audit-fail");
        let agent = DaemonAgent::with_test_confirm_components(
            Config::default(),
            root.clone(),
            Arc::new(executor.clone()),
            Arc::new(audit),
        );
        let (pending, token) = agent
            .pending
            .register(
                backup_action(),
                "backup mail",
                unix_now_ms(),
                AGENT_CONFIRM_TTL_MS,
            )
            .unwrap();

        let err = isyncyou_webui::AgentHandler::confirm(
            &agent,
            &pending.id,
            &token,
            &pending.action_hash,
        )
        .unwrap_err();
        assert_eq!(err, "audit_start_failed");
        assert_eq!(executor.call_count(), 0);
        assert_eq!(order.lock().unwrap().as_slice(), ["audit:started"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn agent_confirm_mobile_policy_refuses_before_mutation() {
        let order = Arc::new(StdMutex::new(Vec::new()));
        let audit = RecordingAuditSink::new(order.clone());
        let root = temp_agent_root("confirm-mobile-disabled");
        let mut agent = DaemonAgent::new_with_policy(
            Config::default(),
            root.clone(),
            AgentOperationPolicy::MobileDisabled,
            Arc::new(Mutex::new(())),
        );
        agent.audit_sink = Arc::new(audit.clone());
        let (pending, token) = agent
            .pending
            .register(
                backup_action(),
                "backup mail",
                unix_now_ms(),
                AGENT_CONFIRM_TTL_MS,
            )
            .unwrap();

        let err = isyncyou_webui::AgentHandler::confirm(
            &agent,
            &pending.id,
            &token,
            &pending.action_hash,
        )
        .unwrap_err();

        assert_eq!(err, "backup failed: not_available_on_mobile");
        assert_eq!(
            order.lock().unwrap().as_slice(),
            ["audit:started", "audit:error"]
        );
        let events = audit.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].1, "started");
        assert_eq!(events[1].1, "error");
        assert!(events[1].2.contains("not_available_on_mobile"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn daemon_agent_fake_turn_streams_tokens_to_subscriber() {
        let script = vec![vec![isyncyou_agent::AssistantBlock::Text(
            "hello world".into(),
        )]];
        let order = Arc::new(StdMutex::new(Vec::new()));
        let executor = RecordingConfirmedExecutor::ok("unused", order.clone());
        let audit = RecordingAuditSink::new(order);
        let root = temp_agent_root("fake-token-stream");
        let agent = DaemonAgent::with_test_provider_script_and_confirm_components(
            Config::default(),
            root.clone(),
            script,
            Arc::new(executor.clone()),
            Arc::new(audit),
        );

        let turn = isyncyou_webui::AgentHandler::start_turn(&agent, "me", "hello").unwrap();
        let rx = isyncyou_webui::AgentHandler::open_stream(&agent, &turn).expect("turn stream");
        let mut tokens = Vec::new();
        let mut done_reason = String::new();
        for _ in 0..6 {
            let line = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("agent stream event");
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            match event["event"].as_str() {
                Some("token") => tokens.push(event["text"].as_str().unwrap().to_string()),
                Some("done") => {
                    done_reason = event["reason"].as_str().unwrap().to_string();
                    break;
                }
                other => panic!("unexpected event: {other:?} {line}"),
            }
        }
        assert_eq!(tokens, ["hello ", "world"]);
        assert_eq!(done_reason, "complete");
        assert_eq!(executor.call_count(), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn daemon_agent_destructive_turn_registers_pending_without_executing() {
        let script = vec![vec![isyncyou_agent::AssistantBlock::ToolUse {
            id: "tool-1".into(),
            input: serde_json::json!({"op":"backup","account":"me","services":["mail"]}),
        }]];
        let order = Arc::new(StdMutex::new(Vec::new()));
        let executor = RecordingConfirmedExecutor::ok("backup accepted", order.clone());
        let audit = RecordingAuditSink::new(order);
        let root = temp_agent_root("fake-pending-no-exec");
        let agent = DaemonAgent::with_test_provider_script_and_confirm_components(
            Config::default(),
            root.clone(),
            script,
            Arc::new(executor.clone()),
            Arc::new(audit.clone()),
        );

        let turn = isyncyou_webui::AgentHandler::start_turn(&agent, "me", "back up mail").unwrap();
        let rx = isyncyou_webui::AgentHandler::open_stream(&agent, &turn).expect("turn stream");
        let mut pending_id = String::new();
        for _ in 0..8 {
            let line = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("agent stream event");
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            if event["event"] == "confirmation_required" {
                pending_id = event["pending_id"].as_str().unwrap().to_string();
                break;
            }
        }
        assert!(!pending_id.is_empty());
        assert_eq!(executor.call_count(), 0);
        assert!(audit.events().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn agent_destructive_preview_is_redacted_before_pending_registration() {
        let script = vec![vec![isyncyou_agent::AssistantBlock::ToolUse {
            id: "tool-1".into(),
            input: serde_json::json!({
                "op":"share",
                "account":"me",
                "service":"onedrive",
                "id":"file-1",
                "recipient":"recipient@example.com"
            }),
        }]];
        let order = Arc::new(StdMutex::new(Vec::new()));
        let executor = RecordingConfirmedExecutor::ok("share accepted", order.clone());
        let audit = RecordingAuditSink::new(order);
        let root = temp_agent_root("pending-preview-redacted");
        let agent = DaemonAgent::with_test_provider_script_and_confirm_components(
            Config::default(),
            root.clone(),
            script,
            Arc::new(executor),
            Arc::new(audit),
        );

        let turn = isyncyou_webui::AgentHandler::start_turn(&agent, "me", "share file").unwrap();
        let rx = isyncyou_webui::AgentHandler::open_stream(&agent, &turn).expect("turn stream");
        let mut preview = String::new();
        let mut tool_call_input = serde_json::Value::Null;
        for _ in 0..8 {
            let line = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("agent stream event");
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            match event["event"].as_str() {
                Some("tool_call") => tool_call_input = event["input"].clone(),
                Some("confirmation_required") => {
                    preview = event["preview"].as_str().unwrap().to_string();
                    break;
                }
                other => panic!("unexpected event before confirmation: {other:?} {line}"),
            }
        }

        assert_eq!(agent.pending.len(), 1);
        assert!(preview.contains("Invite 1 recipient"));
        assert!(!preview.contains("recipient@example.com"));
        assert_eq!(tool_call_input["redacted"], true);
        assert_eq!(tool_call_input["recipient_count"], 1);
        assert!(!tool_call_input
            .to_string()
            .contains("recipient@example.com"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_live_write_does_not_register_pending() {
        let script = vec![vec![isyncyou_agent::AssistantBlock::ToolUse {
            id: "tool-1".into(),
            input: serde_json::json!({
                "op":"live-write",
                "account":"me",
                "service":"mail",
                "target":"drafts",
                "change":{
                    "body":"<html>raw-body-sentinel</html>",
                    "to":["recipient@example.com"]
                }
            }),
        }]];
        let order = Arc::new(StdMutex::new(Vec::new()));
        let executor = RecordingConfirmedExecutor::ok("must not run", order.clone());
        let audit = RecordingAuditSink::new(order);
        let root = temp_agent_root("invalid-live-write-no-pending");
        let agent = DaemonAgent::with_test_provider_script_and_confirm_components(
            Config::default(),
            root.clone(),
            script,
            Arc::new(executor.clone()),
            Arc::new(audit),
        );

        let turn = isyncyou_webui::AgentHandler::start_turn(&agent, "me", "draft mail").unwrap();
        let rx = isyncyou_webui::AgentHandler::open_stream(&agent, &turn).expect("turn stream");
        let mut saw_error = String::new();
        let mut done_reason = String::new();
        let mut saw_confirmation = false;
        for _ in 0..8 {
            let line = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("agent stream event");
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            match event["event"].as_str() {
                Some("error") => saw_error = event["message"].as_str().unwrap().to_string(),
                Some("confirmation_required") => saw_confirmation = true,
                Some("done") => {
                    done_reason = event["reason"].as_str().unwrap().to_string();
                    break;
                }
                Some("tool_call") => {
                    let text = event["input"].to_string();
                    assert!(!text.contains("recipient@example.com"));
                    assert!(!text.contains("raw-body-sentinel"));
                }
                _ => {}
            }
        }

        assert!(!saw_confirmation);
        assert!(saw_error.contains("invalid_live_write"));
        assert!(!saw_error.contains("recipient@example.com"));
        assert!(!saw_error.contains("raw-body-sentinel"));
        assert_eq!(done_reason, "error");
        assert_eq!(agent.pending.len(), 0);
        assert_eq!(executor.call_count(), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    fn issue_624_live_write_token() -> Option<String> {
        if let Ok(path) = std::env::var("ISY624_M365_WRITE_TOKEN_FILE") {
            let token = std::fs::read_to_string(path).ok()?;
            let token = token.trim().to_string();
            if !token.is_empty() {
                return Some(token);
            }
        }
        std::env::var("ISY624_M365_WRITE_TOKEN")
            .ok()
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())
    }

    fn issue_624_live_config(root: &std::path::Path, access_token: &str) -> Config {
        let archive = root.join("archive");
        let sync = root.join("sync");
        let cache = root.join("cache");
        std::fs::create_dir_all(&archive).unwrap();
        std::fs::create_dir_all(&sync).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let token_cache = serde_json::json!({
            "access_token": access_token,
            "refresh_token": null,
            "expires_at": unix_now().parse::<u64>().unwrap_or(0).saturating_add(1800),
        });
        let token_cache_bytes = serde_json::to_vec_pretty(&token_cache).unwrap();
        std::fs::write(
            archive.join(isyncyou_engine::auth::WRITE_CACHE_FILE),
            &token_cache_bytes,
        )
        .unwrap();
        std::fs::write(
            archive.join(isyncyou_engine::auth::READ_CACHE_FILE),
            &token_cache_bytes,
        )
        .unwrap();
        let cfg = Config {
            accounts: vec![isyncyou_core::AccountConfig {
                id: "me".to_string(),
                username: "redacted-live-account".to_string(),
                sync_root: sync,
                archive_root: archive,
                cache_root: cache,
                mount_point: None,
            }],
            ..Default::default()
        };
        cfg.validate().unwrap();
        cfg
    }

    fn issue_624_collect_pending(
        agent: &DaemonAgent,
        prompt: &str,
        expected_op: &str,
    ) -> (String, String, String) {
        let turn = isyncyou_webui::AgentHandler::start_turn(agent, "me", prompt).unwrap();
        let rx = isyncyou_webui::AgentHandler::open_stream(agent, &turn).expect("turn stream");
        let mut pending_id = String::new();
        let mut confirm_token = String::new();
        let mut action_hash = String::new();
        let mut saw_redacted_tool_call = false;
        for _ in 0..8 {
            let line = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("agent stream event");
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            match event["event"].as_str() {
                Some("tool_call") => {
                    saw_redacted_tool_call = true;
                    assert_eq!(event["input"]["op"].as_str(), Some(expected_op));
                    assert!(event["input"]["redacted"].as_bool().unwrap_or(false));
                }
                Some("confirmation_required") => {
                    pending_id = event["pending_id"].as_str().unwrap().to_string();
                    confirm_token = event["token"].as_str().unwrap().to_string();
                    action_hash = event["action_hash"].as_str().unwrap().to_string();
                    break;
                }
                other => panic!("unexpected event before confirmation: {other:?} {line}"),
            }
        }
        assert!(saw_redacted_tool_call);
        assert!(!pending_id.is_empty());
        assert!(!confirm_token.is_empty());
        assert!(!action_hash.is_empty());
        (pending_id, confirm_token, action_hash)
    }

    fn issue_624_url_segment(value: &str) -> String {
        let mut out = String::with_capacity(value.len());
        for byte in value.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(byte as char)
                }
                _ => out.push_str(&format!("%{byte:02X}")),
            }
        }
        out
    }

    struct Issue624DraftCleanup {
        token: String,
        draft_id: Option<String>,
    }

    impl Issue624DraftCleanup {
        fn set(&mut self, draft_id: String) {
            self.draft_id = Some(draft_id);
        }
    }

    impl Drop for Issue624DraftCleanup {
        fn drop(&mut self) {
            if let Some(id) = self.draft_id.take() {
                let graph = isyncyou_graph::GraphClient::new(self.token.clone());
                let _ = graph.delete_message(&id);
            }
        }
    }

    #[test]
    #[ignore = "requires a live Microsoft 365 write token via ISY624_M365_WRITE_TOKEN_FILE or ISY624_M365_WRITE_TOKEN"]
    fn live_issue_624_agent_confirm_create_draft_and_cleanup() {
        let access_token = issue_624_live_write_token()
            .expect("set ISY624_M365_WRITE_TOKEN_FILE or ISY624_M365_WRITE_TOKEN");
        let root = temp_agent_root("live-issue-624-confirm-draft");
        let cfg = issue_624_live_config(&root, &access_token);
        let gate = Arc::new(Mutex::new(()));
        let executor = agent_ops::confirmed_executor_for_policy(
            AgentOperationPolicy::DesktopEnabled,
            cfg.clone(),
            gate,
        );
        let audit = Arc::new(StoreAgentAuditSink { cfg: cfg.clone() });
        let subject = format!("isyncyou issue 624 live confirm {}", unix_now());
        let recipient = std::env::var("ISY624_M365_DRAFT_TO")
            .unwrap_or_else(|_| "recipient@example.invalid".to_string());
        let recipient_for_assert = recipient.clone();
        let script = vec![vec![isyncyou_agent::AssistantBlock::ToolUse {
            id: "tool-1".into(),
            input: serde_json::json!({
                "op":"live-write",
                "account":"me",
                "service":"mail",
                "change":{
                    "verb":"create_draft",
                    "subject": subject,
                    "body_html": "<p>isyncyou issue 624 live confirmation probe</p>",
                    "to": [recipient]
                }
            }),
        }]];
        let agent = DaemonAgent::with_test_provider_script_and_confirm_components(
            cfg,
            root.clone(),
            script,
            executor,
            audit,
        );

        let turn =
            isyncyou_webui::AgentHandler::start_turn(&agent, "me", "create a controlled draft")
                .unwrap();
        let rx = isyncyou_webui::AgentHandler::open_stream(&agent, &turn).expect("turn stream");
        let mut pending_id = String::new();
        let mut confirm_token = String::new();
        let mut action_hash = String::new();
        let mut saw_redacted_tool_call = false;
        for _ in 0..8 {
            let line = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("agent stream event");
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            match event["event"].as_str() {
                Some("tool_call") => {
                    saw_redacted_tool_call = true;
                    let text = event["input"].to_string();
                    assert!(event["input"]["redacted"].as_bool().unwrap_or(false));
                    assert!(!text.contains(&recipient_for_assert));
                    assert!(!text.contains("live confirmation probe"));
                }
                Some("confirmation_required") => {
                    pending_id = event["pending_id"].as_str().unwrap().to_string();
                    confirm_token = event["token"].as_str().unwrap().to_string();
                    action_hash = event["action_hash"].as_str().unwrap().to_string();
                    break;
                }
                other => panic!("unexpected event before confirmation: {other:?} {line}"),
            }
        }
        assert!(saw_redacted_tool_call);
        assert!(!pending_id.is_empty());
        assert!(!confirm_token.is_empty());
        assert!(!action_hash.is_empty());

        let mut cleanup = Issue624DraftCleanup {
            token: access_token,
            draft_id: None,
        };
        let confirm = isyncyou_webui::AgentHandler::confirm(
            &agent,
            &pending_id,
            &confirm_token,
            &action_hash,
        )
        .unwrap();
        let body: serde_json::Value = serde_json::from_str(&confirm).unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["op"], "live-write");
        let summary: serde_json::Value =
            serde_json::from_str(body["summary"].as_str().unwrap()).unwrap();
        assert_eq!(summary["op"], "live-write");
        assert_eq!(summary["service"], "mail");
        assert_eq!(summary["verb"], "create_draft");
        let draft_id = summary["result_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(String::from)
            .expect("confirmed create_draft returns a draft id");
        cleanup.set(draft_id);
        let _ = std::fs::remove_dir_all(root);
    }

    struct Issue624DriveCleanup {
        token: String,
        item_id: Option<String>,
        permission_ids: Vec<String>,
    }

    impl Issue624DriveCleanup {
        fn set_item(&mut self, item_id: String) {
            self.item_id = Some(item_id);
        }

        fn add_permission(&mut self, permission_id: String) {
            self.permission_ids.push(permission_id);
        }
    }

    impl Drop for Issue624DriveCleanup {
        fn drop(&mut self) {
            let graph = isyncyou_graph::GraphClient::new(self.token.clone());
            if let Some(item_id) = self.item_id.as_deref() {
                for permission_id in self.permission_ids.drain(..) {
                    let _ = graph.delete_permission(item_id, &permission_id);
                }
                let _ = graph.delete_item(item_id);
            }
        }
    }

    #[test]
    #[ignore = "requires a live Microsoft 365 write token with Files.ReadWrite via ISY624_M365_WRITE_TOKEN_FILE or ISY624_M365_WRITE_TOKEN"]
    fn live_issue_624_agent_confirm_share_link_and_cleanup() {
        let access_token = issue_624_live_write_token()
            .expect("set ISY624_M365_WRITE_TOKEN_FILE or ISY624_M365_WRITE_TOKEN");
        let root = temp_agent_root("live-issue-624-confirm-share");
        let cfg = issue_624_live_config(&root, &access_token);
        let graph = isyncyou_graph::GraphClient::new(access_token.clone());
        let name = format!("isyncyou-issue-624-live-{}.txt", unix_now());
        let uploaded = graph
            .upload_content_with_conflict_behavior(
                &name,
                b"isyncyou issue 624 controlled share probe",
                isyncyou_graph::http::ConflictBehavior::Fail,
            )
            .unwrap()
            .expect("temporary OneDrive fixture must not already exist");
        let item_id = uploaded["id"].as_str().unwrap().to_string();
        let mut cleanup = Issue624DriveCleanup {
            token: access_token,
            item_id: None,
            permission_ids: Vec::new(),
        };
        cleanup.set_item(item_id.clone());

        let gate = Arc::new(Mutex::new(()));
        let executor = agent_ops::confirmed_executor_for_policy(
            AgentOperationPolicy::DesktopEnabled,
            cfg.clone(),
            gate,
        );
        let audit = Arc::new(StoreAgentAuditSink { cfg: cfg.clone() });
        let script = vec![vec![isyncyou_agent::AssistantBlock::ToolUse {
            id: "tool-1".into(),
            input: serde_json::json!({
                "op":"share",
                "account":"me",
                "service":"onedrive",
                "id": item_id,
                "mode":"link",
                "link_type":"view",
                "scope":"anonymous"
            }),
        }]];
        let agent = DaemonAgent::with_test_provider_script_and_confirm_components(
            cfg,
            root.clone(),
            script,
            executor,
            audit,
        );

        let turn =
            isyncyou_webui::AgentHandler::start_turn(&agent, "me", "share temporary file").unwrap();
        let rx = isyncyou_webui::AgentHandler::open_stream(&agent, &turn).expect("turn stream");
        let mut pending_id = String::new();
        let mut confirm_token = String::new();
        let mut action_hash = String::new();
        for _ in 0..8 {
            let line = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("agent stream event");
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            match event["event"].as_str() {
                Some("tool_call") => {
                    assert!(event["input"]["redacted"].as_bool().unwrap_or(false));
                    assert_eq!(event["input"]["mode"], "link");
                    assert_eq!(event["input"]["link_type"], "view");
                    assert_eq!(event["input"]["scope"], "anonymous");
                }
                Some("confirmation_required") => {
                    pending_id = event["pending_id"].as_str().unwrap().to_string();
                    confirm_token = event["token"].as_str().unwrap().to_string();
                    action_hash = event["action_hash"].as_str().unwrap().to_string();
                    break;
                }
                other => panic!("unexpected event before confirmation: {other:?} {line}"),
            }
        }
        assert!(!pending_id.is_empty());
        let confirm = isyncyou_webui::AgentHandler::confirm(
            &agent,
            &pending_id,
            &confirm_token,
            &action_hash,
        )
        .unwrap();
        let body: serde_json::Value = serde_json::from_str(&confirm).unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["op"], "share");
        let summary: serde_json::Value =
            serde_json::from_str(body["summary"].as_str().unwrap()).unwrap();
        assert_eq!(summary["op"], "share");
        assert_eq!(summary["mode"], "link");
        assert_eq!(summary["link_type"], "view");
        assert_eq!(summary["scope"], "anonymous");

        let permissions = graph
            .list_permissions_detailed(cleanup.item_id.as_ref().unwrap())
            .unwrap();
        let created = permissions
            .iter()
            .find(|permission| {
                permission.link_type.as_deref() == Some("view")
                    && permission.link_scope.as_deref() == Some("anonymous")
                    && !permission.inherited
            })
            .expect("confirmed share creates a direct anonymous view link");
        cleanup.add_permission(created.id.clone());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "requires a live Microsoft 365 token with Mail.Read via ISY624_M365_WRITE_TOKEN_FILE or ISY624_M365_WRITE_TOKEN"]
    fn live_issue_624_agent_confirm_backup_mail_records_run() {
        let access_token = issue_624_live_write_token()
            .expect("set ISY624_M365_WRITE_TOKEN_FILE or ISY624_M365_WRITE_TOKEN");
        let root = temp_agent_root("live-issue-624-confirm-backup");
        let cfg = issue_624_live_config(&root, &access_token);
        let gate = Arc::new(Mutex::new(()));
        let executor = agent_ops::confirmed_executor_for_policy(
            AgentOperationPolicy::DesktopEnabled,
            cfg.clone(),
            gate,
        );
        let audit = Arc::new(StoreAgentAuditSink { cfg: cfg.clone() });
        let script = vec![vec![isyncyou_agent::AssistantBlock::ToolUse {
            id: "tool-1".into(),
            input: serde_json::json!({
                "op":"backup",
                "account":"me",
                "services":["mail"]
            }),
        }]];
        let agent = DaemonAgent::with_test_provider_script_and_confirm_components(
            cfg.clone(),
            root.clone(),
            script,
            executor,
            audit,
        );

        let (pending_id, confirm_token, action_hash) =
            issue_624_collect_pending(&agent, "run a controlled mail backup", "backup");
        let confirm = isyncyou_webui::AgentHandler::confirm(
            &agent,
            &pending_id,
            &confirm_token,
            &action_hash,
        )
        .unwrap();
        let body: serde_json::Value = serde_json::from_str(&confirm).unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["op"], "backup");
        let summary = body["summary"].as_str().unwrap();
        assert!(summary.contains("mail:"));
        assert!(!summary.contains("Bearer "));
        assert!(!summary.contains("access_token"));

        let store =
            isyncyou_store::Store::open(cfg.accounts[0].archive_root.join(".isyncyou-store.db"))
                .unwrap();
        let runs = store.recent_runs("me", 3).unwrap();
        let latest = runs
            .iter()
            .find(|run| run.kind == "backup")
            .expect("confirmed backup records a backup run");
        assert_eq!(latest.status, "ok");
        assert!(latest.summary.contains("mail:"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "requires a live Microsoft 365 token with Mail.ReadWrite via ISY624_M365_WRITE_TOKEN_FILE or ISY624_M365_WRITE_TOKEN"]
    fn live_issue_624_agent_confirm_restore_cloud_mail_and_cleanup() {
        let access_token = issue_624_live_write_token()
            .expect("set ISY624_M365_WRITE_TOKEN_FILE or ISY624_M365_WRITE_TOKEN");
        let root = temp_agent_root("live-issue-624-confirm-restore-cloud");
        let mut cfg = issue_624_live_config(&root, &access_token);
        cfg.restore.cloud_restore_enabled = true;
        let archive = cfg.accounts[0].archive_root.clone();
        std::fs::create_dir_all(archive.join("mail/live-restore")).unwrap();
        let source_id = format!("issue-624-restore-{}", unix_now());
        let rel = format!("mail/live-restore/{source_id}.eml");
        let subject = format!("isyncyou issue 624 restore cloud {}", unix_now());
        let mime = format!(
            "From: iSyncYou Probe <isyncyou-probe@example.invalid>\r\n\
             To: iSyncYou Probe <isyncyou-probe@example.invalid>\r\n\
             Subject: {subject}\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             isyncyou issue 624 restore-cloud controlled probe\r\n"
        );
        {
            let store = isyncyou_store::Store::open(archive.join(".isyncyou-store.db")).unwrap();
            let mut item = isyncyou_store::Item::new(
                "me",
                "mail",
                &source_id,
                "Issue 624 restore-cloud probe.eml",
                "message",
            );
            item.local_path = Some(rel.clone());
            store.upsert_item(&item).unwrap();
        }
        isyncyou_core::envelope::write_body_atomic(&archive.join(&rel), mime.as_bytes()).unwrap();

        let gate = Arc::new(Mutex::new(()));
        let executor = agent_ops::confirmed_executor_for_policy(
            AgentOperationPolicy::DesktopEnabled,
            cfg.clone(),
            gate,
        );
        let audit = Arc::new(StoreAgentAuditSink { cfg: cfg.clone() });
        let script = vec![vec![isyncyou_agent::AssistantBlock::ToolUse {
            id: "tool-1".into(),
            input: serde_json::json!({
                "op":"restore-cloud",
                "account":"me",
                "service":"mail",
                "id": source_id
            }),
        }]];
        let agent = DaemonAgent::with_test_provider_script_and_confirm_components(
            cfg.clone(),
            root.clone(),
            script,
            executor,
            audit,
        );

        let (pending_id, confirm_token, action_hash) =
            issue_624_collect_pending(&agent, "restore controlled archived mail", "restore-cloud");
        let confirm = isyncyou_webui::AgentHandler::confirm(
            &agent,
            &pending_id,
            &confirm_token,
            &action_hash,
        )
        .unwrap();
        let body: serde_json::Value = serde_json::from_str(&confirm).unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["op"], "restore-cloud");
        let summary: serde_json::Value =
            serde_json::from_str(body["summary"].as_str().unwrap()).unwrap();
        assert_eq!(summary["op"], "restore-cloud");
        assert_eq!(summary["service"], "mail");
        assert_eq!(summary["source_id"], source_id);
        let new_id = summary["new_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .expect("restore-cloud returns new message id")
            .to_string();
        let mut cleanup = Issue624DraftCleanup {
            token: access_token,
            draft_id: None,
        };
        cleanup.set(new_id.clone());

        let graph = isyncyou_graph::GraphClient::new(cleanup.token.clone());
        let restored = graph
            .get_json(&format!(
                "/me/messages/{}?$select=id,subject",
                issue_624_url_segment(&new_id)
            ))
            .unwrap();
        assert_eq!(restored["id"].as_str(), Some(new_id.as_str()));
        assert_eq!(restored["subject"].as_str(), Some(subject.as_str()));

        let bytes = isyncyou_core::envelope::read_body(&archive.join(&rel)).unwrap();
        let secret =
            isyncyou_engine::load_or_create_secret(&archive.join(".isyncyou-restore-secret"))
                .unwrap();
        let key = isyncyou_engine::idempotency_key(&secret, "me", "mail", &source_id, &bytes);
        let op_id = format!("me:{key}");
        let store = isyncyou_store::Store::open(archive.join(".isyncyou-store.db")).unwrap();
        let op = store
            .get_restore_operation(&op_id)
            .unwrap()
            .expect("restore-cloud records a ledger operation");
        assert_eq!(op.state, isyncyou_store::RestoreState::Committed);
        assert_eq!(op.new_cloud_id.as_deref(), Some(new_id.as_str()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn daemon_agent_pending_turn_stream_closes_with_pending_confirmation_done() {
        let script = vec![vec![isyncyou_agent::AssistantBlock::ToolUse {
            id: "tool-1".into(),
            input: serde_json::json!({"op":"backup","account":"me","services":["mail"]}),
        }]];
        let order = Arc::new(StdMutex::new(Vec::new()));
        let executor = RecordingConfirmedExecutor::ok("backup accepted", order.clone());
        let audit = RecordingAuditSink::new(order);
        let root = temp_agent_root("fake-pending-done");
        let agent = DaemonAgent::with_test_provider_script_and_confirm_components(
            Config::default(),
            root.clone(),
            script,
            Arc::new(executor),
            Arc::new(audit),
        );

        let turn = isyncyou_webui::AgentHandler::start_turn(&agent, "me", "back up mail").unwrap();
        let rx = isyncyou_webui::AgentHandler::open_stream(&agent, &turn).expect("turn stream");
        let mut saw_confirmation = false;
        let mut done_reason = String::new();
        for _ in 0..8 {
            let line = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("agent stream event");
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            match event["event"].as_str() {
                Some("confirmation_required") => saw_confirmation = true,
                Some("done") => {
                    done_reason = event["reason"].as_str().unwrap().to_string();
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_confirmation);
        assert_eq!(done_reason, "pending_confirmation");
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn daemon_agent_confirm_runs_executor_once_and_replay_fails() {
        let script = vec![vec![isyncyou_agent::AssistantBlock::ToolUse {
            id: "tool-1".into(),
            input: serde_json::json!({"op":"backup","account":"me","services":["mail"]}),
        }]];
        let order = Arc::new(StdMutex::new(Vec::new()));
        let executor = RecordingConfirmedExecutor::ok("backup accepted", order.clone());
        let audit = RecordingAuditSink::new(order.clone());
        let root = temp_agent_root("fake-confirm-once");
        let agent = DaemonAgent::with_test_provider_script_and_confirm_components(
            Config::default(),
            root.clone(),
            script,
            Arc::new(executor.clone()),
            Arc::new(audit.clone()),
        );

        let turn = isyncyou_webui::AgentHandler::start_turn(&agent, "me", "back up mail").unwrap();
        let rx = isyncyou_webui::AgentHandler::open_stream(&agent, &turn).expect("turn stream");
        let mut pending_id = String::new();
        let mut token = String::new();
        let mut action_hash = String::new();
        for _ in 0..8 {
            let line = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("agent stream event");
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            if event["event"] == "confirmation_required" {
                pending_id = event["pending_id"].as_str().unwrap().to_string();
                token = event["token"].as_str().unwrap().to_string();
                action_hash = event["action_hash"].as_str().unwrap().to_string();
                break;
            }
        }
        assert!(!pending_id.is_empty());
        let result =
            isyncyou_webui::AgentHandler::confirm(&agent, &pending_id, &token, &action_hash)
                .unwrap();
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["summary"], "backup accepted");
        let replay =
            isyncyou_webui::AgentHandler::confirm(&agent, &pending_id, &token, &action_hash)
                .unwrap_err();
        assert!(replay.contains("NotFound"));
        assert_eq!(executor.call_count(), 1);
        assert_eq!(
            order.lock().unwrap().as_slice(),
            ["audit:started", "execute", "audit:ok"]
        );
        assert_eq!(audit.events().len(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn daemon_agent_cancel_ends_stream_with_cancelled_done() {
        let order = Arc::new(StdMutex::new(Vec::new()));
        let executor = RecordingConfirmedExecutor::ok("unused", order.clone());
        let audit = RecordingAuditSink::new(order);
        let root = temp_agent_root("cancel-done");
        let agent = DaemonAgent::with_test_confirm_components(
            Config::default(),
            root.clone(),
            Arc::new(executor),
            Arc::new(audit),
        );
        let turn = "turn-cancel";
        let rx_events = agent.hub.open(turn, 8);
        let (tx_str, rx_str) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            while let Ok(ev) = rx_events.recv() {
                if tx_str.send(agent_event_json(&ev)).is_err() {
                    break;
                }
            }
        });
        agent.streams.lock().unwrap().insert(
            turn.to_string(),
            AgentStreamSlot {
                rx: rx_str,
                created_at_ms: unix_now_ms(),
            },
        );

        isyncyou_webui::AgentHandler::cancel(&agent, turn);
        let rx = isyncyou_webui::AgentHandler::open_stream(&agent, turn).expect("turn stream");
        let line = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("cancelled done event");
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(event["event"], "done");
        assert_eq!(event["reason"], "cancelled");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn daemon_agent_unopened_stream_is_swept() {
        let script = vec![vec![isyncyou_agent::AssistantBlock::Text(
            "hello world".into(),
        )]];
        let order = Arc::new(StdMutex::new(Vec::new()));
        let executor = RecordingConfirmedExecutor::ok("unused", order.clone());
        let audit = RecordingAuditSink::new(order);
        let root = temp_agent_root("unopened-sweep");
        let agent = DaemonAgent::with_test_provider_script_and_confirm_components(
            Config::default(),
            root.clone(),
            script,
            Arc::new(executor),
            Arc::new(audit),
        );
        let _turn = isyncyou_webui::AgentHandler::start_turn(&agent, "me", "hello").unwrap();
        assert_eq!(agent.unopened_stream_count_for_tests(), 1);
        assert_eq!(
            agent.sweep_unopened_streams_for_tests(
                unix_now_ms().saturating_add(AGENT_STREAM_UNOPENED_TTL_MS + 1)
            ),
            1
        );
        assert_eq!(agent.unopened_stream_count_for_tests(), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn agent_stream_token_events_contain_no_confirmation_token() {
        let script = vec![vec![
            isyncyou_agent::AssistantBlock::Text("Preparing backup ".into()),
            isyncyou_agent::AssistantBlock::ToolUse {
                id: "tool-1".into(),
                input: serde_json::json!({"op":"backup","account":"me","services":["mail"]}),
            },
        ]];
        let order = Arc::new(StdMutex::new(Vec::new()));
        let executor = RecordingConfirmedExecutor::ok("backup accepted", order.clone());
        let audit = RecordingAuditSink::new(order);
        let root = temp_agent_root("stream-token-redaction");
        let agent = DaemonAgent::with_test_provider_script_and_confirm_components(
            Config::default(),
            root.clone(),
            script,
            Arc::new(executor),
            Arc::new(audit),
        );

        let turn = isyncyou_webui::AgentHandler::start_turn(&agent, "me", "back up mail").unwrap();
        let rx = isyncyou_webui::AgentHandler::open_stream(&agent, &turn).expect("turn stream");
        let mut token_text = String::new();
        let mut confirmation_token = String::new();
        for _ in 0..10 {
            let line = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("agent stream event");
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            match event["event"].as_str() {
                Some("token") => token_text.push_str(event["text"].as_str().unwrap()),
                Some("confirmation_required") => {
                    confirmation_token = event["token"].as_str().unwrap().to_string();
                }
                Some("done") => break,
                _ => {}
            }
        }
        assert!(!confirmation_token.is_empty());
        assert!(token_text.contains("Preparing backup"));
        assert!(
            !token_text.contains(&confirmation_token),
            "token stream leaked confirmation token"
        );
        assert!(!token_text.contains("confirmation_required"));
        assert!(!token_text.contains("action_hash"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn agent_error_and_audit_redact_confirmation_token() {
        let order = Arc::new(StdMutex::new(Vec::new()));
        let executor = RecordingConfirmedExecutor::err("placeholder", order.clone());
        let audit = RecordingAuditSink::new(order);
        let root = temp_agent_root("confirm-redact-token");
        let agent = DaemonAgent::with_test_confirm_components(
            Config::default(),
            root.clone(),
            Arc::new(executor.clone()),
            Arc::new(audit.clone()),
        );
        let (pending, token) = agent
            .pending
            .register(
                backup_action(),
                "backup mail",
                unix_now_ms(),
                AGENT_CONFIRM_TTL_MS,
            )
            .unwrap();
        executor.set_error(format!(
            "raw executor failure includes confirmation token {token}"
        ));

        let err = isyncyou_webui::AgentHandler::confirm(
            &agent,
            &pending.id,
            &token,
            &pending.action_hash,
        )
        .unwrap_err();
        assert_eq!(err, "backup failed: execution_failed");
        assert!(!err.contains(&token));
        let audit_text = serde_json::to_string(&audit.events()).unwrap();
        assert!(!audit_text.contains(&token));
        assert!(!audit_text.contains("raw executor failure"));
        assert!(audit_text.contains("execution_failed"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn daemon_agent_pending_turn_outcome_is_registered_and_confirmable() {
        let script = vec![vec![isyncyou_agent::AssistantBlock::ToolUse {
            id: "tool-1".into(),
            input: serde_json::json!({"op":"backup","account":"me","services":["mail"]}),
        }]];
        let root = std::env::temp_dir().join(format!(
            "isy-apphost-agent-pending-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let order = Arc::new(StdMutex::new(Vec::new()));
        let executor = RecordingConfirmedExecutor::ok("backup accepted", order);
        let audit = RecordingAuditSink::new(Arc::new(StdMutex::new(Vec::new())));
        let agent = DaemonAgent::with_test_provider_script_and_confirm_components(
            Config::default(),
            root.clone(),
            script,
            Arc::new(executor),
            Arc::new(audit),
        );

        let turn = isyncyou_webui::AgentHandler::start_turn(&agent, "me", "back up mail").unwrap();
        let rx = isyncyou_webui::AgentHandler::open_stream(&agent, &turn).expect("turn stream");

        let mut pending_id = String::new();
        let mut token = String::new();
        let mut action_hash = String::new();
        let mut saw_pending_done = false;
        for _ in 0..8 {
            let line = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("agent stream event");
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            match event["event"].as_str() {
                Some("confirmation_required") => {
                    pending_id = event["pending_id"].as_str().unwrap().to_string();
                    token = event["token"].as_str().unwrap().to_string();
                    action_hash = event["action_hash"].as_str().unwrap().to_string();
                    assert_ne!(pending_id, "tool-1", "pending id must be registry-owned");
                    assert_eq!(action_hash.len(), 64);
                    assert_eq!(event["risk"], "destructive");
                    assert!(event["expires_at_ms"].as_u64().unwrap() > 0);
                    assert!(event["preview"].as_str().unwrap().contains("backup"));
                }
                Some("done") => {
                    assert_eq!(event["reason"], "pending_confirmation");
                    saw_pending_done = true;
                    break;
                }
                Some("tool_call") => {
                    assert_eq!(event["id"], "tool-1");
                    assert_eq!(event["name"], "isyncyou");
                }
                other => panic!("unexpected event before pending confirmation: {other:?} {line}"),
            }
        }

        assert!(
            saw_pending_done,
            "pending turn must close with pending_confirmation done"
        );
        assert!(
            !pending_id.is_empty(),
            "registered pending id should be streamed"
        );
        let result =
            isyncyou_webui::AgentHandler::confirm(&agent, &pending_id, &token, &action_hash)
                .unwrap();
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["status"], "ok");
        assert_eq!(result["op"], "backup");
        let replay =
            isyncyou_webui::AgentHandler::confirm(&agent, &pending_id, &token, &action_hash)
                .unwrap_err();
        assert!(replay.contains("NotFound"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn live_provider_agent_executor_reads_store_archive_fixture() {
        let _guard = EnvelopeRequirementGuard::new();
        isyncyou_core::envelope::set_body_key(618_070, [70u8; 32]);

        let root = std::env::temp_dir().join(format!(
            "isy-apphost-agent-retrieval-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("mail/aa")).unwrap();

        let store = isyncyou_store::Store::open(root.join(".isyncyou-store.db")).unwrap();
        let mut item =
            isyncyou_store::Item::new("me", "mail", "m-runtime", "Runtime fixture mail", "message");
        item.local_path = Some("mail/aa/m-runtime.eml".into());
        store.upsert_item(&item).unwrap();
        store
            .index_body("me", "mail", "m-runtime", "Runtime body indexed text")
            .unwrap();
        drop(store);

        isyncyou_core::envelope::write_body_atomic(
            &root.join("mail/aa/m-runtime.eml"),
            b"Runtime body archived text",
        )
        .unwrap();

        let exec = make_executor("me", root.clone());

        let search = isyncyou_agent::ToolAction::Search {
            account: "me".into(),
            services: vec!["mail".into()],
            query: "Runtime".into(),
            limit: Some(5),
        };
        let search_out: serde_json::Value =
            serde_json::from_str(&exec.execute_read(&search).unwrap()).unwrap();
        assert_eq!(search_out["results"][0]["id"], "m-runtime");
        assert_eq!(search_out["results"][0]["service"], "mail");

        let read = isyncyou_agent::ToolAction::Read {
            account: "me".into(),
            service: "mail".into(),
            id: "m-runtime".into(),
            max_bytes: None,
        };
        let read_out: serde_json::Value =
            serde_json::from_str(&exec.execute_read(&read).unwrap()).unwrap();
        assert_eq!(read_out["source"]["id"], "m-runtime");
        assert!(read_out["content"]
            .as_str()
            .unwrap()
            .contains("Runtime body archived text"));

        let list = isyncyou_agent::ToolAction::List {
            account: "me".into(),
            service: "mail".into(),
            parent: None,
            limit: Some(10),
            offset: Some(0),
        };
        let list_out: serde_json::Value =
            serde_json::from_str(&exec.execute_read(&list).unwrap()).unwrap();
        assert_eq!(list_out["service_total"], 1);
        assert_eq!(list_out["results"][0]["id"], "m-runtime");

        let export = isyncyou_agent::ToolAction::Export {
            account: "me".into(),
            service: "mail".into(),
            id: "m-runtime".into(),
        };
        let export_out: serde_json::Value =
            serde_json::from_str(&exec.execute_read(&export).unwrap()).unwrap();
        assert_eq!(export_out["format"], "raw");
        assert_eq!(export_out["source"]["path"], "mail/aa/m-runtime.eml");
        assert!(export_out["content"]
            .as_str()
            .unwrap()
            .contains("Runtime body archived text"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn seed_restore_local_fixture(
        name: &str,
        service: &str,
        id: &str,
        item_name: &str,
        rel: Option<&str>,
        body: &[u8],
    ) -> PathBuf {
        let root = temp_agent_root(name);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let store = isyncyou_store::Store::open(root.join(".isyncyou-store.db")).unwrap();
        let mut item = isyncyou_store::Item::new("me", service, id, item_name, "message");
        if let Some(rel) = rel {
            item.local_path = Some(rel.into());
        }
        store.upsert_item(&item).unwrap();
        drop(store);

        if let Some(rel) = rel {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            isyncyou_core::envelope::write_body_atomic(&path, body).unwrap();
        }
        root
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn restore_local_action(service: &str, id: &str) -> isyncyou_agent::ToolAction {
        isyncyou_agent::ToolAction::RestoreLocal {
            account: "me".into(),
            service: service.into(),
            id: id.into(),
        }
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    fn restored_path(out: &serde_json::Value) -> PathBuf {
        PathBuf::from(out["path"].as_str().unwrap())
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn restore_local_writes_archived_body_to_controlled_restore_root() {
        let _guard = EnvelopeRequirementGuard::new();
        isyncyou_core::envelope::set_body_key(624_301, [31u8; 32]);
        let root = seed_restore_local_fixture(
            "restore-local-write",
            "mail",
            "m1",
            "Archived message.eml",
            Some("mail/aa/m1.eml"),
            b"restore-local plaintext",
        );
        let exec = make_executor("me", root.clone());

        let out: serde_json::Value = serde_json::from_str(
            &exec
                .execute_read(&restore_local_action("mail", "m1"))
                .unwrap(),
        )
        .unwrap();
        let path = restored_path(&out);

        assert!(path.starts_with(root.join(".isyncyou-agent/restore-local/mail")));
        assert_eq!(std::fs::read(&path).unwrap(), b"restore-local plaintext");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn restore_local_output_path_is_not_model_controlled() {
        let _guard = EnvelopeRequirementGuard::new();
        isyncyou_core::envelope::set_body_key(624_302, [32u8; 32]);
        let root = seed_restore_local_fixture(
            "restore-local-path",
            "mail",
            "m-escape",
            "../../escape.txt",
            Some("mail/aa/m-escape.eml"),
            b"safe output",
        );
        let exec = make_executor("me", root.clone());

        let out: serde_json::Value = serde_json::from_str(
            &exec
                .execute_read(&restore_local_action("mail", "m-escape"))
                .unwrap(),
        )
        .unwrap();
        let path = restored_path(&out);

        assert!(path.starts_with(root.join(".isyncyou-agent/restore-local/mail")));
        assert!(!path.to_string_lossy().contains("../"));
        assert_eq!(std::fs::read(&path).unwrap(), b"safe output");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn restore_local_rejects_missing_archived_body() {
        let _guard = EnvelopeRequirementGuard::new();
        let root = seed_restore_local_fixture(
            "restore-local-missing-body",
            "mail",
            "m-missing",
            "Missing body",
            None,
            b"",
        );
        let exec = make_executor("me", root.clone());

        let err = exec
            .execute_read(&restore_local_action("mail", "m-missing"))
            .unwrap_err();

        assert!(err.to_string().contains("has no archived body"));
        assert!(!root.join(".isyncyou-agent/restore-local").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn restore_local_uses_envelope_reader_for_sealed_body() {
        let _guard = EnvelopeRequirementGuard::new();
        isyncyou_core::envelope::set_body_key(624_303, [33u8; 32]);
        let root = seed_restore_local_fixture(
            "restore-local-sealed",
            "mail",
            "m-sealed",
            "Sealed message.eml",
            Some("mail/aa/m-sealed.eml"),
            b"sealed restore bytes",
        );
        let raw_archive = std::fs::read(root.join("mail/aa/m-sealed.eml")).unwrap();
        assert_ne!(raw_archive, b"sealed restore bytes");
        assert!(raw_archive.starts_with(b"ISYE"));
        let exec = make_executor("me", root.clone());

        let out: serde_json::Value = serde_json::from_str(
            &exec
                .execute_read(&restore_local_action("mail", "m-sealed"))
                .unwrap(),
        )
        .unwrap();
        let path = restored_path(&out);

        assert_eq!(std::fs::read(&path).unwrap(), b"sealed restore bytes");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(all(
        unix,
        any(
            feature = "agent-oauth-providers",
            feature = "agent-subscription-experimental"
        )
    ))]
    #[test]
    fn restore_local_file_is_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = EnvelopeRequirementGuard::new();
        isyncyou_core::envelope::set_body_key(624_304, [34u8; 32]);
        let root = seed_restore_local_fixture(
            "restore-local-perms",
            "mail",
            "m-perms",
            "Perms.eml",
            Some("mail/aa/m-perms.eml"),
            b"permission bytes",
        );
        let exec = make_executor("me", root.clone());

        let out: serde_json::Value = serde_json::from_str(
            &exec
                .execute_read(&restore_local_action("mail", "m-perms"))
                .unwrap(),
        )
        .unwrap();
        let path = restored_path(&out);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;

        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn restore_local_response_carries_source_id_and_byte_count() {
        let _guard = EnvelopeRequirementGuard::new();
        isyncyou_core::envelope::set_body_key(624_305, [35u8; 32]);
        let root = seed_restore_local_fixture(
            "restore-local-response",
            "mail",
            "m-response",
            "Response.eml",
            Some("mail/aa/m-response.eml"),
            b"response bytes",
        );
        let exec = make_executor("me", root.clone());

        let out: serde_json::Value = serde_json::from_str(
            &exec
                .execute_read(&restore_local_action("mail", "m-response"))
                .unwrap(),
        )
        .unwrap();

        assert_eq!(out["service"], "mail");
        assert_eq!(out["id"], "m-response");
        assert_eq!(out["bytes"], b"response bytes".len());
        assert_eq!(out["source"]["service"], "mail");
        assert_eq!(out["source"]["id"], "m-response");
        assert_eq!(out["source"]["path"], "mail/aa/m-response.eml");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(feature = "agent-oauth-providers")]
    fn product_live_seed_env(name: &str) -> String {
        match std::env::var(name) {
            Ok(value) if !value.trim().is_empty() => value,
            _ => panic!("{name} is required for the ignored #623 product OAuth live gate"),
        }
    }

    #[cfg(feature = "agent-oauth-providers")]
    fn product_live_expires_at_ms(name: &str) -> u64 {
        match std::env::var(name) {
            Ok(value) if !value.trim().is_empty() => value
                .parse::<u64>()
                .unwrap_or_else(|_| panic!("{name} must be a millisecond Unix timestamp")),
            _ => now_ms() + 3_600_000,
        }
    }

    #[cfg(feature = "agent-oauth-providers")]
    fn seed_product_live_storearchive_fixture(name: &str) -> (PathBuf, Config) {
        let root = temp_agent_root(name);
        let archive = root.join("archive");
        let sync = root.join("sync");
        let cache = root.join("cache");
        std::fs::create_dir_all(archive.join("mail/live")).unwrap();
        std::fs::create_dir_all(&sync).unwrap();
        std::fs::create_dir_all(&cache).unwrap();

        let store = isyncyou_store::Store::open(archive.join(".isyncyou-store.db")).unwrap();
        let mut item = isyncyou_store::Item::new(
            "me",
            "mail",
            "m-live",
            "Issue 623 product live fixture",
            "message",
        );
        item.local_path = Some("mail/live/m-live.eml".into());
        store.upsert_item(&item).unwrap();
        store
            .index_body("me", "mail", "m-live", "isyncyou623productlivesentinel")
            .unwrap();
        drop(store);
        isyncyou_core::envelope::write_body_atomic(
            &archive.join("mail/live/m-live.eml"),
            b"Subject: Issue 623 product live fixture\r\n\r\nisyncyou623productlivesentinel",
        )
        .unwrap();

        (
            root,
            Config {
                accounts: vec![isyncyou_core::AccountConfig {
                    id: "me".into(),
                    username: "me".into(),
                    sync_root: sync,
                    archive_root: archive,
                    cache_root: cache,
                    mount_point: None,
                }],
                ..Default::default()
            },
        )
    }

    #[cfg(feature = "agent-oauth-providers")]
    fn assert_product_live_storearchive_roundtrip(agent: &DaemonAgent, provider: &str) {
        let prompt = concat!(
            "Use the isyncyou tool before answering. Search account me, service mail, ",
            "query \"isyncyou623productlivesentinel\", then read item id \"m-live\". ",
            "Answer exactly: m-live isyncyou623productlivesentinel"
        );
        let turn = isyncyou_webui::AgentHandler::start_turn(agent, "me", prompt).unwrap();
        let rx = isyncyou_webui::AgentHandler::open_stream(agent, &turn).expect("turn stream");
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        let mut saw_tool_call = false;
        let mut saw_tool_result = false;
        let mut done_reason = None::<String>;
        let mut final_text = String::new();
        let mut trace_events = Vec::new();

        for _ in 0..256 {
            let now = std::time::Instant::now();
            assert!(
                now < deadline,
                "{provider} product OAuth live turn timed out before done"
            );
            let line = rx
                .recv_timeout(deadline.saturating_duration_since(now))
                .expect("agent live stream event");
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            match event["event"].as_str() {
                Some("token") => {
                    final_text.push_str(event["text"].as_str().unwrap_or_default());
                }
                Some("tool_call") => {
                    saw_tool_call = true;
                    assert_eq!(event["name"].as_str(), Some(isyncyou_agent::TOOL_NAME));
                    trace_events.push(serde_json::json!({
                        "event": "tool_call",
                        "name": event["name"].as_str().unwrap_or_default(),
                        "input": event["input"],
                    }));
                }
                Some("tool_result") => {
                    let content = event["content"].as_str().unwrap_or_default();
                    if content.contains("m-live")
                        || content.contains("isyncyou623productlivesentinel")
                    {
                        saw_tool_result = true;
                    }
                    trace_events.push(serde_json::json!({
                        "event": "tool_result",
                        "contains_m_live": content.contains("m-live"),
                        "contains_fixture_sentinel": content.contains("isyncyou623productlivesentinel"),
                        "untrusted": event["untrusted"].as_bool().unwrap_or(false),
                    }));
                }
                Some("done") => {
                    done_reason = event["reason"].as_str().map(|s| s.to_string());
                    trace_events.push(serde_json::json!({
                        "event": "done",
                        "reason": event["reason"].as_str().unwrap_or_default(),
                    }));
                    break;
                }
                Some("error") => panic!(
                    "{provider} product OAuth live turn failed: {}",
                    event["message"].as_str().unwrap_or("unknown error")
                ),
                _ => {}
            }
        }

        assert!(
            saw_tool_call,
            "{provider} live gate must include a real provider tool_call"
        );
        assert!(
            saw_tool_result,
            "{provider} live gate must include a StoreArchive tool_result"
        );
        assert_eq!(done_reason.as_deref(), Some("complete"));
        assert!(
            final_text.contains("m-live") || final_text.contains("isyncyou623productlivesentinel"),
            "{provider} final answer must cite the seeded StoreArchive fixture: {final_text}"
        );

        let status: serde_json::Value =
            serde_json::from_str(&isyncyou_webui::AgentHandler::status_json(agent)).unwrap();
        assert_eq!(status["usage"]["provider"].as_str(), Some(provider));
        assert!(
            status["usage"]["model"].as_str().is_some(),
            "{provider} usage must include a model"
        );
        if let Ok(dir) = std::env::var("ISY623_LIVE_TRACE_DIR") {
            let dir = std::path::PathBuf::from(dir);
            std::fs::create_dir_all(&dir).unwrap();
            let trace = serde_json::json!({
                "artifact": format!("issue-623-{provider}-oauth-live-storearchive-trace"),
                "provider": provider,
                "source": "ignored product OAuth live gate with local credential seed persisted through encrypted CredentialStore",
                "events": trace_events,
                "assertions": {
                    "saw_provider_tool_call": saw_tool_call,
                    "saw_storearchive_tool_result": saw_tool_result,
                    "done_reason": done_reason,
                    "final_contains_m_live": final_text.contains("m-live"),
                    "final_contains_fixture_sentinel": final_text.contains("isyncyou623productlivesentinel")
                },
                "usage": {
                    "provider": status["usage"]["provider"],
                    "model": status["usage"]["model"],
                    "input_tokens_present": status["usage"]["input_tokens"].is_number(),
                    "output_tokens_present": status["usage"]["output_tokens"].is_number(),
                    "request_id_present": status["usage"]["request_id"].as_str().is_some()
                },
                "secrets_included": false
            });
            let path = dir.join(format!("{provider}-oauth-live-storearchive-trace.json"));
            std::fs::write(path, serde_json::to_vec_pretty(&trace).unwrap()).unwrap();
        }
    }

    #[cfg(feature = "agent-oauth-providers")]
    #[test]
    #[ignore = "requires explicit ISY623_CLAUDE_OAUTH_ACCESS product OAuth seed; local CLI auth must not be used"]
    fn live_claude_oauth_storearchive_tool_roundtrip() {
        let _env = AppHostCredentialEnvGuard::new();
        let _body_guard = EnvelopeRequirementGuard::new();
        isyncyou_core::envelope::set_body_key(623_120, [120u8; 32]);
        let (root, cfg) = seed_product_live_storearchive_fixture("live-claude-storearchive");
        let oauth_dir = root.join("oauth");
        let agent = DaemonAgent::new(cfg, oauth_dir.clone());
        agent
            .store_credential(&StoredCredential {
                access_token: product_live_seed_env("ISY623_CLAUDE_OAUTH_ACCESS"),
                refresh_token: std::env::var("ISY623_CLAUDE_OAUTH_REFRESH").unwrap_or_default(),
                expires_at_ms: product_live_expires_at_ms("ISY623_CLAUDE_OAUTH_EXPIRES_AT_MS"),
            })
            .unwrap();
        agent.set_agent_settings("claude", DEFAULT_MODEL).unwrap();

        assert_product_live_storearchive_roundtrip(&agent, "claude");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(feature = "agent-oauth-providers")]
    #[test]
    #[ignore = "requires explicit ISY623_CODEX_OAUTH_ACCESS and ISY623_CODEX_ACCOUNT_ID product OAuth seeds; local CLI auth must not be used"]
    fn live_codex_oauth_storearchive_tool_roundtrip() {
        let _env = AppHostCredentialEnvGuard::new();
        let _body_guard = EnvelopeRequirementGuard::new();
        isyncyou_core::envelope::set_body_key(623_120, [120u8; 32]);
        let (root, cfg) = seed_product_live_storearchive_fixture("live-codex-storearchive");
        let oauth_dir = root.join("oauth");
        let agent = DaemonAgent::new(cfg, oauth_dir.clone());
        store_codex_blob(
            &oauth_dir,
            &CodexStoredCredential {
                access_token: product_live_seed_env("ISY623_CODEX_OAUTH_ACCESS"),
                refresh_token: std::env::var("ISY623_CODEX_OAUTH_REFRESH").unwrap_or_default(),
                account_id: product_live_seed_env("ISY623_CODEX_ACCOUNT_ID"),
                expires_at_ms: product_live_expires_at_ms("ISY623_CODEX_OAUTH_EXPIRES_AT_MS"),
            },
        )
        .unwrap();
        agent.set_agent_settings("codex", "gpt-5.5").unwrap();

        assert_product_live_storearchive_roundtrip(&agent, "codex");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn subscription_credential_round_trips_through_agent_credential_store() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("subscription-credential");
        let _ = std::fs::remove_dir_all(&root);
        let cred = StoredCredential {
            access_token: "subscription-access-token".into(),
            refresh_token: "subscription-refresh-token".into(),
            expires_at_ms: 123_456,
        };

        store_agent_credential_blob(&root, SUBSCRIPTION_CREDENTIAL_ID, cred.to_json()).unwrap();
        let raw = load_agent_credential_blob(&root, SUBSCRIPTION_CREDENTIAL_ID)
            .unwrap()
            .expect("stored subscription credential");
        let got = StoredCredential::from_json(raw.expose()).unwrap();

        assert_eq!(got.access_token, "subscription-access-token");
        assert_eq!(got.refresh_token, "subscription-refresh-token");
        assert_eq!(got.expires_at_ms, 123_456);
        let store_dir = agent_credential_config(&root).store_dir().to_path_buf();
        let stored = std::fs::read_dir(store_dir)
            .unwrap()
            .map(|entry| std::fs::read(entry.unwrap().path()).unwrap())
            .collect::<Vec<_>>();
        assert!(
            stored.iter().all(|bytes| !bytes
                .windows(b"subscription-access-token".len())
                .any(|window| window == b"subscription-access-token")),
            "encrypted credential store must not expose subscription token plaintext"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn codex_credential_round_trips_through_agent_credential_store() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("codex-credential");
        let _ = std::fs::remove_dir_all(&root);
        let cred = CodexStoredCredential {
            access_token: "codex-access-token".into(),
            refresh_token: "codex-refresh-token".into(),
            account_id: "acct_123".into(),
            expires_at_ms: 654_321,
        };

        store_codex_blob(&root, &cred).unwrap();
        let raw = load_agent_credential_blob(&root, CODEX_CREDENTIAL_ID)
            .unwrap()
            .expect("stored codex credential");
        let got = CodexStoredCredential::from_json(raw.expose()).unwrap();

        assert_eq!(got.access_token, "codex-access-token");
        assert_eq!(got.refresh_token, "codex-refresh-token");
        assert_eq!(got.account_id, "acct_123");
        assert_eq!(got.expires_at_ms, 654_321);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn credential_store_preferred_over_desktop_cli_fallback() {
        let env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("credential-preference");
        let _ = std::fs::remove_dir_all(&root);
        let oauth_dir = root.join("oauth");
        let home = root.join("home");
        write_local_cli_fixture(
            &home.join(".claude/.credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"desktop-claude-token"}}"#,
        );
        write_local_cli_fixture(
            &home.join(".codex/auth.json"),
            r#"{"tokens":{"access_token":"desktop-codex-token","account_id":"desktop-account"}}"#,
        );
        env.use_home_fallbacks(&home);

        let agent = DaemonAgent::new(Config::default(), oauth_dir.clone());
        agent
            .store_credential(&StoredCredential {
                access_token: "stored-claude-token".into(),
                refresh_token: String::new(),
                expires_at_ms: now_ms() + 3_600_000,
            })
            .unwrap();
        store_codex_blob(
            &oauth_dir,
            &CodexStoredCredential {
                access_token: "stored-codex-token".into(),
                refresh_token: String::new(),
                account_id: "stored-account".into(),
                expires_at_ms: now_ms() + 3_600_000,
            },
        )
        .unwrap();

        let claude = agent.resolve_claude_credential().unwrap();
        let codex = agent.resolve_codex_credential().unwrap();
        assert!(matches!(
            claude,
            ResolvedProviderCredential::Claude {
                origin: ProviderCredentialOrigin::ProductCredentialStore,
                credential: StoredCredential { ref access_token, .. },
            } if access_token == "stored-claude-token"
        ));
        assert!(matches!(
            codex,
            ResolvedProviderCredential::Codex {
                origin: ProviderCredentialOrigin::ProductCredentialStore,
                credential: CodexStoredCredential {
                    ref access_token,
                    ref account_id,
                    ..
                },
            } if access_token == "stored-codex-token" && account_id == "stored-account"
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(feature = "agent-subscription-experimental")]
    #[test]
    fn experimental_local_cli_fallback_is_not_persisted_to_product_store() {
        let env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("local-fallback-not-persisted");
        let oauth_dir = root.join("oauth");
        let home = root.join("home");
        write_local_cli_fixture(
            &home.join(".claude/.credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"local-only-claude"}}"#,
        );
        env.use_home_fallbacks(&home);
        let agent = DaemonAgent::new(Config::default(), oauth_dir.clone());

        let resolved = agent.resolve_claude_credential().unwrap();

        assert!(matches!(
            &resolved,
            ResolvedProviderCredential::Claude {
                origin: ProviderCredentialOrigin::ExperimentalLocalCli,
                ..
            }
        ));
        assert!(
            load_agent_credential_blob(&oauth_dir, SUBSCRIPTION_CREDENTIAL_ID)
                .unwrap()
                .is_none(),
            "experimental fallback must remain in memory"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(feature = "agent-subscription-experimental")]
    #[test]
    fn product_credential_never_combines_with_local_cli_metadata() {
        let env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("credential-origin-is-atomic");
        let oauth_dir = root.join("oauth");
        let local_codex = root.join("local-codex");
        write_local_cli_fixture(
            &local_codex.join("auth.json"),
            r#"{"tokens":{"access_token":"local-token","account_id":"local-account"}}"#,
        );
        env.set_codex_home(&local_codex);
        let agent = DaemonAgent::new(Config::default(), oauth_dir.clone());
        store_codex_blob(
            &oauth_dir,
            &CodexStoredCredential {
                access_token: "product-token".into(),
                refresh_token: String::new(),
                account_id: "product-account".into(),
                expires_at_ms: now_ms() + 3_600_000,
            },
        )
        .unwrap();

        let resolved = agent.resolve_codex_credential().unwrap();

        assert!(matches!(
            resolved,
            ResolvedProviderCredential::Codex {
                origin: ProviderCredentialOrigin::ProductCredentialStore,
                credential: CodexStoredCredential {
                    ref access_token,
                    ref account_id,
                    ..
                },
            } if access_token == "product-token" && account_id == "product-account"
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(feature = "agent-subscription-experimental")]
    #[test]
    fn corrupt_product_credential_does_not_fall_through_to_local_cli() {
        let env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("corrupt-product-no-fallback");
        let oauth_dir = root.join("oauth");
        let local_claude = root.join("local-claude");
        write_local_cli_fixture(
            &local_claude.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"local-token"}}"#,
        );
        env.set_claude_config_dir(&local_claude);
        store_agent_credential_blob(
            &oauth_dir,
            SUBSCRIPTION_CREDENTIAL_ID,
            b"not-the-product-credential-schema".to_vec(),
        )
        .unwrap();
        let agent = DaemonAgent::new(Config::default(), oauth_dir);

        let error = agent.resolve_claude_credential().err();

        assert_eq!(
            error,
            Some(ProviderCredentialResolutionError::ProductReconnectRequired)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn corrupt_product_credential_streams_error_and_done_error() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("corrupt-product-turn-error");
        let oauth_dir = root.join("oauth");
        store_agent_credential_blob(
            &oauth_dir,
            SUBSCRIPTION_CREDENTIAL_ID,
            b"not-the-product-credential-schema".to_vec(),
        )
        .unwrap();
        let agent = DaemonAgent::new(Config::default(), oauth_dir);

        assert_product_credential_turn_fails_closed(&agent);

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(feature = "agent-subscription-experimental")]
    #[test]
    fn expired_product_credential_does_not_silently_switch_origin() {
        let env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("expired-product-no-fallback");
        let oauth_dir = root.join("oauth");
        let local_claude = root.join("local-claude");
        write_local_cli_fixture(
            &local_claude.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"local-token"}}"#,
        );
        env.set_claude_config_dir(&local_claude);
        let agent = DaemonAgent::new(Config::default(), oauth_dir);
        agent
            .store_credential(&StoredCredential {
                access_token: "expired-product-token".into(),
                refresh_token: String::new(),
                expires_at_ms: 1,
            })
            .unwrap();

        let error = agent.resolve_claude_credential().err();

        assert_eq!(
            error,
            Some(ProviderCredentialResolutionError::ProductReconnectRequired)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn expired_product_credential_streams_error_and_done_error() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("expired-product-turn-error");
        let oauth_dir = root.join("oauth");
        let agent = DaemonAgent::new(Config::default(), oauth_dir);
        agent
            .store_credential(&StoredCredential {
                access_token: "expired-product-token".into(),
                refresh_token: String::new(),
                expires_at_ms: 1,
            })
            .unwrap();

        assert_product_credential_turn_fails_closed(&agent);

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(feature = "agent-subscription-experimental")]
    #[test]
    fn expired_product_credential_refresh_preserves_product_origin() {
        let local_called = std::cell::Cell::new(false);
        let resolved = resolve_product_or_local(
            ProductCredentialState::PresentNeedsRefresh("expired"),
            |_| Ok("refreshed"),
            || {
                local_called.set(true);
                Ok(Some("local"))
            },
        )
        .unwrap()
        .unwrap();

        assert_eq!(resolved.0, ProviderCredentialOrigin::ProductCredentialStore);
        assert_eq!(resolved.1, "refreshed");
        assert!(
            !local_called.get(),
            "refresh must not consult local CLI state"
        );
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn unknown_product_expiry_is_not_treated_as_indefinitely_valid() {
        let credential = StoredCredential {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at_ms: 0,
        };

        assert!(matches!(
            classify_claude_product_credential_at(credential, 1_000),
            ProductCredentialState::PresentNeedsRefresh(_)
        ));
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn product_credential_refresh_uses_injected_clock_and_finite_expiry() {
        let refreshed = complete_claude_refresh(
            StoredCredential {
                access_token: "old-access".into(),
                refresh_token: "old-refresh".into(),
                expires_at_ms: 1,
            },
            isyncyou_agent::oauth::RefreshedToken {
                access_token: "new-access".into(),
                refresh_token: String::new(),
                expires_in: 60,
            },
            42_000,
        )
        .unwrap();

        assert_eq!(refreshed.access_token, "new-access");
        assert_eq!(refreshed.refresh_token, "old-refresh");
        assert_eq!(refreshed.expires_at_ms, 102_000);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn partial_refresh_response_never_replaces_last_complete_encrypted_credential() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("partial-refresh-no-write");
        let oauth_dir = root.join("oauth");
        let agent = DaemonAgent::new(Config::default(), oauth_dir.clone());
        agent
            .store_credential(&StoredCredential {
                access_token: "complete-access".into(),
                refresh_token: "complete-refresh".into(),
                expires_at_ms: 99_000,
            })
            .unwrap();

        let incomplete = complete_claude_refresh(
            StoredCredential {
                access_token: "complete-access".into(),
                refresh_token: "complete-refresh".into(),
                expires_at_ms: 99_000,
            },
            isyncyou_agent::oauth::RefreshedToken {
                access_token: "partial-access".into(),
                refresh_token: String::new(),
                expires_in: 0,
            },
            50_000,
        );
        assert!(matches!(
            incomplete,
            Err(ProviderCredentialResolutionError::ProductReconnectRequired)
        ));

        let stored = load_agent_credential_blob(&oauth_dir, SUBSCRIPTION_CREDENTIAL_ID)
            .unwrap()
            .unwrap();
        let stored = StoredCredential::from_json(stored.expose()).unwrap();
        assert_eq!(stored.access_token, "complete-access");
        assert_eq!(stored.refresh_token, "complete-refresh");
        assert_eq!(stored.expires_at_ms, 99_000);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn concurrent_refresh_for_one_provider_is_serialized() {
        let root = apphost_credential_test_root("refresh-serialization");
        let agent = Arc::new(DaemonAgent::new(Config::default(), root.clone()));
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first = agent.clone();
        let first_worker = std::thread::spawn(move || {
            let _guard = first.credential_refresh_gate.lock().unwrap();
            locked_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        locked_rx.recv().unwrap();

        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let second = agent.clone();
        let second_worker = std::thread::spawn(move || {
            let _guard = second.credential_refresh_gate.lock().unwrap();
            acquired_tx.send(()).unwrap();
        });
        assert!(
            acquired_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "a second refresh must wait for the first refresh gate"
        );
        release_tx.send(()).unwrap();
        acquired_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        first_worker.join().unwrap();
        second_worker.join().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(feature = "agent-subscription-experimental")]
    #[test]
    fn experimental_local_cli_does_not_satisfy_product_harness_readiness() {
        let env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("local-not-product-ready");
        let local_claude = root.join("local-claude");
        write_local_cli_fixture(
            &local_claude.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"local-token"}}"#,
        );
        env.set_claude_config_dir(&local_claude);
        let agent = DaemonAgent::new(Config::default(), root.join("oauth"));

        let resolved = agent.resolve_claude_credential().unwrap();

        assert!(matches!(
            &resolved,
            ResolvedProviderCredential::Claude {
                origin: ProviderCredentialOrigin::ExperimentalLocalCli,
                ..
            }
        ));
        // #639: an experimental local-CLI credential never satisfies product readiness.
        assert!(!agent.provider_ready(ProductProviderId::Claude));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn product_harness_requires_encrypted_app_oauth_credential() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("product-harness-readiness");
        let agent = DaemonAgent::new(Config::default(), root.clone());
        agent
            .store_credential(&StoredCredential {
                access_token: "product-token".into(),
                refresh_token: String::new(),
                expires_at_ms: now_ms() + 3_600_000,
            })
            .unwrap();

        let resolved = agent.resolve_claude_credential().unwrap();
        assert!(matches!(
            resolved,
            ResolvedProviderCredential::Claude {
                origin: ProviderCredentialOrigin::ProductCredentialStore,
                ..
            }
        ));
        // #639: readiness is the durable activation authority (provider_ready), not credential origin.
        assert!(agent.provider_ready(ProductProviderId::Claude));
        let _ = std::fs::remove_dir_all(root);
    }

    // #639 T11: the ordered wire states recorded in the generation journal after an injected OAuth
    // token is committed. The injected token IS the OAuth-exchange output (not a pre-seeded "already
    // stored" credential), so this proves ordering through the real commit path.
    #[cfg(all(
        feature = "agent-oauth-providers",
        not(feature = "agent-subscription-experimental")
    ))]
    fn commit_injected_claude_oauth(agent: &DaemonAgent, access: &str) -> (String, Vec<String>) {
        let token = isyncyou_agent::oauth::RefreshedToken {
            access_token: access.to_string(),
            refresh_token: "injected-refresh".to_string(),
            expires_in: 3_600,
        };
        agent.commit_claude_oauth_success(&token).unwrap();
        let generation = load_product_bundle_meta(&agent.oauth_dir, SUBSCRIPTION_CREDENTIAL_ID)
            .unwrap()
            .generation;
        let store_id = OnboardingAttemptJournalV1::journal_store_id_for_generation(&generation);
        let journal = load_onboarding_journal_at(&agent.oauth_dir, &store_id).unwrap();
        let states = journal
            .transitions
            .iter()
            .map(|t| t.state.wire().to_string())
            .collect();
        (generation, states)
    }

    #[cfg(all(
        feature = "agent-oauth-providers",
        not(feature = "agent-subscription-experimental")
    ))]
    fn state_index(states: &[String], wire: &str) -> usize {
        states
            .iter()
            .position(|s| s == wire)
            .unwrap_or_else(|| panic!("state {wire} missing from journal: {states:?}"))
    }

    // #639 T11 (replaces the old seed-only oauth_completes_before_custom_harness_transformation):
    // an injected official OAuth is committed through the real path; the generation journal proves
    // official sign-in precedes credential encryption which precedes the harness transformation and
    // the terminal ready. Readiness is false before and true after — no seeded store is claimed.
    #[cfg(all(
        feature = "agent-oauth-providers",
        not(feature = "agent-subscription-experimental")
    ))]
    #[test]
    fn official_oauth_precedes_credential_encryption_and_harness_transformation() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("oauth-precedes-transformation");
        let _ = std::fs::remove_dir_all(&root);
        let agent = DaemonAgent::new(Config::default(), root.clone());
        // No credential yet: the product path is not ready and no turn provider builds.
        assert!(!agent.provider_ready(ProductProviderId::Claude));
        assert_ne!(agent.build_turn_provider("system").name(), "subscription");

        let (_generation, states) = commit_injected_claude_oauth(&agent, "official-exchange-token");
        assert_eq!(
            states.first().map(String::as_str),
            Some("official_oauth_completed")
        );
        assert!(
            state_index(&states, "official_oauth_completed")
                < state_index(&states, "credential_encrypted")
        );
        assert!(state_index(&states, "credential_encrypted") < state_index(&states, "ready"));
        assert_eq!(states.last().map(String::as_str), Some("ready"));
        // Only now is the product path ready and the real provider built.
        assert!(agent.provider_ready(ProductProviderId::Claude));
        assert_eq!(agent.build_turn_provider("system").name(), "subscription");
        let _ = std::fs::remove_dir_all(root);
    }

    // #639 T11: credential encryption is recorded before the harness activation steps.
    #[cfg(all(
        feature = "agent-oauth-providers",
        not(feature = "agent-subscription-experimental")
    ))]
    #[test]
    fn credential_encrypted_before_harness_activation() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("encrypted-before-activation");
        let _ = std::fs::remove_dir_all(&root);
        let agent = DaemonAgent::new(Config::default(), root.clone());
        let (_g, states) = commit_injected_claude_oauth(&agent, "t");
        assert!(
            state_index(&states, "credential_encrypted")
                < state_index(&states, "m365_profile_activated")
        );
        assert!(
            state_index(&states, "credential_encrypted")
                < state_index(&states, "isyncyou_tool_connected")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    // #639 T11: the custom harness (ready) is blocked until the retained envelope is verified.
    #[cfg(all(
        feature = "agent-oauth-providers",
        not(feature = "agent-subscription-experimental")
    ))]
    #[test]
    fn custom_harness_blocked_before_retained_envelope_verified() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("blocked-before-retained-envelope");
        let _ = std::fs::remove_dir_all(&root);
        let agent = DaemonAgent::new(Config::default(), root.clone());
        // A bundle written WITHOUT activation (envelope not yet verified via attestation) is not ready.
        let meta = ProductBundleMeta::fresh(ProductProviderId::Claude);
        store_agent_credential_blob(
            &root,
            SUBSCRIPTION_CREDENTIAL_ID,
            meta.to_blob(
                StoredCredential {
                    access_token: "t".into(),
                    refresh_token: "r".into(),
                    expires_at_ms: 9_999_999_999_999,
                }
                .to_json(),
            ),
        )
        .unwrap();
        assert!(!agent.provider_ready(ProductProviderId::Claude));
        // After the full commit the ordering places retained-envelope verification before ready.
        let _ = std::fs::remove_dir_all(&root);
        let agent = DaemonAgent::new(Config::default(), root.clone());
        let (_g, states) = commit_injected_claude_oauth(&agent, "t");
        assert!(state_index(&states, "retained_envelope_verified") < state_index(&states, "ready"));
        let _ = std::fs::remove_dir_all(root);
    }

    // #639 T11: the custom harness is blocked until the default harness is removed.
    #[cfg(all(
        feature = "agent-oauth-providers",
        not(feature = "agent-subscription-experimental")
    ))]
    #[test]
    fn custom_harness_blocked_before_default_harness_removed() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("blocked-before-default-removed");
        let _ = std::fs::remove_dir_all(&root);
        let agent = DaemonAgent::new(Config::default(), root.clone());
        let (_g, states) = commit_injected_claude_oauth(&agent, "t");
        assert!(state_index(&states, "default_harness_removed") < state_index(&states, "ready"));
        assert!(
            state_index(&states, "credential_encrypted")
                < state_index(&states, "default_harness_removed")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    // #639 T11: the custom harness is enabled (ready) only after the subscription identity is set.
    #[cfg(all(
        feature = "agent-oauth-providers",
        not(feature = "agent-subscription-experimental")
    ))]
    #[test]
    fn custom_harness_enabled_only_after_subscription_identity_set() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("enabled-after-subscription-identity");
        let _ = std::fs::remove_dir_all(&root);
        let agent = DaemonAgent::new(Config::default(), root.clone());
        let (_g, states) = commit_injected_claude_oauth(&agent, "t");
        assert!(state_index(&states, "subscription_identity_set") < state_index(&states, "ready"));
        assert_eq!(states.last().map(String::as_str), Some("ready"));
        let _ = std::fs::remove_dir_all(root);
    }

    // #639 T11: a corrupt product credential requires reconnect and never falls back to a local CLI
    // credential in the product (oauth-only) build.
    #[cfg(all(
        feature = "agent-oauth-providers",
        not(feature = "agent-subscription-experimental")
    ))]
    #[test]
    fn corrupt_product_credential_requires_reconnect_without_local_fallback() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("corrupt-requires-reconnect");
        let _ = std::fs::remove_dir_all(&root);
        // A legacy/corrupt blob (no V2 meta) is un-migratable -> PresentInvalid.
        store_agent_credential_blob(
            &root,
            SUBSCRIPTION_CREDENTIAL_ID,
            b"{\"access_token\":\"legacy-only\"}".to_vec(),
        )
        .unwrap();
        let agent = DaemonAgent::new(Config::default(), root.clone());
        assert_eq!(
            agent.product_credential_status("claude").unwrap(),
            "reconnect_required"
        );
        assert!(!agent.provider_ready(ProductProviderId::Claude));
        // No local-CLI fallback in the product build: the turn fails closed.
        assert!(isyncyou_webui::AgentHandler::start_turn(&agent, "me", "hi").is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    // #639 T11: a non-official OAuth policy (an override) cannot satisfy the official handoff — an
    // activation whose policy fingerprint is not the official one never reads ready.
    #[cfg(all(
        feature = "agent-oauth-providers",
        not(feature = "agent-subscription-experimental")
    ))]
    #[test]
    fn product_oauth_override_cannot_satisfy_official_handoff() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("override-cannot-satisfy-handoff");
        let _ = std::fs::remove_dir_all(&root);
        let mut agent = DaemonAgent::new(Config::default(), root.clone());
        agent.credential_now_ms = Arc::new(|| 10_000);
        let meta = ProductBundleMeta::fresh(ProductProviderId::Claude);
        store_agent_credential_blob(
            &root,
            SUBSCRIPTION_CREDENTIAL_ID,
            meta.to_blob(
                StoredCredential {
                    access_token: "t".into(),
                    refresh_token: "r".into(),
                    expires_at_ms: 3_610_000,
                }
                .to_json(),
            ),
        )
        .unwrap();
        // Activation minted under a NON-official policy fingerprint (an override).
        store_product_activation(
            &root,
            ProductProviderId::Claude,
            &ProductActivationV1 {
                provider_id: ProductProviderId::Claude.wire().to_string(),
                credential_generation: meta.generation.clone(),
                oauth_policy_fingerprint: "not-the-official-policy".to_string(),
                harness_contract_version: isyncyou_agent::HARNESS_CONTRACT_VERSION,
            },
        )
        .unwrap();
        // The activation's policy fingerprint does not match the official one -> not ready.
        assert!(!agent.provider_ready(ProductProviderId::Claude));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn product_oauth_config_rejects_endpoint_client_scope_override() {
        let root = apphost_credential_test_root("official-oauth-config-rejects-override");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("agent-oauth.json"),
            r#"{
                "authorize_url":"https://provider.invalid/oauth/authorize",
                "token_url":"https://provider.invalid/oauth/token",
                "client_id":"replacement-client",
                "scopes":["unexpected:scope"],
                "manual_redirect_url":"https://provider.invalid/oauth/callback"
            }"#,
        )
        .unwrap();
        let agent = DaemonAgent::new(Config::default(), root.clone());

        assert_eq!(
            agent.load_oauth_config().unwrap_err(),
            "OAuth recipe does not match the official product policy"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn product_oauth_config_accepts_only_compiled_official_recipe() {
        let root = apphost_credential_test_root("official-oauth-config-accepts-exact");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("agent-oauth.json"),
            r#"{
                "authorize_url":"https://claude.com/cai/oauth/authorize",
                "token_url":"https://platform.claude.com/v1/oauth/token",
                "client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e",
                "scopes":["user:inference"],
                "manual_redirect_url":"https://platform.claude.com/oauth/code/callback"
            }"#,
        )
        .unwrap();
        let agent = DaemonAgent::new(Config::default(), root.clone());

        let loaded = agent.load_oauth_config().unwrap();
        let official = isyncyou_agent::OAuthConfig::default();
        assert_eq!(loaded.authorize_url, official.authorize_url);
        assert_eq!(loaded.token_url, official.token_url);
        assert_eq!(loaded.client_id, official.client_id);
        assert_eq!(loaded.scopes, official.scopes);
        assert_eq!(loaded.manual_redirect_url, official.manual_redirect_url);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn daemon_agent_startup_removes_legacy_codex_callback_diagnostics() {
        let root = apphost_credential_test_root("codex-legacy-diagnostics-cleanup");
        std::fs::create_dir_all(&root).unwrap();
        let legacy = root.join(CODEX_CALLBACK_DIAGNOSTICS_FILE);
        std::fs::write(
            &legacy,
            "target=/auth/callback?code=secret&state=secret\nexchange=ERR secret",
        )
        .unwrap();

        let _agent = DaemonAgent::new(Config::default(), root.clone());

        assert!(!legacy.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn codex_callback_blocking_accept_cannot_outlive_deadline() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let attempts = Arc::new(Mutex::new(HashMap::new()));
        let cancelled = Arc::new(AtomicBool::new(false));
        let attempt_id = "deadline-test".to_string();
        attempts.lock().unwrap().insert(
            attempt_id.clone(),
            OAuthAttempt::Codex {
                cancelled: Arc::clone(&cancelled),
                expires_at: std::time::Instant::now() + OAUTH_ATTEMPT_TTL,
            },
        );
        let root = apphost_credential_test_root("codex-callback-deadline");
        let _ = std::fs::remove_dir_all(&root);
        let context = CodexCallbackContext {
            oauth_dir: root.clone(),
            cfg: isyncyou_agent::oauth::CodexOAuthConfig::default(),
            verifier: "verifier".into(),
            want_state: "state".into(),
            attempt_id: attempt_id.clone(),
            cancelled,
            attempts: Arc::clone(&attempts),
            product_runtime_gate: Arc::new(Mutex::new(())),
        };
        let started = std::time::Instant::now();

        codex_callback_serve_until(
            listener,
            context,
            started + std::time::Duration::from_millis(75),
        );

        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(!attempts.lock().unwrap().contains_key(&attempt_id));
        let _ = std::fs::remove_dir_all(root);
    }

    // #639 T8 AC1: crash window "after V2-write, before activation" — startup recovery activates the
    // EXISTING generation without a re-exchange (no new login, same generation).
    #[cfg(all(
        feature = "agent-oauth-providers",
        not(feature = "agent-subscription-experimental")
    ))]
    #[test]
    fn crash_after_credential_write_resumes_activation_without_reexchange() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("crash-window-2");
        let _ = std::fs::remove_dir_all(&root);
        // Simulate the crash: only the V2 bundle was written, activation was not.
        let cred = StoredCredential {
            access_token: "resumed-token".into(),
            refresh_token: "resumed-refresh".into(),
            expires_at_ms: 9_999_999_999_999,
        };
        let meta = ProductBundleMeta::fresh(ProductProviderId::Claude);
        store_agent_credential_blob(
            &root,
            SUBSCRIPTION_CREDENTIAL_ID,
            meta.to_blob(cred.to_json()),
        )
        .unwrap();
        assert!(load_product_activation(&root, ProductProviderId::Claude).is_none());
        // Constructing the agent runs startup recovery.
        let agent = DaemonAgent::new(Config::default(), root.clone());
        let activation = load_product_activation(&root, ProductProviderId::Claude)
            .expect("activation recovered without re-OAuth");
        // Same generation => no new login/exchange happened.
        assert_eq!(activation.credential_generation, meta.generation);
        assert!(agent.provider_ready(ProductProviderId::Claude));
        let _ = std::fs::remove_dir_all(root);
    }

    // #639 T8 AC2: crash window 1 (nothing written) recovers nothing and stays not ready.
    #[cfg(all(
        feature = "agent-oauth-providers",
        not(feature = "agent-subscription-experimental")
    ))]
    #[test]
    fn crash_before_credential_write_leaves_provider_not_ready() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("crash-window-1");
        let _ = std::fs::remove_dir_all(&root);
        let agent = DaemonAgent::new(Config::default(), root.clone());
        assert!(!agent.provider_ready(ProductProviderId::Claude));
        assert!(load_product_activation(&root, ProductProviderId::Claude).is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    // #639 T8 AC2: crash window 3 (bundle + matching activation, terminal journal entry missing) —
    // startup recovery appends the missing Ready transition to the generation journal.
    #[cfg(all(
        feature = "agent-oauth-providers",
        not(feature = "agent-subscription-experimental")
    ))]
    #[test]
    fn crash_after_activation_recovers_terminal_journal_transition() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("crash-window-3");
        let _ = std::fs::remove_dir_all(&root);
        let cred = StoredCredential {
            access_token: "t".into(),
            refresh_token: "r".into(),
            expires_at_ms: 9_999_999_999_999,
        };
        let meta = ProductBundleMeta::fresh(ProductProviderId::Claude);
        store_agent_credential_blob(
            &root,
            SUBSCRIPTION_CREDENTIAL_ID,
            meta.to_blob(cred.to_json()),
        )
        .unwrap();
        store_product_activation(
            &root,
            ProductProviderId::Claude,
            &ProductActivationV1 {
                provider_id: ProductProviderId::Claude.wire().to_string(),
                credential_generation: meta.generation.clone(),
                oauth_policy_fingerprint: oauth_policy_fingerprint(ProductProviderId::Claude),
                harness_contract_version: isyncyou_agent::HARNESS_CONTRACT_VERSION,
            },
        )
        .unwrap();
        let store_id =
            OnboardingAttemptJournalV1::journal_store_id_for_generation(&meta.generation);
        assert!(load_onboarding_journal_at(&root, &store_id).is_none());
        let _agent = DaemonAgent::new(Config::default(), root.clone());
        let journal =
            load_onboarding_journal_at(&root, &store_id).expect("generation journal recovered");
        assert!(journal.has_state(ProductOnboardingState::Ready));
        assert!(journal.has_state(ProductOnboardingState::OfficialOauthCompleted));
        let _ = std::fs::remove_dir_all(root);
    }

    // #639 T8 AC2: an interrupted attempt (callback timed out, not completed, not cancelled) is a
    // terminal, redacted transition in the attempt-keyed journal — never resumed.
    #[cfg(all(
        feature = "agent-oauth-providers",
        not(feature = "agent-subscription-experimental")
    ))]
    #[test]
    fn interrupted_oauth_attempt_records_error_redacted() {
        let _env = AppHostCredentialEnvGuard::new();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let attempts = Arc::new(Mutex::new(HashMap::new()));
        let cancelled = Arc::new(AtomicBool::new(false));
        let attempt_id = "interrupted-attempt".to_string();
        let root = apphost_credential_test_root("crash-window-4");
        let _ = std::fs::remove_dir_all(&root);
        // The in-flight attempt had recorded its official sign-in start.
        record_onboarding_attempt_transition(
            &root,
            &attempt_id,
            ProductOnboardingState::OfficialSignInStarted,
            None,
        );
        let context = CodexCallbackContext {
            oauth_dir: root.clone(),
            cfg: isyncyou_agent::oauth::CodexOAuthConfig::default(),
            verifier: "verifier".into(),
            want_state: "state".into(),
            attempt_id: attempt_id.clone(),
            cancelled,
            attempts: Arc::clone(&attempts),
            product_runtime_gate: Arc::new(Mutex::new(())),
        };
        codex_callback_serve_until(
            listener,
            context,
            std::time::Instant::now() + std::time::Duration::from_millis(75),
        );
        let journal = load_onboarding_journal(&root, &attempt_id).expect("attempt journal");
        assert!(journal.has_state(ProductOnboardingState::ErrorRedacted));
        let terminal = journal.transitions.last().unwrap();
        assert_eq!(terminal.state, ProductOnboardingState::ErrorRedacted);
        assert_eq!(terminal.error_code.as_deref(), Some("interrupted"));
        let _ = std::fs::remove_dir_all(root);
    }

    // #639 T8 AC3: the product completion path and status read share the product-runtime gate, so a
    // status/turn read can never observe a half-written credential/activation revision.
    #[cfg(all(
        feature = "agent-oauth-providers",
        not(feature = "agent-subscription-experimental")
    ))]
    #[test]
    fn completion_and_status_share_the_product_runtime_gate() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("gate-shared-status");
        let _ = std::fs::remove_dir_all(&root);
        let agent = Arc::new(DaemonAgent::new(Config::default(), root.clone()));
        // Hold the same gate the codex callback / commit path holds during a completion.
        let gate = Arc::clone(&agent.product_runtime_gate);
        let guard = gate.lock().unwrap();
        let probe = Arc::clone(&agent);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let status = isyncyou_webui::AgentHandler::status_json(probe.as_ref());
            let _ = tx.send(status);
        });
        // While a completion holds the gate, the status read cannot finish.
        assert!(rx
            .recv_timeout(std::time::Duration::from_millis(200))
            .is_err());
        drop(guard);
        // Once released, the status read completes against a single consistent revision.
        assert!(rx.recv_timeout(std::time::Duration::from_secs(2)).is_ok());
        let _ = std::fs::remove_dir_all(root);
    }

    // #639 T9 AC1: manual completion binds the pasted code to the named attempt — a wrong embedded
    // #state, a wrong/absent attempt id, or a missing #state is rejected, and the attempt survives.
    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn oauth_complete_rejects_wrong_attempt_state_and_codex() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("oauth-complete-binding");
        let _ = std::fs::remove_dir_all(&root);
        let agent = DaemonAgent::new(Config::default(), root.clone());
        agent.oauth_attempts.lock().unwrap().insert(
            "attempt-1".into(),
            OAuthAttempt::Claude {
                state: "S1".into(),
                expires_at: std::time::Instant::now() + OAUTH_ATTEMPT_TTL,
            },
        );
        // Wrong embedded #state.
        assert!(
            isyncyou_webui::AgentHandler::oauth_complete(&agent, "attempt-1", "code#S2").is_err()
        );
        // Wrong attempt id (state matches but the id does not name the attempt).
        assert!(
            isyncyou_webui::AgentHandler::oauth_complete(&agent, "missing", "code#S1").is_err()
        );
        // Missing #state entirely.
        assert!(
            isyncyou_webui::AgentHandler::oauth_complete(&agent, "attempt-1", "code-no-state")
                .is_err()
        );
        // A mismatched completion never consumes the attempt.
        assert!(agent
            .oauth_attempts
            .lock()
            .unwrap()
            .contains_key("attempt-1"));
        let _ = std::fs::remove_dir_all(root);
    }

    // #639 T9 AC2: the onboarding projection is derived from the durable activation, so a ready
    // provider still reports all 8 steps complete AFTER the attempt journal's TTL has expired.
    #[cfg(all(
        feature = "agent-oauth-providers",
        not(feature = "agent-subscription-experimental")
    ))]
    #[test]
    fn status_projection_correct_after_journal_ttl_expiry() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("status-projection-ttl");
        let _ = std::fs::remove_dir_all(&root);
        let mut agent = DaemonAgent::new(Config::default(), root.clone());
        agent.credential_now_ms = Arc::new(|| 10_000);
        agent
            .store_credential(&StoredCredential {
                access_token: "t".into(),
                refresh_token: "r".into(),
                expires_at_ms: 3_610_000,
            })
            .unwrap();
        agent.set_agent_settings("claude", DEFAULT_MODEL).unwrap();
        // No attempt journal is present (TTL-reaped / never persisted) — projection uses activation.
        let status: serde_json::Value =
            serde_json::from_str(&isyncyou_webui::AgentHandler::status_json(&agent)).unwrap();
        let onboarding = &status["onboarding"];
        assert_eq!(onboarding["selected_provider"], "claude");
        assert_eq!(onboarding["selected_state"], "ready");
        let claude = &onboarding["providers"]["claude"];
        assert_eq!(claude["state"], "ready");
        let steps = claude["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 8);
        assert!(steps.iter().all(|s| s["complete"] == true));
        // Codex is untouched -> not_started, no steps complete.
        let codex = &onboarding["providers"]["codex"];
        assert_eq!(codex["state"], "not_started");
        assert!(codex["steps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["complete"] == false));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn agent_provider_selection_uses_stored_claude_oauth_credential() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("agent-provider-claude");
        let _ = std::fs::remove_dir_all(&root);
        let agent = DaemonAgent::new(Config::default(), root.clone());
        agent
            .store_credential(&StoredCredential {
                access_token: "stored-claude-token".into(),
                refresh_token: String::new(),
                expires_at_ms: now_ms() + 3_600_000,
            })
            .unwrap();

        let provider = agent.build_turn_provider("system");

        assert_eq!(provider.name(), "subscription");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn agent_provider_selection_uses_stored_codex_oauth_credential() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("agent-provider-codex");
        let _ = std::fs::remove_dir_all(&root);
        let agent = DaemonAgent::new(Config::default(), root.clone());
        store_codex_blob(
            &root,
            &CodexStoredCredential {
                access_token: "stored-codex-token".into(),
                refresh_token: String::new(),
                account_id: "acct_123".into(),
                expires_at_ms: now_ms() + 3_600_000,
            },
        )
        .unwrap();
        agent.set_agent_settings("codex", "gpt-5.5").unwrap();

        let provider = agent.build_turn_provider("system");

        assert_eq!(provider.name(), "codex");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn successful_product_oauth_selection_uses_matching_provider_and_default_model() {
        let root = apphost_credential_test_root("oauth-provider-selection");
        let _ = std::fs::remove_dir_all(&root);

        store_agent_provider_selection(
            &root,
            "codex",
            &isyncyou_agent::CodexConfig::default().model,
        )
        .unwrap();
        let agent = DaemonAgent::new(Config::default(), root.clone());
        let (provider, model) = agent.agent_settings();

        assert_eq!(provider, "codex");
        assert_eq!(model, isyncyou_agent::CodexConfig::default().model);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn agent_status_keeps_selected_codex_provider_when_refresh_is_required() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("status-selected-codex-refresh");
        let _ = std::fs::remove_dir_all(&root);
        let mut agent = DaemonAgent::new(Config::default(), root.clone());
        agent.credential_now_ms = Arc::new(|| 10_000);
        agent
            .store_credential(&StoredCredential {
                access_token: "valid-claude-token".into(),
                refresh_token: "valid-claude-refresh".into(),
                expires_at_ms: 3_610_000,
            })
            .unwrap();
        store_codex_blob(
            &root,
            &CodexStoredCredential {
                access_token: "expired-codex-token".into(),
                refresh_token: "refresh-codex-token".into(),
                account_id: "acct_status".into(),
                expires_at_ms: 10_001,
            },
        )
        .unwrap();
        agent.set_agent_settings("codex", "gpt-5.5").unwrap();

        let status: serde_json::Value =
            serde_json::from_str(&isyncyou_webui::AgentHandler::status_json(&agent)).unwrap();
        assert_eq!(status["selected_provider"], "codex");
        assert_eq!(status["credential_state"]["claude"], "connected");
        assert_eq!(status["credential_state"]["codex"], "refresh_required");
        // #639 T7: the selection is KEPT on codex (no silent switch to the connected claude), and
        // `connected` now reflects the SELECTED provider's host-verified readiness only. Codex needs
        // refresh -> not ready -> connected:false (the UI prompts reconnect), even though claude is
        // separately ready. The old behavior falsely reported connected:true via the other provider.
        assert_eq!(status["provider"], "codex");
        assert_eq!(status["connected"], false);
        assert_eq!(status["codex"], false);
        assert_eq!(status["claude"], true);
        let _ = std::fs::remove_dir_all(root);
    }

    // #639 T7 AC1: a not-ready product turn is refused at the gate with the closed code and
    // creates no turn state (no stream slot). The router maps the code to 409 (webui test).
    #[cfg(all(
        feature = "agent-oauth-providers",
        not(feature = "agent-subscription-experimental")
    ))]
    #[test]
    fn product_not_ready_returns_409_without_creating_turn_state() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("gate-not-ready-409");
        let _ = std::fs::remove_dir_all(&root);
        let agent = DaemonAgent::new(Config::default(), root.clone());
        // No product credential -> the default-selected claude is not host-verified ready.
        assert!(!agent.provider_ready(ProductProviderId::Claude));
        let err = isyncyou_webui::AgentHandler::start_turn(&agent, "me", "hello").unwrap_err();
        assert_eq!(err, "product_not_ready");
        assert_eq!(agent.unopened_stream_count_for_tests(), 0);
        assert!(isyncyou_webui::AgentHandler::open_stream(&agent, "turn-0-0").is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    // #639 T7 AC2: when the SELECTED provider is not ready, the gate never falls back to the other
    // (even a fully ready) provider — the turn fails closed and `connected` reflects the selection.
    #[cfg(all(
        feature = "agent-oauth-providers",
        not(feature = "agent-subscription-experimental")
    ))]
    #[test]
    fn invalid_selected_provider_never_falls_back() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("gate-no-fallback");
        let _ = std::fs::remove_dir_all(&root);
        let mut agent = DaemonAgent::new(Config::default(), root.clone());
        agent.credential_now_ms = Arc::new(|| 10_000);
        // Claude is fully ready + activated.
        agent
            .store_credential(&StoredCredential {
                access_token: "ready-claude-token".into(),
                refresh_token: "ready-claude-refresh".into(),
                expires_at_ms: 3_610_000,
            })
            .unwrap();
        assert!(agent.provider_ready(ProductProviderId::Claude));
        // The selection is codex, which has no credential and is not ready.
        agent.set_agent_settings("codex", "gpt-5.5").unwrap();
        assert!(!agent.provider_ready(ProductProviderId::Codex));
        // No fallback to the ready claude: the turn fails closed and creates no turn state.
        let err = isyncyou_webui::AgentHandler::start_turn(&agent, "me", "hi").unwrap_err();
        assert_eq!(err, "product_not_ready");
        assert_eq!(agent.unopened_stream_count_for_tests(), 0);
        // The selection probe never builds the non-selected claude provider either.
        assert_ne!(agent.build_turn_provider("system").name(), "subscription");
        // Status: connected reflects the selected codex only; claude stays independently ready.
        let status: serde_json::Value =
            serde_json::from_str(&isyncyou_webui::AgentHandler::status_json(&agent)).unwrap();
        assert_eq!(status["selected_provider"], "codex");
        assert_eq!(status["connected"], false);
        assert_eq!(status["provider"], "codex");
        assert_eq!(status["codex"], false);
        assert_eq!(status["claude"], true);
        let _ = std::fs::remove_dir_all(root);
    }

    // #639 T7 AC3 / #627: an experimental local-CLI credential is never product-ready and never
    // sets `connected`, but the walled experimental turn path stays available (compiled opt-in).
    #[cfg(feature = "agent-subscription-experimental")]
    #[test]
    fn experimental_credential_never_sets_product_readiness() {
        let env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("experimental-never-ready");
        let _ = std::fs::remove_dir_all(&root);
        let local_claude = root.join("local-claude");
        write_local_cli_fixture(
            &local_claude.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"experimental-only-token"}}"#,
        );
        env.set_claude_config_dir(&local_claude);
        let agent = DaemonAgent::new(Config::default(), root.join("oauth"));
        // The experimental credential resolves via the local-CLI origin...
        assert!(matches!(
            agent.resolve_claude_credential().unwrap(),
            ResolvedProviderCredential::Claude {
                origin: ProviderCredentialOrigin::ExperimentalLocalCli,
                ..
            }
        ));
        // ...but it is NEVER product-ready and never sets connected.
        assert!(!agent.provider_ready(ProductProviderId::Claude));
        let status: serde_json::Value =
            serde_json::from_str(&isyncyou_webui::AgentHandler::status_json(&agent)).unwrap();
        assert_eq!(status["connected"], false);
        assert_eq!(status["claude"], false);
        // The walled experimental turn path is still available.
        assert!(agent
            .try_experimental_only_provider(ProductProviderId::Claude, "system")
            .is_some());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn codex_refresh_identity_change_requires_reconnect() {
        let current = CodexStoredCredential {
            access_token: "old".into(),
            refresh_token: "r".into(),
            account_id: "acct_original".into(),
            expires_at_ms: 1,
        };
        let refreshed = isyncyou_agent::oauth::CodexTokens {
            access_token: "new".into(),
            refresh_token: "r2".into(),
            account_id: "acct_DIFFERENT".into(),
            expires_in: 3600,
        };
        // a changed ChatGPT account id under refresh must force reconnect, never silently switch.
        assert!(matches!(
            complete_codex_refresh(current, refreshed, 1000),
            Err(ProviderCredentialResolutionError::ProductReconnectRequired)
        ));
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn refresh_keeps_generation_and_is_not_an_onboarding_event() {
        // Each login mints a fresh generation; re-persisting with explicit meta (the refresh path)
        // preserves it. There is no journal write here — a refresh is a credential-lifecycle event.
        let g1 = ProductBundleMeta::fresh(ProductProviderId::Claude).generation;
        let g2 = ProductBundleMeta::fresh(ProductProviderId::Claude).generation;
        assert_ne!(g1, g2, "each login mints a new generation");
        let meta = ProductBundleMeta {
            generation: g1.clone(),
            policy_fingerprint: oauth_policy_fingerprint(ProductProviderId::Claude),
            lifecycle: CredentialLifecycle::Active,
        };
        let blob = meta.to_blob(
            StoredCredential {
                access_token: "a".into(),
                refresh_token: "r".into(),
                expires_at_ms: 9,
            }
            .to_json(),
        );
        let back = ProductBundleMeta::from_blob(&blob).unwrap();
        assert_eq!(back.generation, g1, "refresh preserves the generation");
        assert_eq!(back.lifecycle, CredentialLifecycle::Active);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn failed_refresh_drops_readiness_without_relogin_journal() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("failed-refresh-reconnect");
        let _ = std::fs::remove_dir_all(&root);
        let agent = DaemonAgent::new(Config::default(), root.clone());
        // What a failed refresh persists: a ReconnectRequired lifecycle over a still-present token.
        agent
            .store_claude_bundle(
                &StoredCredential {
                    access_token: "tok".into(),
                    refresh_token: "r".into(),
                    expires_at_ms: u64::MAX,
                },
                &ProductBundleMeta {
                    generation: uuid_v4(),
                    policy_fingerprint: oauth_policy_fingerprint(ProductProviderId::Claude),
                    lifecycle: CredentialLifecycle::ReconnectRequired {
                        closed_code: "refresh_failed".into(),
                    },
                },
            )
            .unwrap();
        // Readiness drops to reconnect (PresentInvalid), not refresh_required — no refresh loop.
        assert!(matches!(
            agent.claude_product_credential_state(),
            ProductCredentialState::PresentInvalid
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn legacy_blob_without_v2_meta_reads_reconnect() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("legacy-blob-reconnect");
        let _ = std::fs::remove_dir_all(&root);
        let agent = DaemonAgent::new(Config::default(), root.clone());
        // A pre-#639 token-only blob (no schema_version/generation) is un-migratable -> reconnect.
        store_agent_credential_blob(
            &root,
            SUBSCRIPTION_CREDENTIAL_ID,
            StoredCredential {
                access_token: "tok".into(),
                refresh_token: "r".into(),
                expires_at_ms: u64::MAX,
            }
            .to_json(),
        )
        .unwrap();
        assert!(matches!(
            agent.claude_product_credential_state(),
            ProductCredentialState::PresentInvalid
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn activation_matches_requires_policy_and_contract_version() {
        let a = ProductActivationV1 {
            provider_id: "claude".into(),
            credential_generation: "gen-1".into(),
            oauth_policy_fingerprint: "fp-official".into(),
            harness_contract_version: HARNESS_CONTRACT_VERSION,
        };
        assert!(a.matches(
            ProductProviderId::Claude,
            "gen-1",
            "fp-official",
            HARNESS_CONTRACT_VERSION
        ));
        // generation alone is not enough — policy + contract + provider must all match.
        assert!(!a.matches(
            ProductProviderId::Claude,
            "gen-2",
            "fp-official",
            HARNESS_CONTRACT_VERSION
        ));
        assert!(!a.matches(
            ProductProviderId::Claude,
            "gen-1",
            "fp-override",
            HARNESS_CONTRACT_VERSION
        ));
        assert!(!a.matches(
            ProductProviderId::Claude,
            "gen-1",
            "fp-official",
            HARNESS_CONTRACT_VERSION + 1
        ));
        assert!(!a.matches(
            ProductProviderId::Codex,
            "gen-1",
            "fp-official",
            HARNESS_CONTRACT_VERSION
        ));
        // round-trip through the encrypted store
        let root = apphost_credential_test_root("activation-roundtrip");
        let _ = std::fs::remove_dir_all(&root);
        store_product_activation(&root, ProductProviderId::Claude, &a).unwrap();
        assert_eq!(
            load_product_activation(&root, ProductProviderId::Claude),
            Some(a)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn ready_activation_survives_attempt_journal_compaction() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("activation-survives-journal");
        let _ = std::fs::remove_dir_all(&root);
        let activation = ProductActivationV1 {
            provider_id: "claude".into(),
            credential_generation: "gen-keep".into(),
            oauth_policy_fingerprint: oauth_policy_fingerprint(ProductProviderId::Claude),
            harness_contract_version: HARNESS_CONTRACT_VERSION,
        };
        store_product_activation(&root, ProductProviderId::Claude, &activation).unwrap();
        // hammer the bounded journal past its cap; it compacts to MAX_JOURNAL_TRANSITIONS.
        let mut journal = OnboardingAttemptJournalV1 {
            transitions: Vec::new(),
        };
        for i in 0..(MAX_JOURNAL_TRANSITIONS + 20) {
            journal.push(OnboardingTransition {
                state: ProductOnboardingState::OfficialSignInStarted,
                generation: format!("g{i}"),
                error_code: None,
            });
        }
        assert_eq!(journal.transitions.len(), MAX_JOURNAL_TRANSITIONS);
        store_onboarding_journal(&root, "attempt-xyz", &journal).unwrap();
        assert_eq!(
            load_onboarding_journal(&root, "attempt-xyz")
                .unwrap()
                .transitions
                .len(),
            MAX_JOURNAL_TRANSITIONS
        );
        // The durable activation record (readiness authority) is unaffected by journal churn.
        assert_eq!(
            load_product_activation(&root, ProductProviderId::Claude),
            Some(activation)
        );
        // The runtime lock is exclusive: a second concurrent holder fails closed.
        let lock = try_acquire_product_runtime_lock(&root).unwrap();
        assert!(lock.is_some());
        assert!(try_acquire_product_runtime_lock(&root).unwrap().is_none());
        drop(lock);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(feature = "agent-network-device-test-hooks")]
    #[test]
    fn codex_refresh_device_hook_is_one_shot_and_does_not_change_the_clock() {
        let _ = take_codex_refresh_for_device_test();
        arm_codex_refresh_for_device_test();

        assert!(codex_refresh_for_device_test_is_armed());
        assert!(take_codex_refresh_for_device_test());
        assert!(!codex_refresh_for_device_test_is_armed());
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn agent_provider_selection_uses_fake_when_unconfigured() {
        let env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("agent-provider-fake");
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        env.set_home(&home);
        let agent = DaemonAgent::new(Config::default(), root.clone());

        let provider = agent.build_turn_provider("system");

        assert_eq!(provider.name(), "fake");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn agent_oauth_start_rejects_legacy_provider_ids() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("agent-oauth-legacy-provider");
        let _ = std::fs::remove_dir_all(&root);
        let agent = DaemonAgent::new(Config::default(), root.clone());

        let err = isyncyou_webui::AgentHandler::oauth_start(&agent, "openai", "")
            .expect_err("legacy OpenAI provider id must be rejected");
        assert!(err.contains("unknown provider"));
        let err = isyncyou_webui::AgentHandler::oauth_start(&agent, "anthropic", "")
            .expect_err("legacy Anthropic provider id must be rejected");
        assert!(err.contains("unknown provider"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn second_oauth_start_for_same_provider_conflicts_until_cancel() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("agent-oauth-single-claude-attempt");
        let _ = std::fs::remove_dir_all(&root);
        let agent = DaemonAgent::new(Config::default(), root.clone());

        let first = isyncyou_webui::AgentHandler::oauth_start_with_attempt(&agent, "claude", "")
            .expect("first Claude attempt starts");
        let second = isyncyou_webui::AgentHandler::oauth_start_with_attempt(&agent, "claude", "")
            .expect_err("second active Claude attempt must conflict");
        assert_eq!(second, "Claude sign-in is already in progress");

        isyncyou_webui::AgentHandler::oauth_cancel(&agent, "claude", &first.attempt_id)
            .expect("matching attempt cancels");
        assert!(
            isyncyou_webui::AgentHandler::oauth_start_with_attempt(&agent, "claude", "").is_ok(),
            "a cancelled attempt must not block a fresh login"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(all(
        feature = "agent-oauth-providers",
        not(feature = "agent-subscription-experimental")
    ))]
    #[test]
    fn product_build_does_not_read_dot_claude() {
        let env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("non-live-ignores-claude-home");
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        write_local_cli_fixture(
            &home.join(".claude/.credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"must-not-be-read"}}"#,
        );
        env.use_home_fallbacks(&home);
        let agent = DaemonAgent::new(Config::default(), root.join("oauth"));

        assert!(matches!(
            agent.resolve_claude_credential().unwrap(),
            ResolvedProviderCredential::Unconfigured(ProviderKind::Claude)
        ));
        assert_eq!(agent.build_turn_provider("system").name(), "fake");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(all(
        feature = "agent-oauth-providers",
        not(feature = "agent-subscription-experimental")
    ))]
    #[test]
    fn product_build_does_not_read_dot_codex() {
        let env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("non-live-ignores-codex-home");
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        write_local_cli_fixture(
            &home.join(".codex/auth.json"),
            r#"{"tokens":{"access_token":"must-not-be-read","account_id":"acct-real"}}"#,
        );
        env.use_home_fallbacks(&home);
        let agent = DaemonAgent::new(Config::default(), root.join("oauth"));

        assert!(matches!(
            agent.resolve_codex_credential().unwrap(),
            ResolvedProviderCredential::Unconfigured(ProviderKind::Codex)
        ));
        assert_eq!(agent.build_turn_provider("system").name(), "fake");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn non_live_provider_tests_clear_public_api_key_env() {
        let env = AppHostCredentialEnvGuard::new();
        std::env::set_var("ANTHROPIC_API_KEY", "leaked-anthropic-key");
        std::env::set_var("ANTHROPIC_AUTH_TOKEN", "leaked-anthropic-token");
        std::env::set_var("OPENAI_API_KEY", "leaked-openai-key");
        std::env::set_var("ISYNCYOU_AGENT_CRED_KEY", "leaked-credential-key");
        std::env::set_var("ISYNCYOU_AGENT_PROVIDER", "codex");
        std::env::set_var("ISYNCYOU_AGENT_MODEL", "leaked-model");

        env.isolate_provider_env();

        for key in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "OPENAI_API_KEY",
            "ISYNCYOU_AGENT_CRED_KEY",
            "ISYNCYOU_AGENT_PROVIDER",
            "ISYNCYOU_AGENT_MODEL",
        ] {
            assert_eq!(std::env::var_os(key), None, "{key} must be cleared");
        }
        for key in ["HOME", "CODEX_HOME", "CLAUDE_CONFIG_DIR"] {
            let path = std::env::var_os(key).expect("isolated config env");
            assert!(
                PathBuf::from(path).starts_with(&env.root),
                "{key} must point at the isolated test root"
            );
        }
    }

    #[cfg(all(
        feature = "agent-oauth-providers",
        not(feature = "agent-subscription-experimental")
    ))]
    #[test]
    fn product_oauth_provider_does_not_read_desktop_cli_fallbacks() {
        let env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("product-no-cli-fallback");
        let _ = std::fs::remove_dir_all(&root);
        let oauth_dir = root.join("oauth");
        let home = root.join("home");
        write_local_cli_fixture(
            &home.join(".claude/.credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"desktop-claude-token"}}"#,
        );
        write_local_cli_fixture(
            &home.join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"desktop-account"},"userID":"desktop-device"}"#,
        );
        write_local_cli_fixture(
            &home.join(".codex/auth.json"),
            r#"{"tokens":{"access_token":"desktop-codex-token","account_id":"desktop-account"}}"#,
        );
        env.use_home_fallbacks(&home);

        let agent = DaemonAgent::new(Config::default(), oauth_dir);

        assert!(matches!(
            agent.resolve_claude_credential().unwrap(),
            ResolvedProviderCredential::Unconfigured(ProviderKind::Claude)
        ));
        assert!(matches!(
            agent.resolve_codex_credential().unwrap(),
            ResolvedProviderCredential::Unconfigured(ProviderKind::Codex)
        ));
        let cfg = agent.subscription_config();
        assert!(cfg.account_uuid.is_empty());
        assert!(cfg.device_id.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(all(
        feature = "agent-oauth-providers",
        not(feature = "agent-subscription-experimental")
    ))]
    #[test]
    fn agent_subscription_experimental_required_for_local_cli_fallback() {
        let env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("experimental-feature-required");
        let local_claude = root.join("local-claude");
        write_local_cli_fixture(
            &local_claude.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"must-not-be-read"}}"#,
        );
        env.set_claude_config_dir(&local_claude);
        let agent = DaemonAgent::new(Config::default(), root.join("oauth"));

        assert!(matches!(
            agent.resolve_claude_credential().unwrap(),
            ResolvedProviderCredential::Unconfigured(ProviderKind::Claude)
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn app_host_product_source_has_no_credential_ingest_surface() {
        let source = include_str!("lib.rs");
        let method = ["subscription", "_import"].concat();
        let route = ["subscription", "/import"].concat();

        assert!(!source.contains(&method));
        assert!(!source.contains(&route));
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn daemon_agent_status_reports_sanitized_last_provider_usage() {
        let root = apphost_credential_test_root("usage-status");
        let _ = std::fs::remove_dir_all(&root);
        let agent = DaemonAgent::new(Config::default(), root.clone());
        *agent.last_usage.lock().unwrap() = Some(isyncyou_agent::Usage {
            input_tokens: 42,
            output_tokens: 9,
            provider: "claude".into(),
            model: "claude-sonnet-5".into(),
            request_id: Some("req-123".into()),
            rate_limit: std::collections::BTreeMap::from([(
                "anthropic-ratelimit-tokens-remaining".into(),
                "99".into(),
            )]),
        });

        let status: serde_json::Value =
            serde_json::from_str(&isyncyou_webui::AgentHandler::status_json(&agent)).unwrap();

        assert_eq!(status["usage"]["provider"], "claude");
        assert_eq!(status["usage"]["model"], "claude-sonnet-5");
        assert_eq!(status["usage"]["input_tokens"], 42);
        assert_eq!(status["usage"]["output_tokens"], 9);
        assert_eq!(status["usage"]["request_id"], "req-123");
        assert_eq!(
            status["usage"]["rate_limit"]["anthropic-ratelimit-tokens-remaining"],
            "99"
        );
        let rendered = status.to_string();
        assert!(!rendered.contains("Bearer"));
        assert!(!rendered.contains("refresh"));
        assert!(!rendered.contains("acct_"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn app_host_product_code_does_not_reference_legacy_byo_provider_types() {
        let src = include_str!("lib.rs");
        let production = production_source_before_final_test_module(src);

        for needle in [
            "AnthropicProvider",
            "OpenAiProvider",
            "ProviderCredentialResolver",
            "provider_api_key_secret_id",
        ] {
            assert!(
                !production.contains(needle),
                "app-host product code must not reference legacy BYO provider surface: {needle}"
            );
        }
    }

    #[cfg(any(
        feature = "agent-oauth-providers",
        feature = "agent-subscription-experimental"
    ))]
    #[test]
    fn stored_credential_error_redaction_does_not_leak_tokens() {
        let _env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("credential-redaction");
        let _ = std::fs::remove_dir_all(&root);
        let leaked_env_value = "access_token=apphost-redaction-sentinel refresh_token=also-secret";
        std::env::set_var("ISYNCYOU_AGENT_CRED_KEY", leaked_env_value);
        let err = store_agent_credential_blob(
            &root,
            SUBSCRIPTION_CREDENTIAL_ID,
            StoredCredential {
                access_token: "payload-access-token-sentinel".into(),
                refresh_token: "payload-refresh-token-sentinel".into(),
                expires_at_ms: 0,
            }
            .to_json(),
        )
        .unwrap_err();

        assert_eq!(err, "agent credential store error");
        assert!(!err.contains("apphost-redaction-sentinel"));
        assert!(!err.contains("also-secret"));
        assert!(!err.contains("payload-access-token-sentinel"));
        assert!(!err.contains("payload-refresh-token-sentinel"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn restore_handler_refuses_non_restorable_service_before_token_lookup() {
        // The restore handler refuses a service with no crash-safe cloud restore
        // before any cached-token lookup (so no token is needed for the clear message).
        let h = DaemonRestore {
            cfg: Config::default(),
        };
        let err = isyncyou_webui::RestoreHandler::restore(&h, "a", "onedrive", "x").unwrap_err();
        assert!(err.contains("not crash-safe yet"), "onedrive: got: {err}");
    }

    #[test]
    fn daemon_share_impl_does_not_call_graph_share_mutations_directly() {
        let src = include_str!("lib.rs");
        let start = src
            .find("impl isyncyou_webui::ShareHandler for DaemonShare")
            .expect("DaemonShare ShareHandler impl exists");
        let tail = &src[start..];
        let end = tail
            .find("/// Live OneDrive info")
            .expect("DaemonShare impl sentinel exists");
        let block = &tail[..end];
        assert!(
            !block.contains(".create_link("),
            "DaemonShare must route share links through the engine ledger wrapper"
        );
        assert!(
            !block.contains(".invite("),
            "DaemonShare must route invites through the engine ledger wrapper"
        );
    }

    #[test]
    fn daemon_settings_persists_and_applies_poll_interval() {
        use isyncyou_webui::SettingsHandler;
        let dir = std::env::temp_dir().join(format!("isy-apphost-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("isyncyou.toml");
        Config::default().save(&path).unwrap();
        let live = Arc::new(AtomicU64::new(5));
        let h = DaemonSettings {
            config_path: path.clone(),
            live_interval: live.clone(),
        };
        h.set_poll_interval_secs(42).unwrap();
        assert_eq!(live.load(Ordering::Relaxed), 42);
        assert_eq!(Config::load(&path).unwrap().sync.poll_interval_secs, 42);
        h.set_poll_interval_secs(99_999).unwrap();
        assert_eq!(live.load(Ordering::Relaxed), 3600);
        assert_eq!(Config::load(&path).unwrap().sync.poll_interval_secs, 3600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn od_item(id: &str, parent: Option<&str>, item_type: &str, size: Option<i64>) -> Item {
        let mut item = Item::new("a", "onedrive", id, id, item_type);
        item.parent_remote_id = parent.map(str::to_string);
        item.size = size;
        item
    }

    fn od_map(items: &[Item]) -> std::collections::HashMap<&str, &Item> {
        items
            .iter()
            .map(|item| (item.remote_id.as_str(), item))
            .collect()
    }

    fn od_modes(default_mode: OneDriveMode, pairs: &[(&str, OneDriveMode)]) -> OneDriveModes {
        OneDriveModes {
            default_mode,
            folder_modes: pairs
                .iter()
                .map(|(id, mode)| ((*id).to_string(), *mode))
                .collect(),
        }
    }

    #[test]
    fn onedrive_risk_move_uses_explicit_offline_scope_owner_only() {
        let items = vec![
            od_item("off", None, "folder", None),
            od_item("off-file", Some("off"), "file", Some(1)),
            od_item("sync", None, "folder", None),
            od_item("sync-file", Some("sync"), "file", Some(1)),
            od_item("plain", None, "folder", None),
            od_item("plain-file", Some("plain"), "file", Some(1)),
        ];
        let by_id = od_map(&items);
        let modes = od_modes(
            OneDriveMode::Online,
            &[("off", OneDriveMode::Offline), ("sync", OneDriveMode::Sync)],
        );

        assert_eq!(
            classify_onedrive_move_risk_from_items(Some(&modes), &by_id, "off-file", ""),
            OneDriveMoveRisk::MoveOutOfProtected {
                source_scope: "off".into(),
                destination_scope: None,
            }
        );
        assert_eq!(
            classify_onedrive_move_risk_from_items(Some(&modes), &by_id, "off-file", "off"),
            OneDriveMoveRisk::Low
        );
        assert_eq!(
            classify_onedrive_move_risk_from_items(Some(&modes), &by_id, "sync-file", ""),
            OneDriveMoveRisk::Low,
            "explicit Sync scopes are not protected Offline scopes"
        );
        assert_eq!(
            classify_onedrive_move_risk_from_items(Some(&modes), &by_id, "plain-file", "missing"),
            OneDriveMoveRisk::Low,
            "unprotected sources stay low-risk even when destination metadata is stale"
        );
        assert_eq!(
            classify_onedrive_move_risk_from_items(Some(&modes), &by_id, "off-file", "sync"),
            OneDriveMoveRisk::MoveOutOfProtected {
                source_scope: "off".into(),
                destination_scope: None,
            },
            "Sync-only destinations do not count as Offline owners"
        );

        let default_offline = od_modes(OneDriveMode::Offline, &[]);
        assert_eq!(
            classify_onedrive_move_risk_from_items(
                Some(&default_offline),
                &by_id,
                "plain-file",
                "",
            ),
            OneDriveMoveRisk::Low,
            "default_mode=Offline is not an explicit protected scope root"
        );
    }

    #[test]
    fn onedrive_risk_move_missing_and_nested_scope_cases() {
        let items = vec![
            od_item("root-off", None, "folder", None),
            od_item("child-off", Some("root-off"), "folder", None),
            od_item("source", Some("child-off"), "file", Some(1)),
        ];
        let by_id = od_map(&items);
        let modes = od_modes(
            OneDriveMode::Online,
            &[
                ("root-off", OneDriveMode::Offline),
                ("child-off", OneDriveMode::Offline),
            ],
        );

        assert_eq!(
            classify_onedrive_move_risk_from_items(Some(&modes), &by_id, "missing", ""),
            OneDriveMoveRisk::Unknown {
                reason: "missing_source".into(),
            }
        );
        assert_eq!(
            classify_onedrive_move_risk_from_items(Some(&modes), &by_id, "source", "missing"),
            OneDriveMoveRisk::Unknown {
                reason: "missing_destination".into(),
            }
        );
        assert_eq!(
            classify_onedrive_move_risk_from_items(Some(&modes), &by_id, "source", "root-off"),
            OneDriveMoveRisk::MoveOutOfProtected {
                source_scope: "child-off".into(),
                destination_scope: Some("root-off".into()),
            },
            "deepest explicit Offline scope owns the source"
        );
        assert_eq!(
            classify_onedrive_move_risk_from_items(Some(&modes), &by_id, "source", "child-off"),
            OneDriveMoveRisk::Low
        );
    }

    #[test]
    fn onedrive_risk_offline_mode_estimate_thresholds() {
        let folder = od_item("folder", None, "folder", None);
        let small = od_item("small", Some("folder"), "file", Some(4));
        let large = od_item(
            "large",
            Some("folder"),
            "file",
            Some(OFFLINE_LARGE_BYTE_THRESHOLD as i64),
        );
        let unknown = od_item("unknown", Some("folder"), "file", None);
        let nested_folder = od_item("nested", Some("folder"), "folder", None);
        let nested_file = od_item("nested-file", Some("nested"), "file", Some(5));

        let missing_items = vec![small.clone()];
        assert_eq!(
            estimate_onedrive_offline_mode_risk_from_items(&od_map(&missing_items), "folder"),
            offline_mode_risk(true, 0, 0, 0, "unknown_folder")
        );

        let empty_items = vec![folder.clone()];
        assert_eq!(
            estimate_onedrive_offline_mode_risk_from_items(&od_map(&empty_items), "folder"),
            offline_mode_risk(false, 0, 0, 0, "small")
        );

        let small_items = vec![folder.clone(), small.clone()];
        assert_eq!(
            estimate_onedrive_offline_mode_risk_from_items(&od_map(&small_items), "folder"),
            offline_mode_risk(false, 1, 4, 0, "small")
        );

        let bulk_items = vec![folder.clone(), small.clone(), nested_folder, nested_file];
        let bulk = estimate_onedrive_offline_mode_risk_from_items(&od_map(&bulk_items), "folder");
        assert!(bulk.requires_confirmation);
        assert_eq!(bulk.file_count, 2);
        assert_eq!(bulk.reason, "bulk_files");

        let large_items = vec![folder.clone(), large];
        let large = estimate_onedrive_offline_mode_risk_from_items(&od_map(&large_items), "folder");
        assert!(large.requires_confirmation);
        assert_eq!(large.reason, "large_bytes");

        let unknown_items = vec![folder.clone(), unknown];
        let unknown =
            estimate_onedrive_offline_mode_risk_from_items(&od_map(&unknown_items), "folder");
        assert!(unknown.requires_confirmation);
        assert_eq!(unknown.unknown_size_files, 1);
        assert_eq!(unknown.reason, "unknown_size");

        let file_items = vec![od_item("file-id", None, "file", Some(1))];
        assert_eq!(
            estimate_onedrive_offline_mode_risk_from_items(&od_map(&file_items), "file-id"),
            offline_mode_risk(true, 0, 0, 0, "not_folder")
        );
    }

    #[test]
    fn daemon_onedrive_risk_reads_store_and_config() {
        let dir = std::env::temp_dir().join(format!("isy-apphost-risk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let arch = dir.join("archive");
        let sync = dir.join("sync");
        let cache = dir.join("cache");
        std::fs::create_dir_all(&arch).unwrap();
        std::fs::create_dir_all(&sync).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let config_path = dir.join("isyncyou.toml");
        let mut cfg = Config {
            accounts: vec![isyncyou_core::AccountConfig {
                id: "a".into(),
                username: "a".into(),
                sync_root: sync,
                archive_root: arch.clone(),
                cache_root: cache,
                mount_point: None,
            }],
            ..Default::default()
        };
        cfg.onedrive_modes.insert(
            "a".into(),
            od_modes(OneDriveMode::Online, &[("off", OneDriveMode::Offline)]),
        );
        cfg.save(&config_path).unwrap();
        {
            let store = isyncyou_store::Store::open(arch.join(".isyncyou-store.db")).unwrap();
            for item in [
                od_item("off", None, "folder", None),
                od_item("f1", Some("off"), "file", Some(1)),
                od_item("f2", Some("off"), "file", Some(2)),
            ] {
                store.upsert_item(&item).unwrap();
            }
        }

        let handler = DaemonOneDriveRisk {
            config_path: config_path.clone(),
        };
        assert_eq!(
            isyncyou_webui::OneDriveRiskHandler::move_risk(&handler, "a", "f1", "").unwrap(),
            OneDriveMoveRisk::MoveOutOfProtected {
                source_scope: "off".into(),
                destination_scope: None,
            }
        );
        let offline =
            isyncyou_webui::OneDriveRiskHandler::offline_mode_risk(&handler, "a", "off").unwrap();
        assert!(offline.requires_confirmation);
        assert_eq!(offline.file_count, 2);
        assert_eq!(offline.reason, "bulk_files");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn daemon_transfer_surfaces_shared_progress_at_endpoint() {
        // The engine's SharedProgress (what the offline pass writes) is read back through
        // DaemonTransfer at GET /api/v1/onedrive/transfers (#655).
        use isyncyou_connectors::ProgressSink;
        let progress = SharedProgress::new();
        progress.begin("i1", "photo.jpg", 1000);
        progress.advance("i1", 400);
        let events = Arc::new(isyncyou_webui::EventBus::new());
        let router = build_live_router(
            Config::default(),
            None,
            events,
            PathBuf::from("/x/isyncyou.toml"),
            Arc::new(AtomicU64::new(5)),
            progress.clone(),
            AgentOperationPolicy::DesktopEnabled,
        );
        let resp = router.route(&ApiRequest::get("/api/v1/onedrive/transfers"));
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["count"].as_u64(), Some(1));
        assert_eq!(v["transfers"][0]["name"].as_str(), Some("photo.jpg"));
        assert_eq!(v["transfers"][0]["bytes_done"].as_u64(), Some(400));
        assert_eq!(v["transfers"][0]["bytes_total"].as_u64(), Some(1000));
    }

    #[test]
    fn onedrive_open_serves_plaintext_cached_sync_body_when_envelope_not_required() {
        let _guard = EnvelopeRequirementGuard::new();
        let dir =
            std::env::temp_dir().join(format!("isy-apphost-open-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let arch = dir.join("archive");
        let sync = dir.join("sync");
        let cache = dir.join("cache");
        std::fs::create_dir_all(&arch).unwrap();
        std::fs::create_dir_all(&sync).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let config_path = dir.join("isyncyou.toml");
        let cfg = Config {
            accounts: vec![isyncyou_core::AccountConfig {
                id: "a".into(),
                username: "a".into(),
                sync_root: sync,
                archive_root: arch.clone(),
                cache_root: cache.clone(),
                mount_point: None,
            }],
            ..Default::default()
        };
        cfg.save(&config_path).unwrap();
        {
            let store = isyncyou_store::Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut item = Item::new("a", "onedrive", "file-id", "doc.txt", "file");
            item.local_path = Some("doc.txt".into());
            store.upsert_item(&item).unwrap();
            store
                .set_content_state(
                    "a",
                    "onedrive",
                    "file-id",
                    Some("cached"),
                    Some("cache"),
                    Some("available"),
                    None,
                )
                .unwrap();
        }
        std::fs::write(cache.join("doc.txt"), b"cached bytes").unwrap();

        let h = DaemonOneDriveOpen {
            config_path,
            progress: SharedProgress::new(),
        };
        let got = isyncyou_webui::OneDriveOpenHandler::download(&h, "a", "file-id").unwrap();
        assert_eq!(got, b"cached bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn onedrive_open_requires_sealed_cached_body_when_envelope_required() {
        let _guard = EnvelopeRequirementGuard::new();
        isyncyou_core::envelope::set_body_key(719_001, [1u8; 32]);
        isyncyou_core::envelope::require_body_envelope_for_process();

        let dir = std::env::temp_dir().join(format!(
            "isy-apphost-open-cache-strict-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let arch = dir.join("archive");
        let sync = dir.join("sync");
        let cache = dir.join("cache");
        std::fs::create_dir_all(&arch).unwrap();
        std::fs::create_dir_all(&sync).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let config_path = dir.join("isyncyou.toml");
        let cfg = Config {
            accounts: vec![isyncyou_core::AccountConfig {
                id: "a".into(),
                username: "a".into(),
                sync_root: sync,
                archive_root: arch.clone(),
                cache_root: cache.clone(),
                mount_point: None,
            }],
            ..Default::default()
        };
        cfg.save(&config_path).unwrap();
        {
            let store = isyncyou_store::Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut item = Item::new("a", "onedrive", "file-id", "doc.txt", "file");
            item.local_path = Some("doc.txt".into());
            store.upsert_item(&item).unwrap();
            store
                .set_content_state(
                    "a",
                    "onedrive",
                    "file-id",
                    Some("cached"),
                    Some("cache"),
                    Some("available"),
                    None,
                )
                .unwrap();
        }
        let h = DaemonOneDriveOpen {
            config_path,
            progress: SharedProgress::new(),
        };

        isyncyou_core::envelope::write_body_atomic(&cache.join("doc.txt"), b"sealed cached bytes")
            .unwrap();
        let got = isyncyou_webui::OneDriveOpenHandler::download(&h, "a", "file-id").unwrap();
        assert_eq!(got, b"sealed cached bytes");

        std::fs::write(cache.join("doc.txt"), b"raw cached bytes").unwrap();
        let err = isyncyou_webui::OneDriveOpenHandler::download(&h, "a", "file-id").unwrap_err();
        assert!(
            err.contains("sealed envelope"),
            "strict mobile open must reject plaintext cached bodies, got: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upload_staging_uses_account_cache_root_and_body_envelope_when_keyed() {
        let _guard = EnvelopeRequirementGuard::new();
        isyncyou_core::envelope::set_body_key(719, [7u8; 32]);
        let dir =
            std::env::temp_dir().join(format!("isy-apphost-upload-staging-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = dir.join("cache");
        let cfg = Config {
            accounts: vec![isyncyou_core::AccountConfig {
                id: "a".into(),
                username: "a".into(),
                sync_root: dir.join("sync"),
                archive_root: dir.join("archive"),
                cache_root: cache.clone(),
                mount_point: None,
            }],
            ..Default::default()
        };
        let sentinel = b"upload-stage-plaintext-sentinel-719";
        let tmp = TempBody::write(&cfg, "a", sentinel).unwrap();
        let staged_path = tmp.path().to_path_buf();
        assert!(
            staged_path.starts_with(cache.join("upload-staging")),
            "upload staging must stay under the account-private cache root: {staged_path:?}"
        );
        let raw = std::fs::read(&staged_path).unwrap();
        assert_eq!(
            isyncyou_core::envelope::blob_key_id(&raw),
            Some(719),
            "keyed Android staging must be a sealed body envelope"
        );
        assert!(
            !raw.windows(sentinel.len()).any(|w| w == sentinel),
            "staging file must not contain plaintext upload bytes"
        );
        assert_eq!(
            isyncyou_core::envelope::read_body(&staged_path).unwrap(),
            sentinel
        );
        drop(tmp);
        assert!(
            !staged_path.exists(),
            "short-lived staging file should be removed on drop"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upload_staging_cleanup_removes_only_stale_staging_files() {
        let dir = std::env::temp_dir().join(format!(
            "isy-apphost-upload-staging-cleanup-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let stale = dir.join(format!("{}stale.bin", TempBody::PREFIX));
        let fresh = dir.join(format!("{}fresh.bin", TempBody::PREFIX));
        let unrelated = dir.join("unrelated-upload.bin");
        std::fs::write(&stale, b"stale").unwrap();
        std::fs::write(&fresh, b"fresh").unwrap();
        std::fs::write(&unrelated, b"unrelated").unwrap();
        let old = std::time::SystemTime::now()
            .checked_sub(TempBody::STALE_AFTER + Duration::from_secs(60))
            .unwrap();
        filetime::set_file_mtime(&stale, filetime::FileTime::from_system_time(old)).unwrap();

        TempBody::cleanup_stale(&dir);

        assert!(
            !stale.exists(),
            "stale upload staging files must be removed"
        );
        assert!(fresh.exists(), "fresh upload staging files must be kept");
        assert!(
            unrelated.exists(),
            "non-staging files in the staging directory must be left alone"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upload_staging_held_file_survives_cleanup_and_stays_sealed() {
        let _guard = EnvelopeRequirementGuard::new();
        isyncyou_core::envelope::set_body_key(720_003, [3u8; 32]);
        let dir = std::env::temp_dir().join(format!(
            "isy-apphost-upload-staging-held-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let sentinel = b"upload-stage-held-plaintext-sentinel-720";
        let tmp = TempBody::write_in_dir(&dir, sentinel).unwrap();
        let staged_path = tmp.path().to_path_buf();

        TempBody::cleanup_stale(&dir);

        assert!(
            staged_path.exists(),
            "held fresh staging file must not be removed by cleanup"
        );
        let raw = std::fs::read(&staged_path).unwrap();
        assert_eq!(
            isyncyou_core::envelope::blob_key_id(&raw),
            Some(720_003),
            "held staging file must remain a sealed body envelope"
        );
        assert!(
            !raw.windows(sentinel.len()).any(|w| w == sentinel),
            "held staging file must not expose plaintext bytes on disk"
        );
        assert_eq!(
            isyncyou_core::envelope::read_body(&staged_path).unwrap(),
            sentinel
        );
        drop(tmp);
        assert!(
            !staged_path.exists(),
            "held staging file should still be removed on drop"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn daemon_transfer_cancel_requests_cancellation() {
        // DaemonTransfer::cancel (#656) is best-effort: it always accepts and flags the id on
        // the shared progress so the materialize pass skips it before its next file boundary.
        use isyncyou_connectors::ProgressSink;
        use isyncyou_webui::TransferProgress;
        let progress = SharedProgress::new();
        progress.begin("i1", "photo.jpg", 1000);
        let dt = DaemonTransfer {
            progress: progress.clone(),
        };
        assert!(dt.cancel("i1"), "cancel is always accepted (best-effort)");
        assert!(
            progress.is_cancelled("i1"),
            "the cancel is recorded on the shared progress for the pass to observe"
        );
        assert!(
            !progress.is_cancelled("other"),
            "an unrelated id is unaffected"
        );
    }

    #[test]
    fn daemon_transfer_pause_resume_retry_and_paused_flag() {
        // #659: pause/resume/retry map onto the shared progress; the endpoint surfaces `paused`.
        use isyncyou_connectors::ProgressSink;
        use isyncyou_webui::TransferProgress;
        let progress = SharedProgress::new();
        progress.begin("i1", "photo.jpg", 1000);
        progress.retry_after("i1", 30);
        let dt = DaemonTransfer {
            progress: progress.clone(),
        };
        assert!(dt.pause("i1"));
        assert!(
            progress.is_paused_id("i1"),
            "pause is recorded (persistent)"
        );
        // The endpoint mapping derives `paused` from the pause-set.
        assert!(
            dt.transfers()[0].paused,
            "transfers() surfaces the paused flag"
        );

        assert!(dt.resume("i1"));
        assert!(!progress.is_paused_id("i1"), "resume clears the pause");

        // retry un-pauses AND clears the 429 backoff so the panel shows it retrying now.
        dt.pause("i1");
        assert!(dt.retry("i1"));
        assert!(!progress.is_paused_id("i1"), "retry un-pauses");
        assert_eq!(
            progress.snapshot()[0].retry_after_secs,
            0,
            "retry clears the backoff timer"
        );
    }

    #[test]
    fn build_live_router_wires_manage_and_transfer_controls() {
        // #659: build_live_router wires the management handler + the pause/retry transfer controls.
        // A cap-gated POST with NO cap token returns 401 (not 404) → proves the handler is wired.
        let events = Arc::new(isyncyou_webui::EventBus::new());
        let router = build_live_router(
            Config::default(),
            None,
            events,
            PathBuf::from("/x/isyncyou.toml"),
            Arc::new(AtomicU64::new(5)),
            SharedProgress::new(),
            AgentOperationPolicy::DesktopEnabled,
        );
        for path in [
            "/api/v1/onedrive/free-up?account=a&id=i1",
            "/api/v1/onedrive/download-now?account=a&id=i1",
            "/api/v1/onedrive/conflict/resolve?account=a&id=i1&resolution=keep-both",
            "/api/v1/onedrive/cleanup?account=a",
            "/api/v1/onedrive/transfers/pause?id=i1",
            "/api/v1/onedrive/transfers/retry?id=i1",
        ] {
            let resp = router.route(&ApiRequest::new("POST", path));
            assert_eq!(resp.status, 401, "wired + cap-gated (not 404): {path}");
        }
        // The conflicts GET read is wired too (404 would mean no handler).
        let c = router.route(&ApiRequest::get("/api/v1/onedrive/conflicts?account=a"));
        assert_ne!(c.status, 404, "conflicts GET is wired");
    }

    #[test]
    fn base_live_router_wires_share_but_omits_restore_until_mobile_job_wrapper() {
        // #89 + #onedrive-mobile 0.9 profile contract: build_live_router wires the live
        // handlers AND share. restore-cloud is attached by the #625 mobile job wrapper,
        // not by the shared base router. share + a live-write route are reached and
        // cap-gated (401, not 404). On mobile share is additionally biometric-gated by
        // the app's with_biometric_gate (not exercised here — this builds the base router only).
        let events = Arc::new(isyncyou_webui::EventBus::new());
        let router = build_live_router(
            Config::default(),
            None,
            events,
            PathBuf::from("/x/isyncyou.toml"),
            Arc::new(AtomicU64::new(5)),
            SharedProgress::new(),
            AgentOperationPolicy::MobileDisabled,
        );
        assert_eq!(
            router
                .route(&ApiRequest::new(
                    "POST",
                    "/api/v1/restore?account=a&service=mail&id=x"
                ))
                .status,
            404,
            "restore-cloud must be absent in the mobile profile"
        );
        assert_eq!(
            router
                .route(&ApiRequest::new("POST", "/api/v1/share"))
                .status,
            401,
            "share must be wired (cap-gated, not absent)"
        );
        assert_eq!(
            router
                .route(&ApiRequest::new("POST", "/api/v1/mail/send"))
                .status,
            401,
            "mail write must be wired (cap-gated, not absent)"
        );
    }

    #[test]
    fn mobile_full_node_router_exposes_gated_restore_and_backup() {
        let root = temp_agent_root("mobile-full-node-router");
        let cfg = Config {
            accounts: vec![isyncyou_core::AccountConfig {
                id: "me".into(),
                username: "me@example.invalid".into(),
                sync_root: root.join("sync"),
                archive_root: root.join("archive"),
                cache_root: root.join("cache"),
                mount_point: None,
            }],
            ..Config::default()
        };
        let events = Arc::new(isyncyou_webui::EventBus::new());
        let gate = Arc::new(Mutex::new(()));
        let jobs = Arc::new(MobileJobRuntime::new(
            cfg.clone(),
            gate.clone(),
            events.clone(),
        ));
        let router = with_mobile_full_node_jobs(
            build_live_router(
                cfg,
                Some(gate),
                events,
                root.join("isyncyou.toml"),
                Arc::new(AtomicU64::new(5)),
                SharedProgress::new(),
                AgentOperationPolicy::MobileFullNode {
                    mobile_jobs: jobs.clone(),
                },
            ),
            jobs,
        );

        for path in [
            "/api/v1/restore?account=me&service=mail&id=m1",
            "/api/v1/backup?account=me&services=mail",
            "/api/v1/jobs?account=me",
            "/api/v1/jobs/cancel?account=me&job_id=job-1",
        ] {
            let method = if path == "/api/v1/jobs?account=me" {
                "GET"
            } else {
                "POST"
            };
            let resp = router.route(&ApiRequest::new(method, path));
            assert_eq!(resp.status, 401, "wired + cap-gated (not 404): {path}");
        }
    }

    #[test]
    fn mobile_connectivity_snapshot_is_one_time_session_and_purpose_bound() {
        let snapshot = isyncyou_agent::AndroidNetworkSnapshot {
            active_network: true,
            internet_capability: true,
            validated_capability: true,
            metered: false,
            restrict_background: isyncyou_agent::RestrictBackgroundStatus::Disabled,
            notifications_visible: true,
            guard_ready: true,
        };
        let id =
            register_mobile_connectivity_snapshot("session-a", "guard-a", "oauth", snapshot, None)
                .expect("snapshot is registered");
        assert!(consume_mobile_connectivity_snapshot(
            &id,
            Some("session-b"),
            isyncyou_agent::ConnectivityPurpose::OAuthStart,
        )
        .is_err());

        let id =
            register_mobile_connectivity_snapshot("session-a", "guard-a", "oauth", snapshot, None)
                .expect("snapshot is registered");
        assert!(consume_mobile_connectivity_snapshot(
            &id,
            Some("session-a"),
            isyncyou_agent::ConnectivityPurpose::TurnStart,
        )
        .is_err());

        let id =
            register_mobile_connectivity_snapshot("session-a", "guard-a", "oauth", snapshot, None)
                .expect("snapshot is registered");
        let consumed = consume_mobile_connectivity_snapshot(
            &id,
            Some("session-a"),
            isyncyou_agent::ConnectivityPurpose::OAuthStart,
        )
        .expect("correct session and purpose consumes the snapshot");
        assert_eq!(consumed.snapshot, snapshot);
        assert_eq!(consumed.forced_observation, None);
        assert!(consume_mobile_connectivity_snapshot(
            &id,
            Some("session-a"),
            isyncyou_agent::ConnectivityPurpose::OAuthStart,
        )
        .is_err());
    }

    #[test]
    fn mobile_connectivity_snapshot_rejects_destroyed_guard() {
        let snapshot = isyncyou_agent::AndroidNetworkSnapshot {
            active_network: true,
            internet_capability: true,
            validated_capability: true,
            metered: false,
            restrict_background: isyncyou_agent::RestrictBackgroundStatus::Disabled,
            notifications_visible: true,
            guard_ready: true,
        };
        let id = register_mobile_connectivity_snapshot(
            "destroyed-session",
            "destroyed-guard",
            "oauth",
            snapshot,
            None,
        )
        .expect("snapshot is registered");

        invalidate_mobile_connectivity_guard("destroyed-guard");

        assert!(consume_mobile_connectivity_snapshot(
            &id,
            Some("destroyed-session"),
            isyncyou_agent::ConnectivityPurpose::OAuthStart,
        )
        .is_err());
    }

    #[test]
    fn connectivity_preflight_mobile_requires_snapshot_before_probe() {
        let agent = DaemonAgent::new(
            Config::default(),
            PathBuf::from("/tmp/issue-640-no-snapshot"),
        );
        let result = isyncyou_webui::AgentHandler::connectivity_preflight_with_session(
            &agent,
            isyncyou_webui::AgentConnectivityPreflightRequest {
                provider: "claude".into(),
                purpose: "oauth_start".into(),
                snapshot_id: None,
            },
            Some("mobile-session"),
        );

        assert_eq!(
            result.unwrap_err(),
            "mobile connectivity snapshot is required"
        );
    }

    #[cfg(feature = "agent-network-device-test-hooks")]
    #[test]
    fn network_snapshot_hook_is_closed_and_one_shot() {
        let snapshot = isyncyou_agent::AndroidNetworkSnapshot {
            active_network: true,
            internet_capability: true,
            validated_capability: true,
            metered: false,
            restrict_background: isyncyou_agent::RestrictBackgroundStatus::Disabled,
            notifications_visible: true,
            guard_ready: true,
        };
        let id = register_mobile_connectivity_snapshot(
            "hook-session",
            "hook-guard",
            "oauth",
            snapshot,
            Some("tls_failed"),
        )
        .expect("closed hook is accepted only in the test build");
        let consumed = consume_mobile_connectivity_snapshot(
            &id,
            Some("hook-session"),
            isyncyou_agent::ConnectivityPurpose::OAuthStart,
        )
        .expect("hook snapshot can be consumed once");
        assert_eq!(
            consumed.forced_observation,
            Some(isyncyou_agent::ProbeObservation::TlsFailed)
        );
        assert!(consume_mobile_connectivity_snapshot(
            &id,
            Some("hook-session"),
            isyncyou_agent::ConnectivityPurpose::OAuthStart,
        )
        .is_err());
        assert!(register_mobile_connectivity_snapshot(
            "hook-session",
            "hook-guard",
            "oauth",
            snapshot,
            Some("arbitrary_endpoint"),
        )
        .is_err());
    }

    #[cfg(feature = "agent-network-device-test-hooks")]
    #[test]
    fn network_snapshot_hook_forces_redacted_preflight_failure_before_transport() {
        let root = apphost_credential_test_root("network-hook-preflight");
        let _ = std::fs::remove_dir_all(&root);
        let agent = DaemonAgent::new(Config::default(), root.clone());
        let snapshot = isyncyou_agent::AndroidNetworkSnapshot {
            active_network: true,
            internet_capability: true,
            validated_capability: true,
            metered: false,
            restrict_background: isyncyou_agent::RestrictBackgroundStatus::Disabled,
            notifications_visible: true,
            guard_ready: true,
        };
        let id = register_mobile_connectivity_snapshot(
            "hook-session-preflight",
            "hook-guard-preflight",
            "oauth",
            snapshot,
            Some("tls_failed"),
        )
        .expect("hook snapshot is registered");
        let response = agent
            .connectivity_preflight_with_session(
                isyncyou_webui::AgentConnectivityPreflightRequest {
                    provider: "claude".into(),
                    purpose: "oauth_start".into(),
                    snapshot_id: Some(id),
                },
                Some("hook-session-preflight"),
            )
            .expect("forced diagnostic returns a closed response");
        assert_eq!(response.status, "unavailable");
        assert_eq!(response.code, "tls_failed");
        assert!(!response.retryable);
        let _ = std::fs::remove_dir_all(root);
    }
}
