//! Shared web-UI router assembly + the live request handlers, reused by the
//! desktop daemon (`isyncyoud`) and the standalone mobile client (#89). The daemon
//! calls [`build_live_router`] for the shared base and adds its daemon-only
//! restore/share/push on top; the mobile client uses the base as-is.

use isyncyou_core::Config;
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

/// A read-class tool executor placeholder until S-AG.8/#623 wires the real archive
/// retrieval executor. A canned text turn never calls it.
struct StubExecutor;
impl isyncyou_agent::ToolExecutor for StubExecutor {
    fn execute_read(
        &self,
        _action: &isyncyou_agent::ToolAction,
    ) -> Result<String, isyncyou_agent::AgentError> {
        Ok("{\"note\":\"retrieval is wired in S-AG.8/#623\"}".to_string())
    }
}

/// Serialize one stream event to a single-line JSON SSE-data payload.
fn agent_event_json(ev: &isyncyou_agent::StreamEvent) -> String {
    use isyncyou_agent::StreamEvent as E;
    let v = match ev {
        E::Token(t) => serde_json::json!({ "event": "token", "text": t }),
        E::ToolCall { id, name, input } => {
            serde_json::json!({ "event": "tool_call", "id": id, "name": name, "input": input })
        }
        E::ToolResult { id, content, untrusted } => serde_json::json!({
            "event": "tool_result", "id": id, "content": content, "untrusted": untrusted
        }),
        E::ConfirmationRequired { id, preview, .. } => {
            serde_json::json!({ "event": "confirmation_required", "tool_id": id, "preview": preview })
        }
        E::Error(e) => serde_json::json!({ "event": "error", "message": e }),
        E::Done => serde_json::json!({ "event": "done" }),
    };
    v.to_string()
}

/// The in-app agent handler (S-AG.6/#621). This foundation drives a deterministic
/// `FakeProvider` turn (no real LLM token); official providers + real retrieval land in
/// S-AG.8/#623 and operations execution in S-AG.9/#624. It owns the stream hub and the
/// pending-action registry, so the model never holds a capability token.
pub struct DaemonAgent {
    #[allow(dead_code)] // wired into the real retrieval/restore path in #623/#624
    cfg: Config,
    hub: Arc<isyncyou_agent::AgentStreamHub>,
    pending: Arc<isyncyou_agent::PendingRegistry>,
    streams: Mutex<std::collections::HashMap<String, std::sync::mpsc::Receiver<String>>>,
    seq: AtomicU64,
    /// Directory holding the operator's local, uncommitted OAuth recipe
    /// (`agent-oauth.json`) and the credential store — the parent of the config file.
    /// Only read by the experimental subscription login (S-AG.12).
    #[cfg_attr(
        not(feature = "agent-subscription-experimental"),
        allow(dead_code)
    )]
    oauth_dir: PathBuf,
    /// Tracks in-flight device OAuth logins between start and the browser callback.
    #[cfg(feature = "agent-subscription-experimental")]
    oauth: isyncyou_agent::AgentOAuth,
}

