use std::sync::{Arc, Mutex};

use isyncyou_core::Config;
use isyncyou_store::Store;

use crate::{AgentConfirmedActionExecutor, ConfirmedActionResult};

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
use std::io::Write;
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
use std::path::{Path, PathBuf};
#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
use std::sync::atomic::{AtomicU64, Ordering};

/// Explicitly separates desktop Agent operation execution from the shared mobile router.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentOperationPolicy {
    DesktopEnabled,
    MobileDisabled,
}

pub(crate) fn confirmed_executor_for_policy(
    policy: AgentOperationPolicy,
    cfg: Config,
    gate: Arc<Mutex<()>>,
) -> Arc<dyn AgentConfirmedActionExecutor> {
    match policy {
        AgentOperationPolicy::DesktopEnabled => Arc::new(DesktopAgentOperations::new(cfg, gate)),
        AgentOperationPolicy::MobileDisabled => Arc::new(MobileDisabledAgentOperations),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingOperationPreview {
    pub(crate) text: String,
    pub(crate) risk: String,
}

impl PendingOperationPreview {
    fn destructive(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            risk: "destructive".to_string(),
        }
    }
}

const BACKUP_SERVICES: &[&str] = &["mail", "calendar", "contacts", "todo", "onenote"];
const RESTORE_CLOUD_SERVICES: &[&str] = &["mail", "calendar", "contacts", "todo", "onenote"];

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BackupDelta {
    pub mail: u64,
    pub calendar: u64,
    pub contacts: u64,
    pub todo: u64,
    pub onenote: u64,
}

impl BackupDelta {
    pub fn total(&self) -> u64 {
        self.mail + self.calendar + self.contacts + self.todo + self.onenote
    }

    /// A short human notification body, or `None` when nothing new was archived.
    pub fn notification(&self) -> Option<String> {
        if self.total() == 0 {
            return None;
        }
        let one_or_many =
            |n: u64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let mut parts = Vec::new();
        if self.mail > 0 {
            parts.push(one_or_many(self.mail, "email", "emails"));
        }
        if self.calendar > 0 {
            parts.push(one_or_many(self.calendar, "event", "events"));
        }
        if self.contacts > 0 {
            parts.push(one_or_many(self.contacts, "contact", "contacts"));
        }
        if self.todo > 0 {
            parts.push(one_or_many(self.todo, "task", "tasks"));
        }
        if self.onenote > 0 {
            parts.push(one_or_many(self.onenote, "note", "notes"));
        }
        Some(format!("{} backed up", parts.join(", ")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupRun {
    pub summary: String,
    pub delta: BackupDelta,
}

pub fn run_backup_account(
    cfg: &Config,
    account: &str,
    gate: &Arc<Mutex<()>>,
    services: &[String],
) -> Result<BackupRun, String> {
    run_backup_account_with_runtime(cfg, account, gate, services, &LiveBackupRuntime)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BackupServiceSet {
    mail: bool,
    calendar: bool,
    contacts: bool,
    todo: bool,
    onenote: bool,
}

impl BackupServiceSet {
    fn from_requested(services: &[String]) -> Result<Self, String> {
        if services.is_empty() {
            return Ok(Self::all());
        }
        let mut set = Self::none();
        for service in services {
            validate_service("backup", service, BACKUP_SERVICES)?;
            match service.as_str() {
                "mail" => set.mail = true,
                "calendar" => set.calendar = true,
                "contacts" => set.contacts = true,
                "todo" => set.todo = true,
                "onenote" => set.onenote = true,
                _ => unreachable!("validated backup service"),
            }
        }
        Ok(set)
    }

    fn all() -> Self {
        Self {
            mail: true,
            calendar: true,
            contacts: true,
            todo: true,
            onenote: true,
        }
    }

    fn none() -> Self {
        Self {
            mail: false,
            calendar: false,
            contacts: false,
            todo: false,
            onenote: false,
        }
    }

    fn refresh_services(self) -> isyncyou_engine::RefreshServices {
        isyncyou_engine::RefreshServices {
            mail: self.mail,
            calendar: self.calendar,
            contacts: self.contacts,
            todo: self.todo,
            onenote: self.onenote,
        }
    }
}

trait BackupRuntime {
    fn resolve_read_token(&self, cfg: &Config, account: &str) -> Result<String, String>;
    fn resolve_restore_token(&self, cfg: &Config, account: &str) -> Result<String, String>;
    fn refresh(
        &self,
        cfg: &Config,
        account: &str,
        read_token: String,
        restore_token: Option<String>,
        services: isyncyou_engine::RefreshServices,
    ) -> Result<isyncyou_engine::RefreshCounts, String>;
    fn record_run(
        &self,
        cfg: &Config,
        account: &str,
        started: &str,
        finished: &str,
        status: &str,
        summary: &str,
    ) -> Result<(), String>;
}

struct LiveBackupRuntime;

impl BackupRuntime for LiveBackupRuntime {
    fn resolve_read_token(&self, cfg: &Config, account: &str) -> Result<String, String> {
        isyncyou_engine::auth::resolve_cached_read_token(cfg, account)
    }

    fn resolve_restore_token(&self, cfg: &Config, account: &str) -> Result<String, String> {
        isyncyou_engine::auth::resolve_cached_restore_token(cfg, account)
    }

    fn refresh(
        &self,
        cfg: &Config,
        account: &str,
        read_token: String,
        restore_token: Option<String>,
        services: isyncyou_engine::RefreshServices,
    ) -> Result<isyncyou_engine::RefreshCounts, String> {
        isyncyou_engine::refresh_cache_account_filtered(
            cfg,
            account,
            read_token,
            restore_token,
            services,
        )
    }

    fn record_run(
        &self,
        cfg: &Config,
        account: &str,
        started: &str,
        finished: &str,
        status: &str,
        summary: &str,
    ) -> Result<(), String> {
        let path = cfg
            .accounts
            .iter()
            .find(|a| a.id == account)
            .map(|a| a.archive_root.join(".isyncyou-store.db"))
            .ok_or_else(|| format!("no account '{account}'"))?;
        let store = Store::open(path).map_err(|e| e.to_string())?;
        store
            .add_run(account, "backup", started, finished, status, summary)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

fn run_backup_account_with_runtime<R: BackupRuntime>(
    cfg: &Config,
    account: &str,
    gate: &Arc<Mutex<()>>,
    services: &[String],
    runtime: &R,
) -> Result<BackupRun, String> {
    let service_set = BackupServiceSet::from_requested(services)?;
    let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
    let started = crate::unix_now();
    let result: Result<BackupRun, String> = (|| {
        let read = runtime.resolve_read_token(cfg, account)?;
        let restore = runtime.resolve_restore_token(cfg, account).ok();
        let counts =
            runtime.refresh(cfg, account, read, restore, service_set.refresh_services())?;
        Ok(backup_run_from_counts(counts))
    })();
    let finished = crate::unix_now();
    let (status, summary) = match &result {
        Ok(run) => ("ok", run.summary.as_str()),
        Err(error) => ("error", error.as_str()),
    };
    if let Err(e) = runtime.record_run(cfg, account, &started, &finished, status, summary) {
        eprintln!("isyncyou: could not record backup run for {account}: {e}");
    }
    result
}

fn backup_run_from_counts(c: isyncyou_engine::RefreshCounts) -> BackupRun {
    let delta = BackupDelta {
        mail: c.mail_bodies as u64,
        calendar: c.calendar_bodies as u64,
        contacts: c.contacts_bodies as u64,
        todo: c.todo_bodies as u64,
        onenote: c.onenote_bodies as u64,
    };
    let summary = format!(
        "mail: {} folders, {} upserted, {} deleted; {} new bodies; {} flanks | \
         calendar: {} events, {} bodies, {} flanks | \
         contacts: {} upserted, {} bodies, {} photos | \
         todo: {} indexed, {} bodies, {} flanks, {} sub | \
         onenote: {} pages, {} bodies, {} resources, {} containers",
        c.mail_folders,
        c.mail_upserted,
        c.mail_deleted,
        c.mail_bodies,
        c.mail_flanks,
        c.calendar_events,
        c.calendar_bodies,
        c.calendar_flanks,
        c.contacts_upserted,
        c.contacts_bodies,
        c.contacts_photos,
        c.todo_indexed,
        c.todo_bodies,
        c.todo_flanks,
        c.todo_sub,
        c.onenote_pages,
        c.onenote_bodies,
        c.onenote_resources,
        c.onenote_containers,
    );
    BackupRun { summary, delta }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RestoreCloudRun {
    service: String,
    source_id: String,
    new_id: String,
}

fn run_restore_cloud(
    cfg: &Config,
    account: &str,
    service: &str,
    id: &str,
    gate: &Arc<Mutex<()>>,
) -> Result<RestoreCloudRun, String> {
    run_restore_cloud_with_runtime(cfg, account, service, id, gate, &LiveRestoreCloudRuntime)
}

trait RestoreCloudRuntime {
    fn resolve_restore_token(&self, cfg: &Config, account: &str) -> Result<String, String>;
    fn restore_cloud(
        &self,
        cfg: &Config,
        account: &str,
        service: &str,
        id: &str,
        token: String,
    ) -> Result<String, String>;
}

struct LiveRestoreCloudRuntime;

impl RestoreCloudRuntime for LiveRestoreCloudRuntime {
    fn resolve_restore_token(&self, cfg: &Config, account: &str) -> Result<String, String> {
        isyncyou_engine::auth::resolve_cached_restore_token(cfg, account)
    }

    fn restore_cloud(
        &self,
        cfg: &Config,
        account: &str,
        service: &str,
        id: &str,
        token: String,
    ) -> Result<String, String> {
        isyncyou_engine::restore_cloud(cfg, account, service, id, token)
    }
}

fn run_restore_cloud_with_runtime<R: RestoreCloudRuntime>(
    cfg: &Config,
    account: &str,
    service: &str,
    id: &str,
    gate: &Arc<Mutex<()>>,
    runtime: &R,
) -> Result<RestoreCloudRun, String> {
    if !isyncyou_engine::cloud_restore_service_supported(service) {
        return Err(isyncyou_engine::unsupported_cloud_restore_service_error(
            service,
        ));
    }
    if !cfg.restore.cloud_restore_enabled {
        return Err(isyncyou_engine::cloud_restore_disabled_error());
    }
    let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
    let token = runtime.resolve_restore_token(cfg, account)?;
    let new_id = runtime.restore_cloud(cfg, account, service, id, token)?;
    Ok(RestoreCloudRun {
        service: service.to_string(),
        source_id: id.to_string(),
        new_id,
    })
}

pub(crate) fn preview_for_pending_action(
    action: &isyncyou_agent::ToolAction,
) -> Result<PendingOperationPreview, String> {
    if action.class() != isyncyou_agent::ToolClass::Destructive {
        return Err(format!("not_confirmable: {}", action.op()));
    }
    match action {
        isyncyou_agent::ToolAction::Backup { account, services } => {
            validate_services("backup", services, BACKUP_SERVICES)?;
            let scope = if services.is_empty() {
                "all supported services".to_string()
            } else {
                format!("{} selected service(s)", services.len())
            };
            Ok(PendingOperationPreview::destructive(format!(
                "Run backup for account {account} ({scope})"
            )))
        }
        isyncyou_agent::ToolAction::RestoreCloud {
            account,
            service,
            id,
        } => {
            validate_service("restore-cloud", service, RESTORE_CLOUD_SERVICES)?;
            Ok(PendingOperationPreview::destructive(format!(
                "Restore archived {service} item {id} to Microsoft 365 for account {account}"
            )))
        }
        isyncyou_agent::ToolAction::LiveWrite {
            account,
            service,
            target,
            change,
        } => {
            let verb = validate_live_write(service, change)?;
            let target = target.as_deref().unwrap_or("new item");
            Ok(PendingOperationPreview::destructive(format!(
                "Apply {service} {verb} to {target} for account {account}"
            )))
        }
        isyncyou_agent::ToolAction::Share {
            account,
            service,
            id,
            recipient,
        } => {
            validate_service("share", service, &["onedrive"])?;
            if let Some(recipient) = recipient {
                validate_recipient(recipient)?;
                Ok(PendingOperationPreview::destructive(format!(
                    "Invite 1 recipient to OneDrive item {id} for account {account}"
                )))
            } else {
                Ok(PendingOperationPreview::destructive(format!(
                    "Create a sharing link for OneDrive item {id} for account {account}"
                )))
            }
        }
        isyncyou_agent::ToolAction::Search { .. }
        | isyncyou_agent::ToolAction::DeepSearch { .. }
        | isyncyou_agent::ToolAction::Read { .. }
        | isyncyou_agent::ToolAction::List { .. }
        | isyncyou_agent::ToolAction::Export { .. }
        | isyncyou_agent::ToolAction::RestoreLocal { .. } => {
            Err(format!("not_confirmable: {}", action.op()))
        }
    }
}

fn validate_services(kind: &str, services: &[String], allowed: &[&str]) -> Result<(), String> {
    for service in services {
        validate_service(kind, service, allowed)?;
    }
    Ok(())
}

fn validate_service(kind: &str, service: &str, allowed: &[&str]) -> Result<(), String> {
    if allowed.contains(&service) {
        Ok(())
    } else {
        Err(format!(
            "unsupported_{kind}_service: {}",
            redact_agent_operation_text(service)
        ))
    }
}

fn validate_live_write(service: &str, change: &serde_json::Value) -> Result<String, String> {
    let verb = change
        .get("verb")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "invalid_live_write: missing verb".to_string())?;
    let allowed = match service {
        "mail" => &[
            "set_read",
            "set_flag",
            "set_categories",
            "move",
            "create_draft",
            "send_draft",
        ][..],
        "calendar" => &["create", "update", "delete", "respond"][..],
        "contacts" => &["create", "update", "delete"][..],
        "todo" => &[
            "create",
            "update",
            "complete",
            "delete",
            "checklist_add",
            "checklist_toggle",
            "checklist_delete",
            "list_create",
            "list_delete",
        ][..],
        "onenote" => &["create", "delete", "append"][..],
        _ => {
            return Err(format!(
                "unsupported_live_write_service: {}",
                redact_agent_operation_text(service)
            ))
        }
    };
    if allowed.contains(&verb) {
        Ok(verb.to_string())
    } else {
        Err(format!(
            "unsupported_live_write_verb: {}",
            redact_agent_operation_text(verb)
        ))
    }
}

fn validate_recipient(recipient: &str) -> Result<(), String> {
    let trimmed = recipient.trim();
    if trimmed.is_empty() || trimmed.len() > 320 || !trimmed.contains('@') {
        return Err("invalid_share_recipient".to_string());
    }
    Ok(())
}

pub(crate) fn redact_agent_operation_text(raw: &str) -> String {
    let without_secrets = isyncyou_core::obs::redact(raw);
    let without_urls = redact_urls(&without_secrets);
    redact_email_like_tokens(&without_urls)
}

fn redact_urls(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    loop {
        let http = rest.find("http://");
        let https = rest.find("https://");
        let Some(pos) = [http, https].into_iter().flatten().min() else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..pos]);
        let after = &rest[pos..];
        let end = after
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | '}'))
            .unwrap_or(after.len());
        out.push_str("<redacted-url>");
        rest = &after[end..];
    }
}

fn redact_email_like_tokens(input: &str) -> String {
    input
        .split_whitespace()
        .map(|token| {
            let trimmed = token.trim_matches(|c: char| {
                matches!(
                    c,
                    '"' | '\'' | ',' | ';' | ':' | '<' | '>' | '(' | ')' | '[' | ']'
                )
            });
            if trimmed.contains('@') && trimmed.contains('.') {
                token.replace(trimmed, "<redacted-email>")
            } else {
                token.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
static RESTORE_LOCAL_TMP_CTR: AtomicU64 = AtomicU64::new(0);

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
pub(crate) struct RestoreLocalReadExecutor<A, D> {
    source: A,
    delegate: D,
    restore_root: PathBuf,
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
impl<A, D> RestoreLocalReadExecutor<A, D> {
    pub(crate) fn new(source: A, delegate: D, restore_root: PathBuf) -> Self {
        Self {
            source,
            delegate,
            restore_root,
        }
    }
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
impl<A, D> isyncyou_agent::ToolExecutor for RestoreLocalReadExecutor<A, D>
where
    A: isyncyou_agent::ArchiveSource + Send,
    D: isyncyou_agent::ToolExecutor + Send,
{
    fn execute_read(
        &self,
        action: &isyncyou_agent::ToolAction,
    ) -> Result<String, isyncyou_agent::AgentError> {
        match action {
            isyncyou_agent::ToolAction::RestoreLocal {
                account,
                service,
                id,
            } => self.restore_local(account, service, id),
            _ => self.delegate.execute_read(action),
        }
    }

    fn execute_read_streamed(
        &self,
        action: &isyncyou_agent::ToolAction,
        emit: &mut dyn FnMut(isyncyou_agent::StreamEvent),
    ) -> Result<String, isyncyou_agent::AgentError> {
        match action {
            isyncyou_agent::ToolAction::RestoreLocal { .. } => self.execute_read(action),
            _ => self.delegate.execute_read_streamed(action, emit),
        }
    }
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
impl<A, D> RestoreLocalReadExecutor<A, D>
where
    A: isyncyou_agent::ArchiveSource + Send,
    D: isyncyou_agent::ToolExecutor + Send,
{
    fn restore_local(
        &self,
        account: &str,
        service: &str,
        id: &str,
    ) -> Result<String, isyncyou_agent::AgentError> {
        if account != self.source.account() {
            return Err(isyncyou_agent::AgentError::ToolArgs(format!(
                "account mismatch: tool requested {account}, executor is bound to {}",
                self.source.account()
            )));
        }

        let item = self.source.get(service, id)?.ok_or_else(|| {
            isyncyou_agent::AgentError::ToolArgs(format!("no item {service}/{id}"))
        })?;
        let bytes = self.source.read_body(service, id)?;

        let service_dir = self.restore_root.join(safe_path_segment(service));
        std::fs::create_dir_all(&service_dir)
            .map_err(|e| isyncyou_agent::AgentError::Provider(format!("restore-local dir: {e}")))?;
        ensure_under_root(&self.restore_root, &service_dir)?;

        let file_name = restore_file_name(&item.name, &item.id);
        let path = allocate_restore_path(&service_dir, &file_name)?;
        ensure_under_root(&self.restore_root, path.parent().unwrap_or(&service_dir))?;
        write_owner_only_atomic(&path, &bytes).map_err(|e| {
            isyncyou_agent::AgentError::Provider(format!("restore-local write: {e}"))
        })?;

        Ok(serde_json::json!({
            "service": item.service,
            "id": item.id,
            "name": item.name,
            "path": path.to_string_lossy(),
            "bytes": bytes.len(),
            "source": {
                "service": service,
                "id": id,
                "path": item.path,
            }
        })
        .to_string())
    }
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn safe_path_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(96));
    let mut last_was_sep = false;
    for ch in raw.chars() {
        let ch = if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            ch
        } else {
            '_'
        };
        if ch == '_' {
            if !last_was_sep {
                out.push(ch);
            }
            last_was_sep = true;
        } else {
            out.push(ch);
            last_was_sep = false;
        }
        if out.len() >= 96 {
            break;
        }
    }
    let trimmed = out
        .trim_matches(|c| matches!(c, '.' | '_' | ' '))
        .to_string();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "item".to_string()
    } else {
        trimmed
    }
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn restore_file_name(name: &str, id: &str) -> String {
    let safe_name = safe_path_segment(name);
    let safe_id = safe_path_segment(id);
    if safe_name == "item" {
        safe_id
    } else {
        safe_name
    }
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn allocate_restore_path(
    dir: &Path,
    file_name: &str,
) -> Result<PathBuf, isyncyou_agent::AgentError> {
    let candidate = dir.join(file_name);
    if !candidate.exists() {
        return Ok(candidate);
    }
    let (stem, ext) = split_extension(file_name);
    for idx in 1..1000 {
        let candidate = dir.join(format!("{stem}-{idx}{ext}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(isyncyou_agent::AgentError::Provider(
        "restore-local could not allocate a non-clobbering output path".to_string(),
    ))
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn split_extension(file_name: &str) -> (&str, String) {
    match file_name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => (stem, format!(".{ext}")),
        _ => (file_name, String::new()),
    }
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn ensure_under_root(root: &Path, candidate: &Path) -> Result<(), isyncyou_agent::AgentError> {
    let root = root
        .canonicalize()
        .map_err(|e| isyncyou_agent::AgentError::Provider(format!("restore-local root: {e}")))?;
    let candidate = candidate
        .canonicalize()
        .map_err(|e| isyncyou_agent::AgentError::Provider(format!("restore-local path: {e}")))?;
    if candidate.starts_with(&root) {
        Ok(())
    } else {
        Err(isyncyou_agent::AgentError::ToolArgs(
            "restore-local path escape rejected".to_string(),
        ))
    }
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn write_owner_only_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let ctr = RESTORE_LOCAL_TMP_CTR.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(
        ".{file_name}.isyncyou-restore-tmp.{}.{ctr}",
        std::process::id()
    ));

    let res = (|| {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        if path.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "restore-local output path already exists",
            ));
        }
        std::fs::rename(&tmp, path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return res;
    }
    if let Ok(dir_file) = std::fs::File::open(dir) {
        let _ = dir_file.sync_all();
    }
    Ok(())
}

/// Desktop operation executor. Later #624 tasks fill in the individual operation
/// dispatches; Task 1 only makes the desktop/mobile policy explicit.
pub(crate) struct DesktopAgentOperations {
    cfg: Config,
    gate: Arc<Mutex<()>>,
}

impl DesktopAgentOperations {
    pub(crate) fn new(cfg: Config, gate: Arc<Mutex<()>>) -> Self {
        Self { cfg, gate }
    }
}

impl AgentConfirmedActionExecutor for DesktopAgentOperations {
    fn execute_confirmed(
        &self,
        action: &isyncyou_agent::ToolAction,
    ) -> Result<ConfirmedActionResult, String> {
        match action {
            isyncyou_agent::ToolAction::Backup { account, services } => {
                let run = run_backup_account(&self.cfg, account, &self.gate, services)?;
                Ok(ConfirmedActionResult::new(
                    serde_json::json!({
                        "op": "backup",
                        "account": account,
                        "summary": run.summary,
                        "delta": {
                            "mail": run.delta.mail,
                            "calendar": run.delta.calendar,
                            "contacts": run.delta.contacts,
                            "todo": run.delta.todo,
                            "onenote": run.delta.onenote,
                        }
                    })
                    .to_string(),
                ))
            }
            isyncyou_agent::ToolAction::RestoreCloud {
                account,
                service,
                id,
            } => {
                let run = run_restore_cloud(&self.cfg, account, service, id, &self.gate)?;
                Ok(ConfirmedActionResult::new(
                    serde_json::json!({
                        "op": "restore-cloud",
                        "account": redact_agent_operation_text(account),
                        "service": run.service,
                        "source_id": redact_agent_operation_text(&run.source_id),
                        "new_id": redact_agent_operation_text(&run.new_id),
                    })
                    .to_string(),
                ))
            }
            _ => Err(format!(
                "not_implemented: confirmed agent action '{}' lands in S-AG.9/#624",
                action.op()
            )),
        }
    }
}

pub(crate) struct MobileDisabledAgentOperations;

impl AgentConfirmedActionExecutor for MobileDisabledAgentOperations {
    fn execute_confirmed(
        &self,
        _action: &isyncyou_agent::ToolAction,
    ) -> Result<ConfirmedActionResult, String> {
        Err("not_available_on_mobile: mobile_agent_operations_land_in_625_626".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{mpsc, Mutex as StdMutex};
    use std::time::Duration;

    #[derive(Debug, Default)]
    struct BackupRuntimeState {
        read_calls: usize,
        restore_calls: usize,
        refresh_services: Vec<isyncyou_engine::RefreshServices>,
        recorded: Vec<(String, String)>,
    }

    #[derive(Clone)]
    struct RecordingBackupRuntime {
        state: Arc<StdMutex<BackupRuntimeState>>,
        counts: isyncyou_engine::RefreshCounts,
        read_result: Result<String, String>,
        restore_result: Result<String, String>,
        read_signal: Arc<StdMutex<Option<mpsc::Sender<()>>>>,
    }

    impl RecordingBackupRuntime {
        fn new(counts: isyncyou_engine::RefreshCounts) -> Self {
            Self {
                state: Arc::new(StdMutex::new(BackupRuntimeState::default())),
                counts,
                read_result: Ok("read-token".to_string()),
                restore_result: Ok("restore-token".to_string()),
                read_signal: Arc::new(StdMutex::new(None)),
            }
        }

        fn failing_read(error: &str) -> Self {
            Self {
                read_result: Err(error.to_string()),
                ..Self::new(Default::default())
            }
        }

        fn with_read_signal(self, sender: mpsc::Sender<()>) -> Self {
            {
                let mut signal = self.read_signal.lock().unwrap();
                *signal = Some(sender);
            }
            self
        }

        fn state(&self) -> std::sync::MutexGuard<'_, BackupRuntimeState> {
            self.state.lock().unwrap()
        }
    }

    impl BackupRuntime for RecordingBackupRuntime {
        fn resolve_read_token(&self, _cfg: &Config, _account: &str) -> Result<String, String> {
            self.state.lock().unwrap().read_calls += 1;
            if let Some(sender) = self.read_signal.lock().unwrap().take() {
                let _ = sender.send(());
            }
            self.read_result.clone()
        }

        fn resolve_restore_token(&self, _cfg: &Config, _account: &str) -> Result<String, String> {
            self.state.lock().unwrap().restore_calls += 1;
            self.restore_result.clone()
        }

        fn refresh(
            &self,
            _cfg: &Config,
            _account: &str,
            _read_token: String,
            _restore_token: Option<String>,
            services: isyncyou_engine::RefreshServices,
        ) -> Result<isyncyou_engine::RefreshCounts, String> {
            self.state.lock().unwrap().refresh_services.push(services);
            Ok(self.counts.clone())
        }

        fn record_run(
            &self,
            _cfg: &Config,
            _account: &str,
            _started: &str,
            _finished: &str,
            status: &str,
            summary: &str,
        ) -> Result<(), String> {
            self.state
                .lock()
                .unwrap()
                .recorded
                .push((status.to_string(), summary.to_string()));
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct RestoreCloudRuntimeState {
        token_calls: usize,
        restore_calls: Vec<(String, String, String, String, String)>,
    }

    #[derive(Clone)]
    struct RecordingRestoreCloudRuntime {
        state: Arc<StdMutex<RestoreCloudRuntimeState>>,
        token_result: Result<String, String>,
        restore_result: Result<String, String>,
    }

    impl RecordingRestoreCloudRuntime {
        fn new(new_id: &str) -> Self {
            Self {
                state: Arc::new(StdMutex::new(RestoreCloudRuntimeState::default())),
                token_result: Ok("restore-token".to_string()),
                restore_result: Ok(new_id.to_string()),
            }
        }

        fn state(&self) -> std::sync::MutexGuard<'_, RestoreCloudRuntimeState> {
            self.state.lock().unwrap()
        }
    }

    impl RestoreCloudRuntime for RecordingRestoreCloudRuntime {
        fn resolve_restore_token(&self, _cfg: &Config, _account: &str) -> Result<String, String> {
            self.state.lock().unwrap().token_calls += 1;
            self.token_result.clone()
        }

        fn restore_cloud(
            &self,
            _cfg: &Config,
            account: &str,
            service: &str,
            id: &str,
            token: String,
        ) -> Result<String, String> {
            self.state.lock().unwrap().restore_calls.push((
                account.to_string(),
                service.to_string(),
                id.to_string(),
                token,
                self.restore_result.clone().unwrap_or_default(),
            ));
            self.restore_result.clone()
        }
    }

    fn restore_enabled_config() -> Config {
        let mut cfg = Config::default();
        cfg.restore.cloud_restore_enabled = true;
        cfg
    }

    #[test]
    fn share_invite_preview_redacts_recipient_emails() {
        let action = isyncyou_agent::parse_action(&json!({
            "op": "share",
            "account": "me",
            "service": "onedrive",
            "id": "file-1",
            "recipient": "recipient@example.com"
        }))
        .unwrap();

        let preview = preview_for_pending_action(&action).unwrap();

        assert!(preview.text.contains("Invite 1 recipient"));
        assert!(!preview.text.contains("recipient@example.com"));
        assert_eq!(preview.risk, "destructive");
    }

    #[test]
    fn share_link_preview_redacts_url_material() {
        let raw = "created https://tenant.sharepoint.com/:w:/r/sites/x?code=oauth-code for user@example.com";
        let redacted = redact_agent_operation_text(raw);

        assert!(!redacted.contains("tenant.sharepoint.com"));
        assert!(!redacted.contains("oauth-code"));
        assert!(!redacted.contains("user@example.com"));
        assert!(redacted.contains("<redacted-url>"));
        assert!(redacted.contains("<redacted-email>"));
    }

    #[test]
    fn backup_unknown_service_rejected_before_pending() {
        let action = isyncyou_agent::parse_action(&json!({
            "op": "backup",
            "account": "me",
            "services": ["mail", "shell"]
        }))
        .unwrap();

        let err = preview_for_pending_action(&action).unwrap_err();

        assert!(err.contains("unsupported_backup_service"));
        assert!(err.contains("shell"));
    }

    #[test]
    fn agent_backup_confirm_runs_refresh_cache_and_records_backup_run() {
        let runtime = RecordingBackupRuntime::new(isyncyou_engine::RefreshCounts {
            mail_bodies: 2,
            calendar_bodies: 1,
            ..Default::default()
        });
        let gate = Arc::new(Mutex::new(()));

        let run = run_backup_account_with_runtime(&Config::default(), "me", &gate, &[], &runtime)
            .unwrap();

        assert!(run.summary.contains("2 new bodies"));
        assert_eq!(run.delta.mail, 2);
        assert_eq!(run.delta.calendar, 1);
        let state = runtime.state();
        assert_eq!(state.read_calls, 1);
        assert_eq!(state.restore_calls, 1);
        assert_eq!(
            state.refresh_services,
            vec![isyncyou_engine::RefreshServices::all()]
        );
        assert_eq!(state.recorded.len(), 1);
        assert_eq!(state.recorded[0].0, "ok");
        assert!(state.recorded[0].1.contains("mail:"));
    }

    #[test]
    fn agent_backup_confirm_holds_store_gate() {
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let (tx, rx) = mpsc::channel();
        let runtime =
            RecordingBackupRuntime::failing_read("no cached read token").with_read_signal(tx);
        let runtime_for_thread = runtime.clone();
        let gate_for_thread = gate.clone();

        let handle = std::thread::spawn(move || {
            run_backup_account_with_runtime(
                &Config::default(),
                "me",
                &gate_for_thread,
                &[],
                &runtime_for_thread,
            )
        });

        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        drop(held);
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let err = handle.join().unwrap().unwrap_err();
        assert!(err.contains("no cached read token"));
        assert_eq!(runtime.state().read_calls, 1);
    }

    #[test]
    fn agent_backup_unknown_service_rejected_before_token_lookup() {
        let runtime = RecordingBackupRuntime::new(Default::default());
        let gate = Arc::new(Mutex::new(()));

        let err = run_backup_account_with_runtime(
            &Config::default(),
            "me",
            &gate,
            &["shell".to_string()],
            &runtime,
        )
        .unwrap_err();

        assert!(err.contains("unsupported_backup_service"));
        let state = runtime.state();
        assert_eq!(state.read_calls, 0);
        assert!(state.refresh_services.is_empty());
        assert!(state.recorded.is_empty());
    }

    #[test]
    fn agent_backup_service_filter_does_not_run_unselected_connectors() {
        let runtime = RecordingBackupRuntime::new(Default::default());
        let gate = Arc::new(Mutex::new(()));

        run_backup_account_with_runtime(
            &Config::default(),
            "me",
            &gate,
            &["mail".to_string(), "todo".to_string()],
            &runtime,
        )
        .unwrap();

        assert_eq!(
            runtime.state().refresh_services,
            vec![isyncyou_engine::RefreshServices {
                mail: true,
                calendar: false,
                contacts: false,
                todo: true,
                onenote: false,
            }]
        );
    }

    #[test]
    fn agent_restore_cloud_confirm_routes_to_ledger_restore() {
        let cfg = restore_enabled_config();
        let gate = Arc::new(Mutex::new(()));
        let runtime = RecordingRestoreCloudRuntime::new("new-cloud-id");

        let run = run_restore_cloud_with_runtime(&cfg, "me", "mail", "source-id", &gate, &runtime)
            .unwrap();

        assert_eq!(run.service, "mail");
        assert_eq!(run.source_id, "source-id");
        assert_eq!(run.new_id, "new-cloud-id");
        let state = runtime.state();
        assert_eq!(state.token_calls, 1);
        assert_eq!(
            state.restore_calls,
            vec![(
                "me".to_string(),
                "mail".to_string(),
                "source-id".to_string(),
                "restore-token".to_string(),
                "new-cloud-id".to_string(),
            )]
        );
    }

    #[test]
    fn agent_restore_cloud_disabled_refuses_before_token_lookup() {
        let cfg = Config::default();
        let gate = Arc::new(Mutex::new(()));
        let runtime = RecordingRestoreCloudRuntime::new("new-cloud-id");

        let err = run_restore_cloud_with_runtime(&cfg, "me", "mail", "source-id", &gate, &runtime)
            .unwrap_err();

        assert!(err.contains("cloud restore is disabled"));
        let state = runtime.state();
        assert_eq!(state.token_calls, 0);
        assert!(state.restore_calls.is_empty());
    }

    #[test]
    fn agent_restore_cloud_unsupported_service_refuses_before_token_lookup() {
        let cfg = restore_enabled_config();
        let gate = Arc::new(Mutex::new(()));
        let runtime = RecordingRestoreCloudRuntime::new("new-cloud-id");

        let err =
            run_restore_cloud_with_runtime(&cfg, "me", "onedrive", "source-id", &gate, &runtime)
                .unwrap_err();

        assert!(err.contains("not crash-safe yet"));
        let state = runtime.state();
        assert_eq!(state.token_calls, 0);
        assert!(state.restore_calls.is_empty());
    }
}
