//! Shared web-UI router assembly + the live request handlers, reused by the
//! desktop daemon (`isyncyoud`) and the standalone mobile client (#89). The daemon
//! calls [`build_live_router`] for the shared base and adds its daemon-only
//! restore/share/push on top; the mobile client uses the base as-is.

use isyncyou_connectors::ProgressSink;
use isyncyou_core::{Config, OneDriveMode, OneDriveModes};
use isyncyou_store::{Item, Store};
use isyncyou_webui::{OfflineModeRisk, OneDriveMoveRisk};
use std::collections::{BTreeSet, HashMap};
#[cfg(feature = "agent-subscription-experimental")]
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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

/// The daemon's destructive-action handler: re-create an archived item in the
/// cloud using the cached `login --write` (restore-scoped) token.
pub struct DaemonRestore {
    cfg: Config,
}
impl isyncyou_webui::RestoreHandler for DaemonRestore {
    fn restore(&self, account: &str, service: &str, id: &str) -> Result<String, String> {
        // Refuse a not-yet-ledger-migrated service before resolving a token, so the
        // web UI gets the clear "not crash-safe yet" message. (Engine re-checks.)
        if !isyncyou_engine::cloud_restore_service_supported(service) {
            return Err(isyncyou_engine::unsupported_cloud_restore_service_error(
                service,
            ));
        }
        let token = isyncyou_engine::auth::resolve_cached_restore_token(&self.cfg, account)?;
        isyncyou_engine::restore_cloud(&self.cfg, account, service, id, token)
    }
}

/// Fallback read executor for builds without the experimental agent (no store/SQLCipher
/// pull): returns a placeholder so the turn loop still runs in CI/release shapes.
#[cfg(not(feature = "agent-subscription-experimental"))]
struct StubExecutor;
#[cfg(not(feature = "agent-subscription-experimental"))]
impl isyncyou_agent::ToolExecutor for StubExecutor {
    fn execute_read(
        &self,
        _action: &isyncyou_agent::ToolAction,
    ) -> Result<String, isyncyou_agent::AgentError> {
        Ok("{\"note\":\"retrieval needs the agent-subscription-experimental build\"}".to_string())
    }
}

/// Build the read-class tool executor for a turn. The experimental agent build binds the
/// real `StoreArchive` retrieval executor (S-AG.3/#618: search/read/list/export over the
/// encrypted store + on-disk body files for `account` under `archive_root`); other builds
/// get the stub. S-AG.18/#643 is the progressive/deep-search behavior layered on top.
#[cfg(feature = "agent-subscription-experimental")]
fn make_executor(
    account: &str,
    archive_root: std::path::PathBuf,
) -> Box<dyn isyncyou_agent::ToolExecutor + Send> {
    Box::new(isyncyou_agent::retrieval::RetrievalExecutor::new(
        isyncyou_agent::archive::StoreArchive::new(account, archive_root),
    ))
}
#[cfg(not(feature = "agent-subscription-experimental"))]
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
#[cfg(feature = "agent-subscription-experimental")]
const DEFAULT_MODEL: &str = "claude-sonnet-5";

/// The Claude subscription models the in-app switcher offers (id, human label). Each id is
/// verified against the subscription messages API.
#[cfg(feature = "agent-subscription-experimental")]
const CLAUDE_MODELS: &[(&str, &str)] = &[
    ("claude-opus-4-8", "Opus 4.8"),
    ("claude-sonnet-5", "Sonnet 5"),
    ("claude-haiku-4-5-20251001", "Haiku 4.5"),
];
/// The ChatGPT/Codex models the in-app switcher offers (id, human label).
#[cfg(feature = "agent-subscription-experimental")]
const CODEX_MODELS: &[(&str, &str)] = &[("gpt-5.5", "GPT-5.5"), ("gpt-5.4", "GPT-5.4")];

/// A turn-provider builder (a `DaemonAgent` method): given the system prompt, return a
/// boxed provider if its credentials are present.
#[cfg(feature = "agent-subscription-experimental")]
type ProviderBuilder =
    fn(&DaemonAgent, &str) -> Option<Box<dyn isyncyou_agent::LlmProvider + Send>>;
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

struct NotImplementedConfirmedActionExecutor;