impl DaemonAgent {
    pub fn new(cfg: Config, oauth_dir: PathBuf) -> Self {
        Self {
            cfg,
            hub: Arc::new(isyncyou_agent::AgentStreamHub::new()),
            pending: Arc::new(isyncyou_agent::PendingRegistry::new()),
            streams: Mutex::new(std::collections::HashMap::new()),
            seq: AtomicU64::new(0),
            oauth_dir,
            #[cfg(feature = "agent-subscription-experimental")]
            oauth: isyncyou_agent::AgentOAuth::new(),
        }
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

    /// Load the operator's local OAuth recipe (uncommitted).
    fn load_oauth_config(&self) -> Result<isyncyou_agent::OAuthConfig, String> {
        let path = self.oauth_dir.join("agent-oauth.json");
        let s = std::fs::read_to_string(&path).map_err(|e| {
            format!(
                "OAuth recipe not found at {} — the operator must place it locally: {e}",
                path.display()
            )
        })?;
        serde_json::from_str(&s).map_err(|e| format!("OAuth recipe is invalid JSON: {e}"))
    }

    /// Persist the obtained access token at rest under a device-local key.
    fn store_token(&self, token: &str) -> Result<(), String> {
        let dir = self.oauth_dir.join("agent-credentials");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let key = isyncyou_agent::LocalKey::new(self.oauth_dir.join("agent-credentials.key"));
        let store = isyncyou_agent::CredentialStore::new(dir, key);
        store
            .put(
                isyncyou_agent::SecretClass::ProviderOAuthRefresh,
                "subscription",
                &isyncyou_agent::Secret::new(token.as_bytes().to_vec()),
            )
            .map_err(|e| e.to_string())
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
        self.streams.lock().unwrap().insert(turn_id.clone(), rx_str);
        // Run a deterministic FakeProvider turn on a background thread (no real token).
        let hub = self.hub.clone();
        let tid = turn_id.clone();
        let account = account.to_string();
        let prompt = prompt.to_string();
        std::thread::spawn(move || {
            let answer = format!(
                "iSyncYou agent foundation is live (account {account}). You said: {prompt}\n\
                 The official model + archive retrieval arrive in S-AG.8/#623."
            );
            let mut provider = isyncyou_agent::FakeProvider::new(vec![vec![
                isyncyou_agent::AssistantBlock::Text(answer),
            ]]);
            let exec = StubExecutor;
            let mut history = vec![isyncyou_agent::Message::user(prompt)];
            let _ = isyncyou_agent::run_turn(&mut provider, &exec, &mut history, &mut |ev| {
                hub.emit(&tid, ev);
            });
            hub.close(&tid);
        });
        Ok(turn_id)
    }

    fn confirm(&self, pending_id: &str, token: &str) -> Result<String, String> {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        match self.pending.confirm(pending_id, token, now_ms) {
            Ok(action) => Ok(format!(
                "confirmed {} (execution lands in S-AG.9/#624)",
                action.op()
            )),
            Err(e) => Err(format!("{e:?}")),
        }
    }

    fn cancel(&self, turn_id: &str) {
        self.hub.cancel(turn_id);
    }

    fn open_stream(&self, turn_id: &str) -> Option<std::sync::mpsc::Receiver<String>> {
        self.streams.lock().unwrap().remove(turn_id)
    }

    /// EXPERIMENTAL (S-AG.12). Begin a device OAuth login: PKCE + state from the local
    /// recipe; the app opens the returned URL in the system browser. Only present with
    /// the feature; otherwise the trait default returns "not enabled".
    #[cfg(feature = "agent-subscription-experimental")]
    fn oauth_start(&self, _provider: &str, redirect_uri: &str) -> Result<String, String> {
        let cfg = self.load_oauth_config()?;
        let started = self
            .oauth
            .start(&cfg, redirect_uri)
            .map_err(|e| e.to_string())?;
        Ok(started.authorize_url)
    }

    /// EXPERIMENTAL (S-AG.12). The system browser returns here; exchange the code with
    /// the stored PKCE verifier and persist the token, then show a success page.
    #[cfg(feature = "agent-subscription-experimental")]
    fn oauth_callback(&self, code: &str, state: &str) -> Result<String, String> {
        let (verifier, redirect_uri) = self
            .oauth
            .take(state)
            .ok_or("unknown or expired login state")?;
        let cfg = self.load_oauth_config()?;
        let http = isyncyou_agent::http::HttpTransport::new().map_err(|e| e.to_string())?;
        let token = isyncyou_agent::oauth::exchange(&http, &cfg, code, &verifier, &redirect_uri)
            .map_err(|e| e.to_string())?;
        self.store_token(&token)?;
        Ok(Self::OAUTH_SUCCESS_HTML.to_string())
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

/// Web-UI outbound sharing (#494): create a sharing link for a OneDrive item by id
/// using the cached write token (`Files.ReadWrite`). Only OneDrive drive items are
/// shareable via `createLink`.
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
        let token = isyncyou_engine::auth::resolve_cached_sync_token(&self.cfg, account)?;
        isyncyou_graph::GraphClient::new(token)
            .create_link(id, link_type, scope, None, None, None)
            .map_err(|e| e.to_string())
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
        let roles: &[&str] = if role == "write" {
            &["write"]
        } else {
            &["read"]
        };
        let token = isyncyou_engine::auth::resolve_cached_sync_token(&self.cfg, account)?;
        // Invite named people: require sign-in + send the invitation email.
        isyncyou_graph::GraphClient::new(token)
            .invite(id, emails, roles, true, true, "", None, None)
            .map(|ids| {
                format!(
                    "invited {} recipient(s) ({role})",
                    emails.len().max(ids.len())
                )
            })
            .map_err(|e| e.to_string())
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
pub fn build_live_router(
    cfg: Config,
    gate: Option<Arc<Mutex<()>>>,
    events: Arc<isyncyou_webui::EventBus>,
    config_path: PathBuf,
    live_interval: Arc<AtomicU64>,
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
        .with_verify(
            Arc::new(DaemonVerify { cfg: cfg.clone() }),
            mint_cap_token(),
        )
        .with_settings(
            Arc::new(DaemonSettings {
                config_path,
                live_interval,
            }),
            mint_cap_token(),
        )
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
            Arc::new(DaemonAgent::new(cfg.clone(), oauth_dir)) as Arc<dyn isyncyou_webui::AgentHandler>,
            mint_cap_token(),
        )
        .with_events(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use isyncyou_webui::ApiRequest;

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

    #[test]
    fn mobile_live_router_omits_restore_and_share() {
        // #89 profile contract: build_live_router wires the live handlers but NOT the
        // daemon-only restore/share. POSTs to those routes are refused 404 (handler
        // absent); a wired live-write route is reached and cap-gated (401, not 404).
        let events = Arc::new(isyncyou_webui::EventBus::new());
        let router = build_live_router(
            Config::default(),
            None,
            events,
            PathBuf::from("/x/isyncyou.toml"),
            Arc::new(AtomicU64::new(5)),
        );
        assert_eq!(
            router
                .route(&ApiRequest::new(
                    "POST",
                    "/api/v1/restore?account=a&service=mail&id=x"
                ))
                .status,
            404,
            "restore must be absent in the mobile profile"
        );
        assert_eq!(
            router
                .route(&ApiRequest::new("POST", "/api/v1/share"))
                .status,
            404,
            "share must be absent in the mobile profile"
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