impl AgentConfirmedActionExecutor for NotImplementedConfirmedActionExecutor {
    fn execute_confirmed(
        &self,
        action: &isyncyou_agent::ToolAction,
    ) -> Result<ConfirmedActionResult, String> {
        Err(format!(
            "not_implemented: confirmed agent action '{}' lands in S-AG.9/#624",
            action.op()
        ))
    }
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
        format!("account={}", action.account()),
    ];
    if let Some(service) = action.service() {
        parts.push(format!("service={service}"));
    }
    if let Some(item) = action.item_or_target() {
        parts.push(format!("item={item}"));
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
    } else {
        "execution_failed"
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

struct AgentStreamSlot {
    rx: std::sync::mpsc::Receiver<String>,
    created_at_ms: u64,
}

/// The in-app agent handler (S-AG.6/#621). Drives a real turn: the experimental
/// subscription provider when the user has connected an account, otherwise a deterministic
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
    seq: AtomicU64,
    /// Directory holding the operator's local, uncommitted OAuth recipe
    /// (`agent-oauth.json`) and the credential store — the parent of the config file.
    /// Only read by the experimental subscription login (S-AG.12).
    #[cfg_attr(not(feature = "agent-subscription-experimental"), allow(dead_code))]
    oauth_dir: PathBuf,
    /// Tracks in-flight device OAuth logins between start and the browser callback.
    #[cfg(feature = "agent-subscription-experimental")]
    oauth: isyncyou_agent::AgentOAuth,
    #[cfg(test)]
    test_provider_script: Option<TestProviderScript>,
}

impl DaemonAgent {
    pub fn new(cfg: Config, oauth_dir: PathBuf) -> Self {
        let audit_sink = Arc::new(StoreAgentAuditSink { cfg: cfg.clone() });
        Self {
            cfg,
            hub: Arc::new(isyncyou_agent::AgentStreamHub::new()),
            pending: Arc::new(isyncyou_agent::PendingRegistry::new()),
            confirmed_executor: Arc::new(NotImplementedConfirmedActionExecutor),
            audit_sink,
            streams: Mutex::new(std::collections::HashMap::new()),
            seq: AtomicU64::new(0),
            oauth_dir,
            #[cfg(feature = "agent-subscription-experimental")]
            oauth: isyncyou_agent::AgentOAuth::new(),
            #[cfg(test)]
            test_provider_script: None,
        }
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

    /// Pick the turn provider: the connected subscription (experimental feature) when a
    /// token is present, otherwise a deterministic "not connected" message so the UI still
    /// streams a clear instruction instead of erroring.
    fn build_turn_provider(&self, system: &str) -> Box<dyn isyncyou_agent::LlmProvider + Send> {
        #[cfg(test)]
        if let Some(script) = &self.test_provider_script {
            if let Some(script) = script.lock().unwrap().take() {
                return Box::new(isyncyou_agent::FakeProvider::new(script));
            }
        }
        #[cfg(feature = "agent-subscription-experimental")]
        {
            // Provider preference comes from the in-app switcher (persisted), falling back
            // to the env override; either falls back to the other if only one is connected.
            let prefer_codex = self.agent_settings().0 == "codex";
            let (first, second): (ProviderBuilder, ProviderBuilder) = if prefer_codex {
                (Self::try_codex_provider, Self::try_subscription_provider)
            } else {
                (Self::try_subscription_provider, Self::try_codex_provider)
            };
            if let Some(p) = first(self, system).or_else(|| second(self, system)) {
                return p;
            }
        }
        #[cfg(not(feature = "agent-subscription-experimental"))]
        let _ = system;
        Box::new(isyncyou_agent::FakeProvider::new(vec![vec![
            isyncyou_agent::AssistantBlock::Text(
                "The AI assistant isn't connected yet — open the Assistant tab and connect your \
                 Claude account, then try again."
                    .to_string(),
            ),
        ]]))
    }
}

/// The subscription credential we persist on mobile: the access token plus the refresh
/// token and the access token's absolute expiry (ms since the Unix epoch), so the daemon
/// can refresh the access token itself — the desktop `claude` CLI does this for its own
/// `~/.claude/.credentials.json`, but on mobile we own the credential.
#[cfg(feature = "agent-subscription-experimental")]
struct StoredCredential {
    access_token: String,
    refresh_token: String,
    /// Absolute expiry in ms since the Unix epoch; 0 = unknown.
    expires_at_ms: u64,
}

#[cfg(feature = "agent-subscription-experimental")]
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

/// Ms since the Unix epoch (0 on a clock error).
#[cfg(feature = "agent-subscription-experimental")]
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The Codex/ChatGPT credential we persist (access + refresh + ChatGPT account id + expiry).
#[cfg(feature = "agent-subscription-experimental")]
struct CodexStoredCredential {
    access_token: String,
    refresh_token: String,
    account_id: String,
    expires_at_ms: u64,
}

#[cfg(feature = "agent-subscription-experimental")]
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

#[cfg(feature = "agent-subscription-experimental")]
const SUBSCRIPTION_CREDENTIAL_ID: &str = "subscription";

#[cfg(feature = "agent-subscription-experimental")]
const CODEX_CREDENTIAL_ID: &str = "codex";

#[cfg(feature = "agent-subscription-experimental")]
fn credential_store_error(e: impl std::fmt::Display) -> String {
    let raw = e.to_string();
    let redacted = isyncyou_core::obs::redact(&raw);
    if redacted != raw {
        format!("agent credential store error: {redacted}")
    } else {
        "agent credential store error".to_string()
    }
}

#[cfg(feature = "agent-subscription-experimental")]
fn agent_credential_config(oauth_dir: &Path) -> isyncyou_agent::CredentialStoreConfig {
    isyncyou_agent::CredentialStoreConfig::new(oauth_dir)
}

#[cfg(feature = "agent-subscription-experimental")]
fn agent_credential_store(
    oauth_dir: &Path,
) -> Result<isyncyou_agent::AgentCredentialStore, String> {
    isyncyou_agent::CredentialStoreResolver::new(agent_credential_config(oauth_dir))
        .resolve()
        .map_err(credential_store_error)
}

#[cfg(feature = "agent-subscription-experimental")]
fn agent_credential_store_exists(oauth_dir: &Path) -> bool {
    agent_credential_config(oauth_dir).store_dir().exists()
}

#[cfg(feature = "agent-subscription-experimental")]
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

#[cfg(feature = "agent-subscription-experimental")]
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
#[cfg(feature = "agent-subscription-experimental")]
fn store_codex_blob(oauth_dir: &Path, cred: &CodexStoredCredential) -> Result<(), String> {
    store_agent_credential_blob(oauth_dir, CODEX_CREDENTIAL_ID, cred.to_json())
}

/// Minimal percent-decode for the loopback callback query (`+`→space, `%XX`→byte).
#[cfg(feature = "agent-subscription-experimental")]
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

#[cfg(feature = "agent-subscription-experimental")]
const CODEX_OK_HTML: &str = "<!doctype html><meta charset=utf-8><title>ChatGPT connected</title>\
<body style=\"font-family:system-ui;background:#0b0d12;color:#e8eaf0;display:flex;min-height:100vh;\
align-items:center;justify-content:center;margin:0\"><div style=text-align:center><h1>Connected</h1>\
<p style=color:#9aa3b2>ChatGPT is now linked. Close this tab and return to iSyncYou.</p></div>";

#[cfg(feature = "agent-subscription-experimental")]
const CODEX_ERR_HTML: &str = "<!doctype html><meta charset=utf-8><title>Sign-in failed</title>\
<body style=\"font-family:system-ui;background:#0b0d12;color:#e8eaf0;display:flex;min-height:100vh;\
align-items:center;justify-content:center;margin:0\"><div style=text-align:center><h1>Sign-in failed</h1>\
<p style=color:#9aa3b2>Please return to iSyncYou and try connecting ChatGPT again.</p></div>";

/// One-shot loopback callback server for the Codex OAuth (OpenAI registers the fixed
/// `:1455` redirect). Waits for the browser to hit `/auth/callback?code=&state=`, verifies
/// the CSRF `state`, exchanges the code, and persists the credential. Background thread;
/// gives up after 5 minutes.
#[cfg(feature = "agent-subscription-experimental")]
fn codex_callback_serve(
    listener: std::net::TcpListener,
    oauth_dir: std::path::PathBuf,
    cfg: isyncyou_agent::oauth::CodexOAuthConfig,
    verifier: String,
    want_state: String,
) {
    use std::io::{Read, Write};
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    for stream in listener.incoming() {
        if std::time::Instant::now() > deadline {
            break;
        }
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(15)));
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        let target = req
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("");
        if !target.starts_with("/auth/callback") {
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
        let mut dbg = format!(
            "target={}\nstate_match={}\ncode_len={}\n",
            &target[..target.len().min(120)],
            state == want_state,
            code.len()
        );
        // Diagnostic: raw TCP connect from THIS app process (uid) to key hosts, to separate a
        // routing/connect block from a TLS/fingerprint stall.
        for (label, addr) in [
            ("cf_104", "104.18.41.241:443"),
            ("cf_172", "172.64.146.15:443"),
            ("google_8888", "8.8.8.8:443"),
            ("anthropic", "160.79.104.10:443"),
        ] {
            if let Ok(sa) = addr.parse::<std::net::SocketAddr>() {
                let r =
                    std::net::TcpStream::connect_timeout(&sa, std::time::Duration::from_secs(5));
                dbg.push_str(&format!(
                    "tcp {label} = {}\n",
                    match r {
                        Ok(_) => "OK".to_string(),
                        Err(e) => e.to_string(),
                    }
                ));
            }
        }
        let ok = if state == want_state && !code.is_empty() {
            let doh = isyncyou_agent::http::doh_resolve("auth.openai.com");
            match &doh {
                Ok(ips) => dbg.push_str(&format!("doh_ips={ips:?}\n")),
                Err(e) => dbg.push_str(&format!("doh_err={e}\n")),
            }
            let mut ips = doh.unwrap_or_default();
            if ips.is_empty() {
                // Stable Cloudflare anycast IPs for auth.openai.com — used when this network
                // blocks the app from reaching any DoH resolver.
                ips = vec![
                    std::net::IpAddr::from([104, 18, 41, 241]),
                    std::net::IpAddr::from([172, 64, 146, 15]),
                ];
                dbg.push_str("using hardcoded auth.openai.com IPs\n");
            }
            match isyncyou_agent::http::HttpTransport::new_resolving("auth.openai.com", &ips)
                .map_err(|e| e.to_string())
                .and_then(|http| {
                    isyncyou_agent::oauth::codex_exchange(&http, &cfg, &code, &verifier)
                        .map_err(|e| e.to_string())
                }) {
                Ok(tok) => {
                    dbg.push_str(&format!(
                        "exchange=OK account_id={}\n",
                        if tok.account_id.is_empty() {
                            "EMPTY"
                        } else {
                            "present"
                        }
                    ));
                    let expires_at_ms = if tok.expires_in > 0 {
                        now_ms() + tok.expires_in * 1000
                    } else {
                        0
                    };
                    store_codex_blob(
                        &oauth_dir,
                        &CodexStoredCredential {
                            access_token: tok.access_token,
                            refresh_token: tok.refresh_token,
                            account_id: tok.account_id,
                            expires_at_ms,
                        },
                    )
                    .is_ok()
                }
                Err(e) => {
                    dbg.push_str(&format!("exchange=ERR {e}\n"));
                    false
                }
            }
        } else {
            dbg.push_str("skipped: state/code check failed\n");
            false
        };
        let _ = std::fs::write(oauth_dir.join("codex-debug.txt"), &dbg);
        let body = if ok { CODEX_OK_HTML } else { CODEX_ERR_HTML };
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes());
        return;
    }
}

/// EXPERIMENTAL subscription device-OAuth (S-AG.12) — only compiled with
/// `agent-subscription-experimental`. The operator's recipe (endpoints/client_id) and
/// the obtained token both live locally; nothing provider-specific is hardcoded.
#[cfg(feature = "agent-subscription-experimental")]
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

    /// The OAuth recipe: the in-repo Claude default, with optional local overrides from
    /// `agent-oauth.json` next to the config (the recipe may now live in-repo, so no file
    /// is required for the default Claude flow to work).
    fn load_oauth_config(&self) -> Result<isyncyou_agent::OAuthConfig, String> {
        let path = self.oauth_dir.join("agent-oauth.json");
        if path.exists() {
            let s = std::fs::read_to_string(&path).map_err(|e| format!("OAuth recipe: {e}"))?;
            serde_json::from_str(&s).map_err(|e| format!("OAuth recipe is invalid JSON: {e}"))
        } else {
            Ok(isyncyou_agent::OAuthConfig::default())
        }
    }

    /// Persist a subscription credential (access + refresh + expiry) at rest under a
    /// device-local key, so the daemon can refresh the access token itself.
    fn store_credential(&self, cred: &StoredCredential) -> Result<(), String> {
        store_agent_credential_blob(&self.oauth_dir, SUBSCRIPTION_CREDENTIAL_ID, cred.to_json())
    }

    /// Persist the FULL token set from the OAuth code exchange (access + refresh + expiry) so
    /// `fresh_access_token` can self-refresh before the ~8h subscription token expires
    /// (LIVE-verified 2026-07-01 — without the refresh token the client "connection-lost"s
    /// every ~8h with no way to renew).
    #[cfg(feature = "agent-subscription-experimental")]
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
        match provider {
            "codex" => isyncyou_agent::CodexConfig::default().model,
            _ => {
                std::env::var("ISYNCYOU_AGENT_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string())
            }
        }
    }

    /// Persist the switcher selection after validating it against the offered models.
    fn set_agent_settings(&self, provider: &str, model: &str) -> Result<(), String> {
        let known = match provider {
            "claude" => CLAUDE_MODELS,
            "codex" => CODEX_MODELS,
            _ => return Err("unknown provider".into()),
        };
        if !known.iter().any(|(id, _)| *id == model) {
            return Err("unknown model for provider".into());
        }
        std::fs::create_dir_all(&self.oauth_dir).map_err(|e| e.to_string())?;
        let blob = serde_json::to_vec(&serde_json::json!({
            "provider": provider,
            "model": model,
        }))
        .map_err(|e| e.to_string())?;
        std::fs::write(self.oauth_dir.join("agent-settings.json"), blob).map_err(|e| e.to_string())
    }

    /// The subscription access token: our stored token (mobile, from the device OAuth
    /// callback) first, else the existing `claude` CLI login on desktop
    /// (`~/.claude/.credentials.json` → `claudeAiOauth.accessToken`). Never logged.
    fn subscription_token(&self) -> Option<String> {
        if agent_credential_store_exists(&self.oauth_dir) {
            if let Ok(Some(secret)) =
                load_agent_credential_blob(&self.oauth_dir, SUBSCRIPTION_CREDENTIAL_ID)
            {
                let raw = secret.expose();
                // Newer format: a JSON credential blob (access + refresh + expiry). Older
                // format (pre-refresh): the bare access token as UTF-8.
                let cred = StoredCredential::from_json(raw).unwrap_or_else(|| StoredCredential {
                    access_token: std::str::from_utf8(raw).unwrap_or("").to_string(),
                    refresh_token: String::new(),
                    expires_at_ms: 0,
                });
                return self.fresh_access_token(cred);
            }
        }
        // Desktop: the existing `claude` CLI login, which the CLI keeps refreshed.
        let home = std::env::var_os("HOME")?;
        let data =
            std::fs::read_to_string(PathBuf::from(home).join(".claude/.credentials.json")).ok()?;
        let v: serde_json::Value = serde_json::from_str(&data).ok()?;
        v.get("claudeAiOauth")?
            .get("accessToken")?
            .as_str()
            .map(|s| s.to_string())
    }

    /// Return a usable access token from a stored credential, refreshing it first if it is
    /// expired (or within a small margin) and we hold a refresh token. On a successful
    /// refresh the rotated credential is persisted so the next call is cheap.
    fn fresh_access_token(&self, cred: StoredCredential) -> Option<String> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        // 5-minute margin so a turn never starts on a token about to expire mid-flight.
        let near_expiry = cred.expires_at_ms != 0 && cred.expires_at_ms <= now_ms + 5 * 60 * 1000;
        if !cred.refresh_token.is_empty() && (near_expiry || cred.access_token.is_empty()) {
            if let Ok(cfg) = self.load_oauth_config() {
                if let Ok(http) = isyncyou_agent::http::HttpTransport::new() {
                    if let Ok(t) = isyncyou_agent::oauth::refresh(&http, &cfg, &cred.refresh_token)
                    {
                        let expires_at_ms = if t.expires_in > 0 {
                            now_ms + t.expires_in * 1000
                        } else {
                            0
                        };
                        let _ = self.store_credential(&StoredCredential {
                            access_token: t.access_token.clone(),
                            refresh_token: t.refresh_token,
                            expires_at_ms,
                        });
                        return Some(t.access_token);
                    }
                }
            }
        }
        if cred.access_token.is_empty() {
            None
        } else {
            Some(cred.access_token)
        }
    }

    /// The subscription config: the in-repo recipe + (on desktop) the account identity from
    /// `~/.claude.json` for `metadata.user_id`.
    fn subscription_config(&self) -> isyncyou_agent::SubscriptionConfig {
        let mut cfg = isyncyou_agent::SubscriptionConfig::default();
        if let Some(home) = std::env::var_os("HOME") {
            if let Ok(data) = std::fs::read_to_string(PathBuf::from(home).join(".claude.json")) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) {
                    if let Some(a) = v
                        .get("oauthAccount")
                        .and_then(|o| o.get("accountUuid"))
                        .and_then(|x| x.as_str())
                    {
                        cfg.account_uuid = a.to_string();
                    }
                    if let Some(d) = v.get("userID").and_then(|x| x.as_str()) {
                        cfg.device_id = d.to_string();
                    }
                }
            }
        }
        cfg
    }

    /// Build the subscription provider if a token is available (else None → fallback).
    fn try_subscription_provider(
        &self,
        system: &str,
    ) -> Option<Box<dyn isyncyou_agent::LlmProvider + Send>> {
        let token = self.subscription_token()?;
        let p = isyncyou_agent::SubscriptionProvider::new(
            token,
            self.model_for("claude"),
            system,
            self.subscription_config(),
        )
        .ok()?;
        Some(Box::new(p))
    }

    /// ChatGPT/Codex credentials: the existing `codex` CLI login on desktop
    /// (`~/.codex/auth.json` → tokens.access_token + account_id). Never logged.
    fn codex_credentials(&self) -> Option<(String, String)> {
        // Mobile: a device-logged-in Codex credential in the store, refreshed if expired.
        if agent_credential_store_exists(&self.oauth_dir) {
            if let Ok(Some(secret)) =
                load_agent_credential_blob(&self.oauth_dir, CODEX_CREDENTIAL_ID)
            {
                if let Some(cred) = CodexStoredCredential::from_json(secret.expose()) {
                    return self.fresh_codex_credential(cred);
                }
            }
        }
        // Desktop: the existing `codex` CLI login (`~/.codex/auth.json`).
        let home = std::env::var_os("HOME")?;
        let data = std::fs::read_to_string(PathBuf::from(home).join(".codex/auth.json")).ok()?;
        let v: serde_json::Value = serde_json::from_str(&data).ok()?;
        let t = v.get("tokens")?;
        let token = t.get("access_token")?.as_str()?.to_string();
        let account = t
            .get("account_id")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        if token.is_empty() {
            return None;
        }
        Some((token, account))
    }

    /// Usable Codex creds from a stored credential, refreshing first if expired (5-min
    /// margin). The refresh response may omit the id_token → keep the stored account id.
    fn fresh_codex_credential(&self, cred: CodexStoredCredential) -> Option<(String, String)> {
        let now = now_ms();
        let near_expiry = cred.expires_at_ms != 0 && cred.expires_at_ms <= now + 5 * 60 * 1000;
        if !cred.refresh_token.is_empty() && (near_expiry || cred.access_token.is_empty()) {
            let cfg = isyncyou_agent::oauth::CodexOAuthConfig::default();
            let mut ips = isyncyou_agent::http::doh_resolve("auth.openai.com").unwrap_or_default();
            if ips.is_empty() {
                ips = vec![
                    std::net::IpAddr::from([104, 18, 41, 241]),
                    std::net::IpAddr::from([172, 64, 146, 15]),
                ];
            }
            if let Ok(http) =
                isyncyou_agent::http::HttpTransport::new_resolving("auth.openai.com", &ips)
            {
                if let Ok(tok) =
                    isyncyou_agent::oauth::codex_refresh(&http, &cfg, &cred.refresh_token)
                {
                    let account_id = if tok.account_id.is_empty() {
                        cred.account_id.clone()
                    } else {
                        tok.account_id.clone()
                    };
                    let expires_at_ms = if tok.expires_in > 0 {
                        now + tok.expires_in * 1000
                    } else {
                        0
                    };
                    let _ = store_codex_blob(
                        &self.oauth_dir,
                        &CodexStoredCredential {
                            access_token: tok.access_token.clone(),
                            refresh_token: tok.refresh_token,
                            account_id: account_id.clone(),
                            expires_at_ms,
                        },
                    );
                    return Some((tok.access_token, account_id));
                }
            }
        }
        if cred.access_token.is_empty() {
            None
        } else {
            Some((cred.access_token, cred.account_id))
        }
    }

    /// EXPERIMENTAL (S-AG.12). Start the Codex/ChatGPT device OAuth: bind OpenAI's fixed
    /// loopback port, spawn a one-shot callback server (exchanges + stores on success),
    /// and return the authorize URL for the system browser. The app polls
    /// `/api/v1/agent/status` for `codex:true`.
    fn codex_oauth_start(&self) -> Result<String, String> {
        let cfg = isyncyou_agent::oauth::CodexOAuthConfig::default();
        let (verifier, challenge) = isyncyou_agent::oauth::pkce().map_err(|e| e.to_string())?;
        let state = isyncyou_agent::oauth::rand_state().map_err(|e| e.to_string())?;
        let url = isyncyou_agent::oauth::codex_build_authorize_url(&cfg, &challenge, &state);
        // Bind OpenAI's registered redirect port up front (fail early if busy).
        let listener = std::net::TcpListener::bind(("127.0.0.1", 1455)).map_err(|e| {
            format!("could not open the ChatGPT sign-in port :1455 ({e}) — is another login already running?")
        })?;
        let oauth_dir = self.oauth_dir.clone();
        std::thread::spawn(move || codex_callback_serve(listener, oauth_dir, cfg, verifier, state));
        Ok(url)
    }

    /// Build the Codex (ChatGPT) provider if credentials are available.
    fn try_codex_provider(
        &self,
        instructions: &str,
    ) -> Option<Box<dyn isyncyou_agent::LlmProvider + Send>> {
        let (token, account) = self.codex_credentials()?;
        let cfg = isyncyou_agent::CodexConfig {
            account_id: account,
            model: self.model_for("codex"),
            ..Default::default()
        };
        let p = isyncyou_agent::CodexProvider::new(token, instructions, cfg).ok()?;
        Some(Box::new(p))
    }
}

impl isyncyou_webui::AgentHandler for DaemonAgent {
    fn start_turn(&self, account: &str, prompt: &str) -> Result<String, String> {
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
        // Build the provider on this thread (it may read the local token), then run the
        // turn on a background thread streaming events into the hub.
        let hub = self.hub.clone();
        let tid = turn_id.clone();
        let system = format!("{AGENT_SYSTEM_PROMPT}\n\nActive account: {account}.");
        let prompt = prompt.to_string();
        // Resolve the account's archive root now (reads config on this thread), so the
        // turn thread can build the real store-backed retrieval executor for it.
        let account_id = account.to_string();
        let archive_root = self.archive_root_for(&account_id);
        let mut provider = self.build_turn_provider(&system);
        let pending = self.pending.clone();
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
            match outcome {
                Ok(isyncyou_agent::TurnOutcome::Final { .. }) => {}
                Ok(isyncyou_agent::TurnOutcome::PendingConfirmation {
                    action, preview, ..
                }) => {
                    match pending.register(action, preview, unix_now_ms(), AGENT_CONFIRM_TTL_MS) {
                        Ok((pending_action, token)) => {
                            let _ = hub.emit(
                                &tid,
                                isyncyou_agent::StreamEvent::ConfirmationRequired {
                                    id: pending_action.id,
                                    action: pending_action.action,
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
                            let _ =
                                hub.emit(&tid, isyncyou_agent::StreamEvent::Error(e.to_string()));
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
                    let _ = hub.emit(&tid, isyncyou_agent::StreamEvent::Error(e.to_string()));
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

    fn confirm(&self, pending_id: &str, token: &str, action_hash: &str) -> Result<String, String> {
        let action = self
            .pending
            .confirm(pending_id, token, action_hash, unix_now_ms())
            .map_err(|e| format!("{e:?}"))?;
        let action_summary = agent_action_summary(&action);
        self.audit_sink
            .record_confirm(&action, "started", &action_summary)?;
        match self.confirmed_executor.execute_confirmed(&action) {
            Ok(result) => {
                self.audit_sink
                    .record_confirm(&action, "ok", &format!("{action_summary} ok"))?;
                serde_json::to_string(&serde_json::json!({
                    "status": "ok",
                    "op": action.op(),
                    "summary": result.summary,
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

    /// EXPERIMENTAL (S-AG.12). Begin the MANUAL device OAuth login: PKCE + state, with the
    /// manual (copy-paste) redirect — claude.ai shows a code instead of redirecting to a
    /// loopback server. The app opens the returned URL in the system browser. Robust on
    /// mobile (no loopback host/port/IPv6 fragility).
    #[cfg(feature = "agent-subscription-experimental")]
    fn oauth_start(&self, provider: &str, redirect_uri: &str) -> Result<String, String> {
        if provider == "codex" {
            return self.codex_oauth_start();
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
        Ok(started.authorize_url)
    }

    /// EXPERIMENTAL (S-AG.12). Complete the MANUAL login: the operator pastes the
    /// `code#state` shown by claude.ai. Look up the PKCE verifier by state, exchange, and
    /// persist the token.
    #[cfg(feature = "agent-subscription-experimental")]
    fn oauth_complete(&self, pasted: &str) -> Result<String, String> {
        let (code, state_opt) = isyncyou_agent::oauth::parse_pasted_code(pasted);
        let state = state_opt.ok_or("the pasted code is missing its #state part")?;
        let (verifier, redirect_uri) = self
            .oauth
            .take(&state)
            .ok_or("unknown or expired login — start the login again")?;
        let cfg = self.load_oauth_config()?;
        let http = isyncyou_agent::http::HttpTransport::new().map_err(|e| e.to_string())?;
        let token =
            isyncyou_agent::oauth::exchange(&http, &cfg, &code, &verifier, &redirect_uri, &state)
                .map_err(|e| e.to_string())?;
        self.store_token(&token)?;
        Ok("connected".to_string())
    }

    /// EXPERIMENTAL (S-AG.12). Import a subscription credential obtained on another device
    /// (e.g. the desktop `claude` login, where the OAuth consent works) so this device can
    /// run + self-refresh it. Session/cap gated at the router; the credential is stored
    /// encrypted at rest exactly like a device-OAuth result.
    #[cfg(feature = "agent-subscription-experimental")]
    fn subscription_import(
        &self,
        access: &str,
        refresh: &str,
        expires_at_ms: u64,
    ) -> Result<(), String> {
        if access.is_empty() {
            return Err("access token is required".into());
        }
        self.store_credential(&StoredCredential {
            access_token: access.to_string(),
            refresh_token: refresh.to_string(),
            expires_at_ms,
        })
    }

    /// EXPERIMENTAL (S-AG.12). The loopback callback path (kept for the auto flow); exchange
    /// the code with the stored verifier + state and persist the token, then show a page.
    #[cfg(feature = "agent-subscription-experimental")]
    fn oauth_callback(&self, code: &str, state: &str) -> Result<String, String> {
        let (verifier, redirect_uri) = self
            .oauth
            .take(state)
            .ok_or("unknown or expired login state")?;
        let cfg = self.load_oauth_config()?;
        let http = isyncyou_agent::http::HttpTransport::new().map_err(|e| e.to_string())?;
        let token =
            isyncyou_agent::oauth::exchange(&http, &cfg, code, &verifier, &redirect_uri, state)
                .map_err(|e| e.to_string())?;
        self.store_token(&token)?;
        Ok(Self::OAUTH_SUCCESS_HTML.to_string())
    }

    #[cfg(feature = "agent-subscription-experimental")]
    fn status_json(&self) -> String {
        let claude = self.subscription_token().is_some();
        let codex = self.codex_credentials().is_some();
        let (sel_provider, _) = self.agent_settings();
        // Effective provider: the selection if it is connected, else whichever is
        // (Claude preferred). A selected+connected Claude is already covered by the
        // `else if claude` arm, so it needs no separate branch.
        let provider = if sel_provider == "codex" && codex {
            "codex"
        } else if claude {
            "claude"
        } else if codex {
            "codex"
        } else {
            ""
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
        serde_json::json!({
            "connected": claude || codex,
            "enabled": true,
            "provider": provider,
            "model": model,
            "claude": claude,
            "codex": codex,
            "models": { "claude": list(CLAUDE_MODELS), "codex": list(CODEX_MODELS) },
        })
        .to_string()
    }

    /// Persist the switcher's provider+model selection (validated against the offered lists).
    #[cfg(feature = "agent-subscription-experimental")]
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
    /// Construct the restore handler (daemon-only; the mobile profile never wires it).
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
) -> isyncyou_webui::Router {
    // The experimental subscription login reads its local recipe + stores its token
    // next to the config file (on mobile that is the app-private filesDir).
    let oauth_dir = config_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
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
            Arc::new(DaemonAgent::new(cfg.clone(), oauth_dir))
                as Arc<dyn isyncyou_webui::AgentHandler>,
            mint_cap_token(),
        )
        // #onedrive-mobile 0.9: outbound sharing is wired here (was daemon-only) so the
        // mobile profile gets it too. On mobile it is additionally biometric-gated (op
        // "share" is in the per-action-token catalogue); the cap token is the CSRF gate.
        // restore-cloud stays daemon-only (excluded on mobile).
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

#[cfg(test)]
mod tests {
    use super::*;
    use isyncyou_webui::ApiRequest;
    use std::sync::{Mutex as StdMutex, OnceLock as StdOnceLock};

    static ENVELOPE_REQUIREMENT_TEST_LOCK: StdOnceLock<StdMutex<()>> = StdOnceLock::new();
    #[cfg(feature = "agent-subscription-experimental")]
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

    #[cfg(feature = "agent-subscription-experimental")]
    struct AppHostCredentialEnvGuard {
        _guard: std::sync::MutexGuard<'static, ()>,
        home: Option<std::ffi::OsString>,
        cred_key: Option<std::ffi::OsString>,
    }

    #[cfg(feature = "agent-subscription-experimental")]
    impl AppHostCredentialEnvGuard {
        fn new() -> Self {
            let guard = APP_HOST_CREDENTIAL_ENV_TEST_LOCK
                .get_or_init(|| StdMutex::new(()))
                .lock()
                .unwrap();
            let home = std::env::var_os("HOME");
            let cred_key = std::env::var_os("ISYNCYOU_AGENT_CRED_KEY");
            std::env::remove_var("ISYNCYOU_AGENT_CRED_KEY");
            Self {
                _guard: guard,
                home,
                cred_key,
            }
        }

        fn set_home(&self, home: &Path) {
            std::env::set_var("HOME", home);
        }
    }

    #[cfg(feature = "agent-subscription-experimental")]
    impl Drop for AppHostCredentialEnvGuard {
        fn drop(&mut self) {
            match &self.home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match &self.cred_key {
                Some(value) => std::env::set_var("ISYNCYOU_AGENT_CRED_KEY", value),
                None => std::env::remove_var("ISYNCYOU_AGENT_CRED_KEY"),
            }
        }
    }

    #[cfg(feature = "agent-subscription-experimental")]
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

    #[cfg(feature = "agent-subscription-experimental")]
    #[test]
    fn experimental_agent_executor_reads_store_archive_fixture() {
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

    #[cfg(feature = "agent-subscription-experimental")]
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

    #[cfg(feature = "agent-subscription-experimental")]
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

    #[cfg(feature = "agent-subscription-experimental")]
    #[test]
    fn credential_store_preferred_over_desktop_cli_fallback() {
        let env = AppHostCredentialEnvGuard::new();
        let root = apphost_credential_test_root("credential-preference");
        let _ = std::fs::remove_dir_all(&root);
        let oauth_dir = root.join("oauth");
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".claude/.credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"desktop-claude-token"}}"#,
        )
        .unwrap();
        std::fs::write(
            home.join(".codex/auth.json"),
            r#"{"tokens":{"access_token":"desktop-codex-token","account_id":"desktop-account"}}"#,
        )
        .unwrap();
        env.set_home(&home);

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

        assert_eq!(
            agent.subscription_token().as_deref(),
            Some("stored-claude-token")
        );
        assert_eq!(
            agent.codex_credentials(),
            Some(("stored-codex-token".into(), "stored-account".into()))
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(feature = "agent-subscription-experimental")]
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
    fn mobile_live_router_wires_share_but_omits_restore() {
        // #89 + #onedrive-mobile 0.9 profile contract: build_live_router wires the live
        // handlers AND (now) share, but NOT the daemon-only restore-cloud. restore POSTs
        // are refused 404 (absent); share + a live-write route are reached and cap-gated
        // (401, not 404). On mobile share is additionally biometric-gated by the app's
        // with_biometric_gate (not exercised here — this builds the base router only).
        let events = Arc::new(isyncyou_webui::EventBus::new());
        let router = build_live_router(
            Config::default(),
            None,
            events,
            PathBuf::from("/x/isyncyou.toml"),
            Arc::new(AtomicU64::new(5)),
            SharedProgress::new(),
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
}
