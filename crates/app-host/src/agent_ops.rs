use std::sync::{Arc, Mutex};

use isyncyou_core::Config;
use isyncyou_store::MobileJob;
use isyncyou_store::Store;

use crate::mobile_jobs::MobileJobRuntime;
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
#[derive(Clone)]
pub enum AgentOperationPolicy {
    DesktopEnabled,
    MobileDisabled,
    MobileFullNode { mobile_jobs: Arc<MobileJobRuntime> },
}

impl std::fmt::Debug for AgentOperationPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DesktopEnabled => f.write_str("DesktopEnabled"),
            Self::MobileDisabled => f.write_str("MobileDisabled"),
            Self::MobileFullNode { .. } => f.write_str("MobileFullNode"),
        }
    }
}

pub(crate) fn confirmed_executor_for_policy(
    policy: AgentOperationPolicy,
    cfg: Config,
    gate: Arc<Mutex<()>>,
) -> Arc<dyn AgentConfirmedActionExecutor> {
    match policy {
        AgentOperationPolicy::DesktopEnabled => Arc::new(DesktopAgentOperations::new(cfg, gate)),
        AgentOperationPolicy::MobileDisabled => Arc::new(MobileDisabledAgentOperations),
        AgentOperationPolicy::MobileFullNode { mobile_jobs } => {
            Arc::new(MobileFullNodeAgentOperations::new(cfg, gate, mobile_jobs))
        }
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
const LIVE_WRITE_SERVICES: &[&str] = &["mail", "calendar", "contacts", "todo", "onenote"];
const SHARE_SERVICES: &[&str] = &["onedrive"];
const SHARE_LINK_TYPES: &[&str] = &["view", "edit", "embed"];
const SHARE_LINK_SCOPES: &[&str] = &["anonymous", "organization", "users"];
const SHARE_INVITE_ROLES: &[&str] = &["read", "write"];
const LIVE_WRITE_MAX_STRING: usize = 16 * 1024;
const LIVE_WRITE_MAX_BODY: usize = 128 * 1024;
const LIVE_WRITE_MAX_ARRAY: usize = 50;

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

#[derive(Debug)]
pub(crate) enum MobileBackupError {
    InvalidRequest,
    Authentication,
    Refresh(isyncyou_engine::RefreshFailure),
}

pub(crate) fn run_mobile_backup_account(
    cfg: &Config,
    account: &str,
    gate: &Arc<Mutex<()>>,
    services: &[String],
) -> Result<BackupRun, MobileBackupError> {
    let service_set = BackupServiceSet::from_requested(services)
        .map_err(|_| MobileBackupError::InvalidRequest)?;
    let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    let started = crate::unix_now();
    let result = (|| {
        let read = isyncyou_engine::auth::resolve_cache_refresh_token(cfg, account)
            .map_err(|_| MobileBackupError::Authentication)?;
        let restore = isyncyou_engine::auth::resolve_cached_restore_token(cfg, account).ok();
        let counts = isyncyou_engine::refresh_cache_account_filtered_strict(
            cfg,
            account,
            read,
            restore,
            service_set.refresh_services(),
        )
        .map_err(MobileBackupError::Refresh)?;
        Ok(backup_run_from_counts(counts))
    })();
    let finished = crate::unix_now();
    let (status, summary) = match &result {
        Ok(run) => ("ok", run.summary.as_str()),
        Err(_) => ("error", "mobile backup failed"),
    };
    if let Err(_error) =
        LiveBackupRuntime.record_run(cfg, account, &started, &finished, status, summary)
    {
        eprintln!("isyncyou: mobile_backup_run_record_failed");
    }
    result
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
        Err(_) => ("error", "backup failed"),
    };
    if let Err(_error) = runtime.record_run(cfg, account, &started, &finished, status, summary) {
        eprintln!("isyncyou: backup_run_record_failed");
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

#[derive(Debug, Clone, PartialEq)]
enum AgentLiveWriteIntent {
    Mail(MailLiveWrite),
    Calendar(CalendarLiveWrite),
    Contacts(ContactLiveWrite),
    Todo(TodoLiveWrite),
    OneNote(OneNoteLiveWrite),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MailLiveWrite {
    SetRead {
        target: String,
        is_read: bool,
    },
    SetFlag {
        target: String,
        flag_status: String,
        due: Option<String>,
        tz: String,
    },
    SetCategories {
        target: String,
        categories: Vec<String>,
    },
    Move {
        target: String,
        destination_id: String,
    },
    CreateDraft {
        subject: String,
        body_html: String,
        to: Vec<String>,
    },
    SendDraft {
        target: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum CalendarLiveWrite {
    Create {
        event: serde_json::Value,
    },
    Update {
        target: String,
        event: serde_json::Value,
    },
    Delete {
        target: String,
    },
    Respond {
        target: String,
        response: String,
        comment: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum ContactLiveWrite {
    Create {
        contact: serde_json::Value,
    },
    Update {
        target: String,
        contact: serde_json::Value,
    },
    Delete {
        target: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum TodoLiveWrite {
    Create {
        list_id: String,
        task: serde_json::Value,
    },
    Update {
        list_id: String,
        target: String,
        task: serde_json::Value,
    },
    Complete {
        list_id: String,
        target: String,
    },
    Delete {
        list_id: String,
        target: String,
    },
    ChecklistAdd {
        list_id: String,
        target: String,
        title: String,
    },
    ChecklistToggle {
        list_id: String,
        target: String,
        item_id: String,
        checked: bool,
    },
    ChecklistDelete {
        list_id: String,
        target: String,
        item_id: String,
    },
    ListCreate {
        name: String,
    },
    ListDelete {
        list_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OneNoteLiveWrite {
    Create { section_id: String, html: Vec<u8> },
    Delete { target: String },
    Append { target: String, text: String },
}

impl AgentLiveWriteIntent {
    fn service(&self) -> &'static str {
        match self {
            Self::Mail(_) => "mail",
            Self::Calendar(_) => "calendar",
            Self::Contacts(_) => "contacts",
            Self::Todo(_) => "todo",
            Self::OneNote(_) => "onenote",
        }
    }

    fn verb(&self) -> &'static str {
        match self {
            Self::Mail(MailLiveWrite::SetRead { .. }) => "set_read",
            Self::Mail(MailLiveWrite::SetFlag { .. }) => "set_flag",
            Self::Mail(MailLiveWrite::SetCategories { .. }) => "set_categories",
            Self::Mail(MailLiveWrite::Move { .. }) => "move",
            Self::Mail(MailLiveWrite::CreateDraft { .. }) => "create_draft",
            Self::Mail(MailLiveWrite::SendDraft { .. }) => "send_draft",
            Self::Calendar(CalendarLiveWrite::Create { .. }) => "create",
            Self::Calendar(CalendarLiveWrite::Update { .. }) => "update",
            Self::Calendar(CalendarLiveWrite::Delete { .. }) => "delete",
            Self::Calendar(CalendarLiveWrite::Respond { .. }) => "respond",
            Self::Contacts(ContactLiveWrite::Create { .. }) => "create",
            Self::Contacts(ContactLiveWrite::Update { .. }) => "update",
            Self::Contacts(ContactLiveWrite::Delete { .. }) => "delete",
            Self::Todo(TodoLiveWrite::Create { .. }) => "create",
            Self::Todo(TodoLiveWrite::Update { .. }) => "update",
            Self::Todo(TodoLiveWrite::Complete { .. }) => "complete",
            Self::Todo(TodoLiveWrite::Delete { .. }) => "delete",
            Self::Todo(TodoLiveWrite::ChecklistAdd { .. }) => "checklist_add",
            Self::Todo(TodoLiveWrite::ChecklistToggle { .. }) => "checklist_toggle",
            Self::Todo(TodoLiveWrite::ChecklistDelete { .. }) => "checklist_delete",
            Self::Todo(TodoLiveWrite::ListCreate { .. }) => "list_create",
            Self::Todo(TodoLiveWrite::ListDelete { .. }) => "list_delete",
            Self::OneNote(OneNoteLiveWrite::Create { .. }) => "create",
            Self::OneNote(OneNoteLiveWrite::Delete { .. }) => "delete",
            Self::OneNote(OneNoteLiveWrite::Append { .. }) => "append",
        }
    }

    fn target_label(&self) -> &'static str {
        match self {
            Self::Mail(MailLiveWrite::CreateDraft { .. })
            | Self::Calendar(CalendarLiveWrite::Create { .. })
            | Self::Contacts(ContactLiveWrite::Create { .. })
            | Self::Todo(TodoLiveWrite::Create { .. })
            | Self::Todo(TodoLiveWrite::ListCreate { .. })
            | Self::OneNote(OneNoteLiveWrite::Create { .. }) => "new item",
            Self::Todo(TodoLiveWrite::ListDelete { .. }) => "list",
            _ => "existing item",
        }
    }

    fn confirmation_copy(&self) -> &'static str {
        match self {
            Self::Mail(MailLiveWrite::SetRead { is_read: true, .. }) => "Mark this email as read",
            Self::Mail(MailLiveWrite::SetRead { is_read: false, .. }) => {
                "Mark this email as unread"
            }
            Self::Mail(MailLiveWrite::SetFlag { .. }) => "Update the follow-up flag on this email",
            Self::Mail(MailLiveWrite::SetCategories { .. }) => {
                "Update the categories on this email"
            }
            Self::Mail(MailLiveWrite::Move { .. }) => "Move this email",
            Self::Mail(MailLiveWrite::CreateDraft { .. }) => "Create an email draft",
            Self::Mail(MailLiveWrite::SendDraft { .. }) => "Send this email draft",
            Self::Calendar(CalendarLiveWrite::Create { .. }) => "Create a calendar event",
            Self::Calendar(CalendarLiveWrite::Update { .. }) => "Update this calendar event",
            Self::Calendar(CalendarLiveWrite::Delete { .. }) => "Delete this calendar event",
            Self::Calendar(CalendarLiveWrite::Respond { .. }) => {
                "Respond to this calendar invitation"
            }
            Self::Contacts(ContactLiveWrite::Create { .. }) => "Create a contact",
            Self::Contacts(ContactLiveWrite::Update { .. }) => "Update this contact",
            Self::Contacts(ContactLiveWrite::Delete { .. }) => "Delete this contact",
            Self::Todo(TodoLiveWrite::Create { .. }) => "Create a task",
            Self::Todo(TodoLiveWrite::Update { .. }) => "Update this task",
            Self::Todo(TodoLiveWrite::Complete { .. }) => "Mark this task as complete",
            Self::Todo(TodoLiveWrite::Delete { .. }) => "Delete this task",
            Self::Todo(TodoLiveWrite::ChecklistAdd { .. }) => "Add a checklist item to this task",
            Self::Todo(TodoLiveWrite::ChecklistToggle { checked: true, .. }) => {
                "Mark this checklist item as complete"
            }
            Self::Todo(TodoLiveWrite::ChecklistToggle { checked: false, .. }) => {
                "Mark this checklist item as incomplete"
            }
            Self::Todo(TodoLiveWrite::ChecklistDelete { .. }) => "Delete this checklist item",
            Self::Todo(TodoLiveWrite::ListCreate { .. }) => "Create a task list",
            Self::Todo(TodoLiveWrite::ListDelete { .. }) => "Delete this task list",
            Self::OneNote(OneNoteLiveWrite::Create { .. }) => "Create a OneNote page",
            Self::OneNote(OneNoteLiveWrite::Delete { .. }) => "Delete this OneNote page",
            Self::OneNote(OneNoteLiveWrite::Append { .. }) => "Add content to this OneNote page",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveWriteRun {
    service: String,
    verb: String,
    target: String,
    result_id: Option<String>,
    summary: String,
}

fn run_live_write(
    cfg: &Config,
    action: &isyncyou_agent::ToolAction,
    gate: &Arc<Mutex<()>>,
) -> Result<LiveWriteRun, String> {
    run_live_write_with_runtime(cfg, action, gate, &LiveLiveWriteRuntime)
}

trait LiveWriteRuntime {
    fn execute_live_write(
        &self,
        cfg: &Config,
        account: &str,
        intent: &AgentLiveWriteIntent,
    ) -> Result<Option<String>, String>;
}

struct LiveLiveWriteRuntime;

impl LiveWriteRuntime for LiveLiveWriteRuntime {
    fn execute_live_write(
        &self,
        cfg: &Config,
        account: &str,
        intent: &AgentLiveWriteIntent,
    ) -> Result<Option<String>, String> {
        match intent {
            AgentLiveWriteIntent::Mail(op) => {
                let writer = crate::DaemonMailWrite { cfg: cfg.clone() };
                match op {
                    MailLiveWrite::SetRead { target, is_read } => {
                        isyncyou_webui::MailWriteHandler::set_read(
                            &writer, account, target, *is_read,
                        )?;
                        Ok(None)
                    }
                    MailLiveWrite::SetFlag {
                        target,
                        flag_status,
                        due,
                        tz,
                    } => {
                        isyncyou_webui::MailWriteHandler::set_flag(
                            &writer,
                            account,
                            target,
                            flag_status,
                            due.as_deref(),
                            tz,
                        )?;
                        Ok(None)
                    }
                    MailLiveWrite::SetCategories { target, categories } => {
                        isyncyou_webui::MailWriteHandler::set_categories(
                            &writer, account, target, categories,
                        )?;
                        Ok(None)
                    }
                    MailLiveWrite::Move {
                        target,
                        destination_id,
                    } => isyncyou_webui::MailWriteHandler::move_to(
                        &writer,
                        account,
                        target,
                        destination_id,
                    )
                    .map(Some),
                    MailLiveWrite::CreateDraft {
                        subject,
                        body_html,
                        to,
                    } => isyncyou_webui::MailWriteHandler::create_draft(
                        &writer, account, subject, body_html, to,
                    )
                    .map(Some),
                    MailLiveWrite::SendDraft { target } => {
                        isyncyou_webui::MailWriteHandler::send_draft(&writer, account, target)?;
                        Ok(None)
                    }
                }
            }
            AgentLiveWriteIntent::Calendar(op) => {
                let writer = crate::DaemonCalendarWrite { cfg: cfg.clone() };
                match op {
                    CalendarLiveWrite::Create { event } => {
                        isyncyou_webui::CalendarWriteHandler::create(&writer, account, event)
                            .map(Some)
                    }
                    CalendarLiveWrite::Update { target, event } => {
                        isyncyou_webui::CalendarWriteHandler::update(
                            &writer, account, target, event,
                        )?;
                        Ok(None)
                    }
                    CalendarLiveWrite::Delete { target } => {
                        isyncyou_webui::CalendarWriteHandler::delete(&writer, account, target)?;
                        Ok(None)
                    }
                    CalendarLiveWrite::Respond {
                        target,
                        response,
                        comment,
                    } => {
                        isyncyou_webui::CalendarWriteHandler::respond(
                            &writer, account, target, response, comment,
                        )?;
                        Ok(None)
                    }
                }
            }
            AgentLiveWriteIntent::Contacts(op) => {
                let writer = crate::DaemonContactWrite { cfg: cfg.clone() };
                match op {
                    ContactLiveWrite::Create { contact } => {
                        isyncyou_webui::ContactWriteHandler::create(&writer, account, contact)
                            .map(Some)
                    }
                    ContactLiveWrite::Update { target, contact } => {
                        isyncyou_webui::ContactWriteHandler::update(
                            &writer, account, target, contact,
                        )?;
                        Ok(None)
                    }
                    ContactLiveWrite::Delete { target } => {
                        isyncyou_webui::ContactWriteHandler::delete(&writer, account, target)?;
                        Ok(None)
                    }
                }
            }
            AgentLiveWriteIntent::Todo(op) => {
                let writer = crate::DaemonTaskWrite { cfg: cfg.clone() };
                match op {
                    TodoLiveWrite::Create { list_id, task } => {
                        isyncyou_webui::TaskWriteHandler::create(&writer, account, list_id, task)
                            .map(Some)
                    }
                    TodoLiveWrite::Update {
                        list_id,
                        target,
                        task,
                    } => {
                        isyncyou_webui::TaskWriteHandler::update(
                            &writer, account, list_id, target, task,
                        )?;
                        Ok(None)
                    }
                    TodoLiveWrite::Complete { list_id, target } => {
                        isyncyou_webui::TaskWriteHandler::complete(
                            &writer, account, list_id, target,
                        )?;
                        Ok(None)
                    }
                    TodoLiveWrite::Delete { list_id, target } => {
                        isyncyou_webui::TaskWriteHandler::delete(
                            &writer, account, list_id, target,
                        )?;
                        Ok(None)
                    }
                    TodoLiveWrite::ChecklistAdd {
                        list_id,
                        target,
                        title,
                    } => isyncyou_webui::TaskWriteHandler::checklist_add(
                        &writer, account, list_id, target, title,
                    )
                    .map(Some),
                    TodoLiveWrite::ChecklistToggle {
                        list_id,
                        target,
                        item_id,
                        checked,
                    } => {
                        isyncyou_webui::TaskWriteHandler::checklist_toggle(
                            &writer, account, list_id, target, item_id, *checked,
                        )?;
                        Ok(None)
                    }
                    TodoLiveWrite::ChecklistDelete {
                        list_id,
                        target,
                        item_id,
                    } => {
                        isyncyou_webui::TaskWriteHandler::checklist_delete(
                            &writer, account, list_id, target, item_id,
                        )?;
                        Ok(None)
                    }
                    TodoLiveWrite::ListCreate { name } => {
                        isyncyou_webui::TaskWriteHandler::list_create(&writer, account, name)
                            .map(Some)
                    }
                    TodoLiveWrite::ListDelete { list_id } => {
                        isyncyou_webui::TaskWriteHandler::list_delete(&writer, account, list_id)?;
                        Ok(None)
                    }
                }
            }
            AgentLiveWriteIntent::OneNote(op) => {
                let writer = crate::DaemonOneNoteWrite { cfg: cfg.clone() };
                match op {
                    OneNoteLiveWrite::Create { section_id, html } => {
                        isyncyou_webui::OneNoteWriteHandler::create(
                            &writer, account, section_id, html,
                        )
                        .map(Some)
                    }
                    OneNoteLiveWrite::Delete { target } => {
                        isyncyou_webui::OneNoteWriteHandler::delete(&writer, account, target)?;
                        Ok(None)
                    }
                    OneNoteLiveWrite::Append { target, text } => {
                        isyncyou_webui::OneNoteWriteHandler::append(
                            &writer, account, target, text,
                        )?;
                        Ok(None)
                    }
                }
            }
        }
    }
}

fn run_live_write_with_runtime<R: LiveWriteRuntime>(
    cfg: &Config,
    action: &isyncyou_agent::ToolAction,
    _gate: &Arc<Mutex<()>>,
    runtime: &R,
) -> Result<LiveWriteRun, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, .. } = action else {
        return Err(format!("not_live_write_action: {}", action.op()));
    };
    let intent = validate_live_write_action(action)?;
    // These adapters call the same direct Graph writers as the WebUI and do not
    // open the local Store. Waiting on the mobile Store gate here can starve a
    // confirmed action behind a long cache refresh until the bridge times out.
    let result_id = runtime.execute_live_write(cfg, account, &intent)?;
    let service = intent.service().to_string();
    let verb = intent.verb().to_string();
    let target = intent.target_label().to_string();
    let summary = match &result_id {
        Some(id) => format!("{service} {verb} ok id={}", redact_agent_operation_text(id)),
        None => format!("{service} {verb} ok"),
    };
    Ok(LiveWriteRun {
        service,
        verb,
        target,
        result_id: result_id.map(|id| redact_agent_operation_text(&id)),
        summary,
    })
}

fn validate_live_write_action(
    action: &isyncyou_agent::ToolAction,
) -> Result<AgentLiveWriteIntent, String> {
    let isyncyou_agent::ToolAction::LiveWrite {
        service,
        target,
        change,
        ..
    } = action
    else {
        return Err(format!("not_live_write_action: {}", action.op()));
    };
    validate_service("live_write", service, LIVE_WRITE_SERVICES)?;
    let verb = required_change_str(change, "verb")?;
    match (service.as_str(), verb.as_str()) {
        ("mail", "set_read") => Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::SetRead {
            target: required_target(target, change)?,
            is_read: required_change_bool(change, "is_read")?,
        })),
        ("mail", "set_flag") => {
            let flag_status = required_change_str(change, "flag_status")?;
            validate_named_value(
                "live_write_flag_status",
                &flag_status,
                &["notFlagged", "flagged", "complete"],
            )?;
            Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::SetFlag {
                target: required_target(target, change)?,
                flag_status,
                due: optional_change_str(change, "due")?,
                tz: optional_change_str(change, "tz")?.unwrap_or_else(|| "UTC".to_string()),
            }))
        }
        ("mail", "set_categories") => {
            Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::SetCategories {
                target: required_target(target, change)?,
                categories: optional_string_array(change, "categories")?.unwrap_or_default(),
            }))
        }
        ("mail", "move") => Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::Move {
            target: required_target(target, change)?,
            destination_id: required_change_str(change, "destination_id")?,
        })),
        ("mail", "create_draft") => Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::CreateDraft {
            subject: optional_change_str(change, "subject")?.unwrap_or_default(),
            body_html: required_body_str(change, &["body_html", "body"])?,
            to: required_string_array(change, "to")?,
        })),
        ("mail", "send_draft") => Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::SendDraft {
            target: required_target(target, change)?,
        })),
        ("calendar", "create") => Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Create {
            event: required_object(change, "event")?,
        })),
        ("calendar", "update") => Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Update {
            target: required_target(target, change)?,
            event: required_object(change, "event")?,
        })),
        ("calendar", "delete") => Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Delete {
            target: required_target(target, change)?,
        })),
        ("calendar", "respond") => {
            let response = required_change_str(change, "response")?;
            validate_named_value(
                "live_write_calendar_response",
                &response,
                &["accept", "decline", "tentative"],
            )?;
            Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Respond {
                target: required_target(target, change)?,
                response,
                comment: optional_change_str(change, "comment")?.unwrap_or_default(),
            }))
        }
        ("contacts", "create") => Ok(AgentLiveWriteIntent::Contacts(ContactLiveWrite::Create {
            contact: required_object(change, "contact")?,
        })),
        ("contacts", "update") => Ok(AgentLiveWriteIntent::Contacts(ContactLiveWrite::Update {
            target: required_target(target, change)?,
            contact: required_object(change, "contact")?,
        })),
        ("contacts", "delete") => Ok(AgentLiveWriteIntent::Contacts(ContactLiveWrite::Delete {
            target: required_target(target, change)?,
        })),
        ("todo", "create") => Ok(AgentLiveWriteIntent::Todo(TodoLiveWrite::Create {
            list_id: required_change_str(change, "list_id")?,
            task: required_object(change, "task")?,
        })),
        ("todo", "update") => Ok(AgentLiveWriteIntent::Todo(TodoLiveWrite::Update {
            list_id: required_change_str(change, "list_id")?,
            target: required_target(target, change)?,
            task: required_object(change, "task")?,
        })),
        ("todo", "complete") => Ok(AgentLiveWriteIntent::Todo(TodoLiveWrite::Complete {
            list_id: required_change_str(change, "list_id")?,
            target: required_target(target, change)?,
        })),
        ("todo", "delete") => Ok(AgentLiveWriteIntent::Todo(TodoLiveWrite::Delete {
            list_id: required_change_str(change, "list_id")?,
            target: required_target(target, change)?,
        })),
        ("todo", "checklist_add") => Ok(AgentLiveWriteIntent::Todo(TodoLiveWrite::ChecklistAdd {
            list_id: required_change_str(change, "list_id")?,
            target: required_target(target, change)?,
            title: required_change_str(change, "title")?,
        })),
        ("todo", "checklist_toggle") => {
            Ok(AgentLiveWriteIntent::Todo(TodoLiveWrite::ChecklistToggle {
                list_id: required_change_str(change, "list_id")?,
                target: required_target(target, change)?,
                item_id: required_change_str(change, "item_id")?,
                checked: required_change_bool(change, "checked")?,
            }))
        }
        ("todo", "checklist_delete") => {
            Ok(AgentLiveWriteIntent::Todo(TodoLiveWrite::ChecklistDelete {
                list_id: required_change_str(change, "list_id")?,
                target: required_target(target, change)?,
                item_id: required_change_str(change, "item_id")?,
            }))
        }
        ("todo", "list_create") => Ok(AgentLiveWriteIntent::Todo(TodoLiveWrite::ListCreate {
            name: required_change_str(change, "name")?,
        })),
        ("todo", "list_delete") => Ok(AgentLiveWriteIntent::Todo(TodoLiveWrite::ListDelete {
            list_id: required_change_str(change, "list_id")?,
        })),
        ("onenote", "create") => Ok(AgentLiveWriteIntent::OneNote(OneNoteLiveWrite::Create {
            section_id: required_change_str(change, "section_id")?,
            html: required_body_str(change, &["html", "body_html"])?.into_bytes(),
        })),
        ("onenote", "delete") => Ok(AgentLiveWriteIntent::OneNote(OneNoteLiveWrite::Delete {
            target: required_target(target, change)?,
        })),
        ("onenote", "append") => Ok(AgentLiveWriteIntent::OneNote(OneNoteLiveWrite::Append {
            target: required_target(target, change)?,
            text: required_body_str(change, &["text"])?,
        })),
        _ => Err(format!(
            "unsupported_live_write_verb: {}",
            redact_agent_operation_text(&verb)
        )),
    }
}

fn ensure_mobile_live_write_allowlisted(action: &isyncyou_agent::ToolAction) -> Result<(), String> {
    let intent = validate_live_write_action(action)?;
    let allowed = matches!(
        intent,
        AgentLiveWriteIntent::Mail(MailLiveWrite::SetRead { .. })
            | AgentLiveWriteIntent::Mail(MailLiveWrite::SetFlag { .. })
            | AgentLiveWriteIntent::Mail(MailLiveWrite::SetCategories { .. })
            | AgentLiveWriteIntent::Todo(TodoLiveWrite::Complete { .. })
            | AgentLiveWriteIntent::Todo(TodoLiveWrite::ChecklistToggle { .. })
    );
    if allowed {
        return Ok(());
    }
    Err(format!(
        "not_available_on_mobile: live-write {}:{} is outside the mobile allowlist",
        intent.service(),
        intent.verb()
    ))
}

fn required_target(
    top_target: &Option<String>,
    change: &serde_json::Value,
) -> Result<String, String> {
    if let Some(target) = top_target.as_deref().filter(|s| !s.trim().is_empty()) {
        return validate_bounded_string("target", target, LIVE_WRITE_MAX_STRING);
    }
    required_change_str(change, "target")
}

fn change_object(
    change: &serde_json::Value,
) -> Result<&serde_json::Map<String, serde_json::Value>, String> {
    change
        .as_object()
        .ok_or_else(|| "invalid_live_write: change must be an object".to_string())
}

fn required_change_str(change: &serde_json::Value, key: &str) -> Result<String, String> {
    let value = change_object(change)?
        .get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("invalid_live_write: missing {key}"))?;
    validate_bounded_string(key, value, LIVE_WRITE_MAX_STRING)
}

fn optional_change_str(change: &serde_json::Value, key: &str) -> Result<Option<String>, String> {
    match change_object(change)?.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => {
            let s = value
                .as_str()
                .ok_or_else(|| format!("invalid_live_write: {key} must be a string"))?;
            validate_bounded_string(key, s, LIVE_WRITE_MAX_STRING).map(Some)
        }
    }
}

fn required_body_str(change: &serde_json::Value, keys: &[&str]) -> Result<String, String> {
    for key in keys {
        if let Some(value) = change_object(change)?.get(*key) {
            let s = value
                .as_str()
                .ok_or_else(|| format!("invalid_live_write: {key} must be a string"))?;
            return validate_bounded_string(key, s, LIVE_WRITE_MAX_BODY);
        }
    }
    Err(format!("invalid_live_write: missing {}", keys.join("|")))
}

fn validate_bounded_string(key: &str, value: &str, max: usize) -> Result<String, String> {
    if value.trim().is_empty() {
        return Err(format!("invalid_live_write: {key} is empty"));
    }
    if value.len() > max {
        return Err(format!("invalid_live_write: {key} is too long"));
    }
    if value.chars().any(char::is_control) && key != "body" && key != "body_html" && key != "text" {
        return Err(format!(
            "invalid_live_write: {key} contains control characters"
        ));
    }
    Ok(value.to_string())
}

fn required_change_bool(change: &serde_json::Value, key: &str) -> Result<bool, String> {
    change_object(change)?
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| format!("invalid_live_write: missing bool {key}"))
}

fn required_object(change: &serde_json::Value, key: &str) -> Result<serde_json::Value, String> {
    let value = change_object(change)?
        .get(key)
        .ok_or_else(|| format!("invalid_live_write: missing {key}"))?;
    if !value.is_object() {
        return Err(format!("invalid_live_write: {key} must be an object"));
    }
    let text = value.to_string();
    if text.len() > LIVE_WRITE_MAX_BODY {
        return Err(format!("invalid_live_write: {key} is too large"));
    }
    Ok(value.clone())
}

fn required_string_array(change: &serde_json::Value, key: &str) -> Result<Vec<String>, String> {
    let values = optional_string_array(change, key)?
        .ok_or_else(|| format!("invalid_live_write: missing {key}"))?;
    if values.is_empty() {
        return Err(format!("invalid_live_write: {key} is empty"));
    }
    Ok(values)
}

fn optional_string_array(
    change: &serde_json::Value,
    key: &str,
) -> Result<Option<Vec<String>>, String> {
    let Some(value) = change_object(change)?.get(key) else {
        return Ok(None);
    };
    let array = value
        .as_array()
        .ok_or_else(|| format!("invalid_live_write: {key} must be an array"))?;
    if array.len() > LIVE_WRITE_MAX_ARRAY {
        return Err(format!("invalid_live_write: {key} has too many values"));
    }
    let mut out = Vec::with_capacity(array.len());
    for value in array {
        let s = value
            .as_str()
            .ok_or_else(|| format!("invalid_live_write: {key} values must be strings"))?;
        out.push(validate_bounded_string(key, s, LIVE_WRITE_MAX_STRING)?);
    }
    Ok(Some(out))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AgentShareIntent {
    Link {
        link_type: String,
        scope: String,
    },
    Invite {
        recipients: Vec<String>,
        role: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShareRun {
    mode: String,
    service: String,
    item_id: String,
    link_type: Option<String>,
    scope: Option<String>,
    role: Option<String>,
    recipient_count: usize,
    summary: String,
}

fn run_share(
    cfg: &Config,
    action: &isyncyou_agent::ToolAction,
    gate: &Arc<Mutex<()>>,
) -> Result<ShareRun, String> {
    run_share_with_runtime(cfg, action, gate, &LiveShareRuntime)
}

trait ShareRuntime {
    fn share_link(
        &self,
        cfg: &Config,
        account: &str,
        service: &str,
        id: &str,
        link_type: &str,
        scope: &str,
    ) -> Result<String, String>;

    fn invite(
        &self,
        cfg: &Config,
        account: &str,
        service: &str,
        id: &str,
        recipients: &[String],
        role: &str,
    ) -> Result<String, String>;
}

struct LiveShareRuntime;

impl ShareRuntime for LiveShareRuntime {
    fn share_link(
        &self,
        cfg: &Config,
        account: &str,
        service: &str,
        id: &str,
        link_type: &str,
        scope: &str,
    ) -> Result<String, String> {
        let handler = crate::DaemonShare::new(cfg.clone());
        isyncyou_webui::ShareHandler::share(&handler, account, service, id, link_type, scope)
    }

    fn invite(
        &self,
        cfg: &Config,
        account: &str,
        service: &str,
        id: &str,
        recipients: &[String],
        role: &str,
    ) -> Result<String, String> {
        let handler = crate::DaemonShare::new(cfg.clone());
        isyncyou_webui::ShareHandler::invite(&handler, account, service, id, recipients, role)
    }
}

fn run_share_with_runtime<R: ShareRuntime>(
    cfg: &Config,
    action: &isyncyou_agent::ToolAction,
    gate: &Arc<Mutex<()>>,
    runtime: &R,
) -> Result<ShareRun, String> {
    let isyncyou_agent::ToolAction::Share {
        account,
        service,
        id,
        ..
    } = action
    else {
        return Err(format!("not_share_action: {}", action.op()));
    };
    let intent = validate_share_action(action)?;
    let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
    match intent {
        AgentShareIntent::Link { link_type, scope } => {
            let _web_url = runtime.share_link(cfg, account, service, id, &link_type, &scope)?;
            Ok(ShareRun {
                mode: "link".to_string(),
                service: service.to_string(),
                item_id: id.to_string(),
                link_type: Some(link_type),
                scope: Some(scope),
                role: None,
                recipient_count: 0,
                summary: "sharing link created".to_string(),
            })
        }
        AgentShareIntent::Invite { recipients, role } => {
            let summary = runtime.invite(cfg, account, service, id, &recipients, &role)?;
            Ok(ShareRun {
                mode: "invite".to_string(),
                service: service.to_string(),
                item_id: id.to_string(),
                link_type: None,
                scope: None,
                role: Some(role),
                recipient_count: recipients.len(),
                summary: redact_agent_operation_text(&summary),
            })
        }
    }
}

fn validate_share_action(action: &isyncyou_agent::ToolAction) -> Result<AgentShareIntent, String> {
    let isyncyou_agent::ToolAction::Share {
        service,
        id,
        mode,
        link_type,
        scope,
        recipients,
        role,
        recipient,
        ..
    } = action
    else {
        return Err(format!("not_share_action: {}", action.op()));
    };
    validate_service("share", service, SHARE_SERVICES)?;
    validate_share_item_id(id)?;

    let mut invite_recipients = recipients.clone();
    if let Some(recipient) = recipient {
        invite_recipients.push(recipient.clone());
    }
    let effective_mode = mode.as_deref().unwrap_or(if invite_recipients.is_empty() {
        "link"
    } else {
        "invite"
    });

    match effective_mode {
        "link" => {
            if !invite_recipients.is_empty() {
                return Err("invalid_share_request: link mode cannot include recipients".into());
            }
            let link_type = link_type.as_deref().unwrap_or("view");
            let scope = scope.as_deref().unwrap_or("anonymous");
            validate_named_value("share_link_type", link_type, SHARE_LINK_TYPES)?;
            validate_named_value("share_link_scope", scope, SHARE_LINK_SCOPES)?;
            Ok(AgentShareIntent::Link {
                link_type: link_type.to_string(),
                scope: scope.to_string(),
            })
        }
        "invite" => {
            if link_type.is_some() || scope.is_some() {
                return Err("invalid_share_request: invite mode cannot include link fields".into());
            }
            let role = role.as_deref().unwrap_or("read");
            validate_named_value("invite_role", role, SHARE_INVITE_ROLES)?;
            let recipients = normalize_share_recipients(&invite_recipients)?;
            Ok(AgentShareIntent::Invite {
                recipients,
                role: role.to_string(),
            })
        }
        _ => Err("invalid_share_request: unknown mode".to_string()),
    }
}

fn validate_named_value(kind: &str, value: &str, allowed: &[&str]) -> Result<(), String> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(format!("invalid_{kind}"))
    }
}

fn validate_share_item_id(item_id: &str) -> Result<(), String> {
    if item_id.is_empty() {
        return Err("share item id is empty".into());
    }
    if item_id.len() > 512 {
        return Err("share item id is too long".into());
    }
    Ok(())
}

fn normalize_share_recipients(recipients: &[String]) -> Result<Vec<String>, String> {
    if recipients.is_empty() {
        return Err("invite requires at least one recipient".into());
    }
    if recipients.len() > 20 {
        return Err("invite recipient count exceeds limit".into());
    }
    let mut normalized = Vec::new();
    for recipient in recipients {
        let trimmed = recipient.trim();
        if trimmed.is_empty() {
            return Err("invite recipient is empty".into());
        }
        if trimmed.len() > 320 {
            return Err("invite recipient is too long".into());
        }
        if trimmed.chars().any(char::is_control) || !trimmed.contains('@') {
            return Err("invite recipient is malformed".into());
        }
        let lowered = trimmed.to_ascii_lowercase();
        if !normalized.contains(&lowered) {
            normalized.push(lowered);
        }
    }
    normalized.sort();
    Ok(normalized)
}

pub(crate) fn preview_for_pending_action(
    action: &isyncyou_agent::ToolAction,
) -> Result<PendingOperationPreview, String> {
    if action.class() != isyncyou_agent::ToolClass::Destructive {
        return Err(format!("not_confirmable: {}", action.op()));
    }
    match action {
        isyncyou_agent::ToolAction::Backup { services, .. } => {
            validate_services("backup", services, BACKUP_SERVICES)?;
            let copy = if services.is_empty() {
                "Back up all supported Microsoft 365 data".to_string()
            } else {
                let count = services.len();
                format!(
                    "Back up {count} selected {} of Microsoft 365 data",
                    if count == 1 { "category" } else { "categories" }
                )
            };
            Ok(PendingOperationPreview::destructive(copy))
        }
        isyncyou_agent::ToolAction::RestoreCloud { service, .. } => {
            validate_service("restore-cloud", service, RESTORE_CLOUD_SERVICES)?;
            let item = match service.as_str() {
                "mail" => "email",
                "calendar" => "calendar event",
                "contacts" => "contact",
                "todo" => "task",
                "onenote" => "OneNote page",
                _ => "item",
            };
            Ok(PendingOperationPreview::destructive(format!(
                "Restore this archived {item} to Microsoft 365"
            )))
        }
        isyncyou_agent::ToolAction::LiveWrite { .. } => {
            let intent = validate_live_write_action(action)?;
            Ok(PendingOperationPreview::destructive(
                intent.confirmation_copy().to_string(),
            ))
        }
        isyncyou_agent::ToolAction::Share { .. } => {
            let intent = validate_share_action(action)?;
            match intent {
                AgentShareIntent::Invite { recipients, role } => {
                    let count = recipients.len();
                    let access = if role == "write" { "edit" } else { "view" };
                    Ok(PendingOperationPreview::destructive(format!(
                        "Give {count} {} {access} access to this OneDrive item",
                        if count == 1 {
                            "recipient"
                        } else {
                            "recipients"
                        }
                    )))
                }
                AgentShareIntent::Link { link_type, scope } => {
                    let access = match link_type.as_str() {
                        "edit" => "edit",
                        "embed" => "embedded-view",
                        _ => "view-only",
                    };
                    let copy = match scope.as_str() {
                        "anonymous" => {
                            format!("Create a public {access} link for this OneDrive item")
                        }
                        "organization" => format!(
                            "Create an organization-wide {access} link for this OneDrive item"
                        ),
                        "users" => format!(
                            "Create a {access} link for specific people to this OneDrive item"
                        ),
                        _ => format!("Create a restricted {access} link for this OneDrive item"),
                    };
                    Ok(PendingOperationPreview::destructive(copy))
                }
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
            } => self.restore_local(account, service, id, None, None),
            _ => self.delegate.execute_read(action),
        }
    }

    fn execute_read_bound(
        &self,
        action: &isyncyou_agent::ToolAction,
        binding: &isyncyou_agent::ReadExecutionBinding,
    ) -> Result<String, isyncyou_agent::AgentError> {
        match action {
            isyncyou_agent::ToolAction::RestoreLocal {
                account,
                service,
                id,
            } => self.restore_local(account, service, id, Some(binding), None),
            _ => self.delegate.execute_read_bound(action, binding),
        }
    }

    fn prepare_read_effect(
        &self,
        action: &isyncyou_agent::ToolAction,
        binding: &isyncyou_agent::ReadExecutionBinding,
    ) -> Result<Option<isyncyou_agent::LocalEffectCheckpointV1>, isyncyou_agent::AgentError> {
        match action {
            isyncyou_agent::ToolAction::RestoreLocal {
                account,
                service,
                id,
            } => self
                .planned_local_effect(account, service, id, binding)
                .map(Some),
            _ => self.delegate.prepare_read_effect(action, binding),
        }
    }

    fn execute_read_prepared(
        &self,
        action: &isyncyou_agent::ToolAction,
        binding: &isyncyou_agent::ReadExecutionBinding,
        local_effect: Option<&isyncyou_agent::LocalEffectCheckpointV1>,
    ) -> Result<String, isyncyou_agent::AgentError> {
        match action {
            isyncyou_agent::ToolAction::RestoreLocal {
                account,
                service,
                id,
            } => self.restore_local(account, service, id, Some(binding), local_effect),
            _ => self
                .delegate
                .execute_read_prepared(action, binding, local_effect),
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
        binding: Option<&isyncyou_agent::ReadExecutionBinding>,
        local_effect: Option<&isyncyou_agent::LocalEffectCheckpointV1>,
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
        let path = binding.map_or_else(
            || allocate_restore_path(&service_dir, &file_name),
            |binding| {
                Ok(deterministic_restore_path(
                    &service_dir,
                    &file_name,
                    service,
                    id,
                    binding,
                ))
            },
        )?;
        if let Some(binding) = binding {
            let planned = self.planned_local_effect(account, service, id, binding)?;
            if local_effect.is_some_and(|checkpoint| {
                checkpoint.relative_path != planned.relative_path
                    || checkpoint.source_sha256 != planned.source_sha256
                    || checkpoint.expected_file_sha256 != planned.expected_file_sha256
            }) {
                return Err(isyncyou_agent::AgentError::Provider(
                    "outcome_unknown".into(),
                ));
            }
        }
        ensure_under_root(&self.restore_root, path.parent().unwrap_or(&service_dir))?;
        if path.exists() {
            adopt_matching_owner_only_file(&path, &bytes)?;
        } else {
            write_owner_only_atomic(&path, &bytes).map_err(|e| {
                isyncyou_agent::AgentError::Provider(format!("restore-local write: {e}"))
            })?;
        }

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

    fn planned_local_effect(
        &self,
        account: &str,
        service: &str,
        id: &str,
        binding: &isyncyou_agent::ReadExecutionBinding,
    ) -> Result<isyncyou_agent::LocalEffectCheckpointV1, isyncyou_agent::AgentError> {
        if account != self.source.account() {
            return Err(isyncyou_agent::AgentError::ToolArgs(
                "account mismatch".into(),
            ));
        }
        let item = self
            .source
            .get(service, id)?
            .ok_or_else(|| isyncyou_agent::AgentError::ToolArgs("item not found".into()))?;
        let bytes = self.source.read_body(service, id)?;
        let service_dir = self.restore_root.join(safe_path_segment(service));
        let path = deterministic_restore_path(
            &service_dir,
            &restore_file_name(&item.name, &item.id),
            service,
            id,
            binding,
        );
        let relative_path = path
            .strip_prefix(&self.restore_root)
            .map_err(|_| isyncyou_agent::AgentError::Provider("outcome_unknown".into()))?
            .to_str()
            .ok_or_else(|| isyncyou_agent::AgentError::Provider("outcome_unknown".into()))?
            .to_owned();
        let digest = content_sha256(&bytes);
        Ok(isyncyou_agent::LocalEffectCheckpointV1 {
            relative_path,
            source_sha256: digest.clone(),
            expected_file_sha256: digest,
            state: isyncyou_agent::LocalEffectState::Planned,
        })
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
fn deterministic_restore_path(
    dir: &Path,
    file_name: &str,
    service: &str,
    source_item_id: &str,
    binding: &isyncyou_agent::ReadExecutionBinding,
) -> PathBuf {
    let mut material = Vec::new();
    material.extend_from_slice(b"isyncyou-restore-local-v1");
    for value in [
        binding.session_id.as_str(),
        binding.request_id.as_str(),
        binding.tool_use_id.as_str(),
        service,
        source_item_id,
    ] {
        material.extend_from_slice(&(value.len() as u64).to_be_bytes());
        material.extend_from_slice(value.as_bytes());
    }
    let digest = ring::digest::digest(&ring::digest::SHA256, &material);
    let stem = digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let (_, extension) = split_extension(file_name);
    let extension = if extension.len() <= 17 {
        extension
    } else {
        String::new()
    };
    dir.join(format!("{stem}{extension}"))
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn content_sha256(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(any(
    feature = "agent-oauth-providers",
    feature = "agent-subscription-experimental"
))]
fn adopt_matching_owner_only_file(
    path: &Path,
    expected: &[u8],
) -> Result<(), isyncyou_agent::AgentError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| isyncyou_agent::AgentError::Provider("outcome_unknown".into()))?;
    if !metadata.file_type().is_file() || std::fs::read(path).ok().as_deref() != Some(expected) {
        return Err(isyncyou_agent::AgentError::Provider(
            "outcome_unknown".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(isyncyou_agent::AgentError::Provider(
                "outcome_unknown".into(),
            ));
        }
    }
    Ok(())
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
            isyncyou_agent::ToolAction::LiveWrite { account, .. } => {
                let run = run_live_write(&self.cfg, action, &self.gate)?;
                Ok(ConfirmedActionResult::new(
                    serde_json::json!({
                        "op": "live-write",
                        "account": redact_agent_operation_text(account),
                        "service": run.service,
                        "verb": run.verb,
                        "target": run.target,
                        "result_id": run.result_id,
                        "summary": run.summary,
                    })
                    .to_string(),
                ))
            }
            isyncyou_agent::ToolAction::Share { account, .. } => {
                let run = run_share(&self.cfg, action, &self.gate)?;
                Ok(ConfirmedActionResult::new(
                    serde_json::json!({
                        "op": "share",
                        "account": redact_agent_operation_text(account),
                        "service": run.service,
                        "item_id": redact_agent_operation_text(&run.item_id),
                        "mode": run.mode,
                        "link_type": run.link_type,
                        "scope": run.scope,
                        "role": run.role,
                        "recipient_count": run.recipient_count,
                        "summary": run.summary,
                    })
                    .to_string(),
                ))
            }
            _ => Err(format!(
                "not_confirmable: confirmed agent action '{}' is not a supported desktop operation",
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
        Err(
            "not_available_on_mobile: mobile disabled policy refused this confirmed operation"
                .to_string(),
        )
    }
}

pub(crate) struct MobileFullNodeAgentOperations {
    cfg: Config,
    gate: Arc<Mutex<()>>,
    mobile_jobs: Arc<MobileJobRuntime>,
}

impl MobileFullNodeAgentOperations {
    pub(crate) fn new(
        cfg: Config,
        gate: Arc<Mutex<()>>,
        mobile_jobs: Arc<MobileJobRuntime>,
    ) -> Self {
        Self {
            cfg,
            gate,
            mobile_jobs,
        }
    }

    fn queued_job_result(
        op: &str,
        account: &str,
        job: &MobileJob,
        extra: serde_json::Value,
    ) -> ConfirmedActionResult {
        let mut body = serde_json::json!({
            "op": op,
            "account": redact_agent_operation_text(account),
            "queued": true,
            "job_id": job.job_id,
            "kind": job.kind.as_str(),
            "state": job.state.as_str(),
        });
        if let (Some(dst), Some(src)) = (body.as_object_mut(), extra.as_object()) {
            for (key, value) in src {
                dst.insert(key.clone(), value.clone());
            }
        }
        ConfirmedActionResult::new(body.to_string())
    }
}

impl AgentConfirmedActionExecutor for MobileFullNodeAgentOperations {
    fn execute_confirmed(
        &self,
        action: &isyncyou_agent::ToolAction,
    ) -> Result<ConfirmedActionResult, String> {
        match action {
            isyncyou_agent::ToolAction::Backup { account, services } => {
                let job = self.mobile_jobs.enqueue_backup(account, services)?;
                Ok(Self::queued_job_result(
                    "backup",
                    account,
                    &job,
                    serde_json::json!({ "services": services }),
                ))
            }
            isyncyou_agent::ToolAction::RestoreCloud {
                account,
                service,
                id,
            } => {
                let job = self
                    .mobile_jobs
                    .enqueue_restore_cloud(account, service, id)?;
                Ok(Self::queued_job_result(
                    "restore-cloud",
                    account,
                    &job,
                    serde_json::json!({
                        "service": service,
                        "source_id": redact_agent_operation_text(id),
                    }),
                ))
            }
            isyncyou_agent::ToolAction::LiveWrite { account, .. } => {
                ensure_mobile_live_write_allowlisted(action)?;
                let run = run_live_write(&self.cfg, action, &self.gate)?;
                Ok(ConfirmedActionResult::new(
                    serde_json::json!({
                        "op": "live-write",
                        "account": redact_agent_operation_text(account),
                        "service": run.service,
                        "verb": run.verb,
                        "target": run.target,
                        "result_id": run.result_id,
                        "summary": run.summary,
                    })
                    .to_string(),
                ))
            }
            isyncyou_agent::ToolAction::Share { account, .. } => {
                let run = run_share(&self.cfg, action, &self.gate)?;
                Ok(ConfirmedActionResult::new(
                    serde_json::json!({
                        "op": "share",
                        "account": redact_agent_operation_text(account),
                        "service": run.service,
                        "item_id": redact_agent_operation_text(&run.item_id),
                        "mode": run.mode,
                        "link_type": run.link_type,
                        "scope": run.scope,
                        "role": run.role,
                        "recipient_count": run.recipient_count,
                        "summary": run.summary,
                    })
                    .to_string(),
                ))
            }
            _ => Err(format!(
                "not_confirmable: confirmed agent action '{}' is not a supported mobile full-node operation",
                action.op()
            )),
        }
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

    #[derive(Debug, Default)]
    struct LiveWriteRuntimeState {
        calls: Vec<(String, AgentLiveWriteIntent)>,
    }

    #[derive(Clone)]
    struct RecordingLiveWriteRuntime {
        state: Arc<StdMutex<LiveWriteRuntimeState>>,
        result: Result<Option<String>, String>,
    }

    impl RecordingLiveWriteRuntime {
        fn new() -> Self {
            Self {
                state: Arc::new(StdMutex::new(LiveWriteRuntimeState::default())),
                result: Ok(None),
            }
        }

        fn with_result(result: Option<&str>) -> Self {
            Self {
                result: Ok(result.map(str::to_string)),
                ..Self::new()
            }
        }

        fn state(&self) -> std::sync::MutexGuard<'_, LiveWriteRuntimeState> {
            self.state.lock().unwrap()
        }
    }

    impl LiveWriteRuntime for RecordingLiveWriteRuntime {
        fn execute_live_write(
            &self,
            _cfg: &Config,
            account: &str,
            intent: &AgentLiveWriteIntent,
        ) -> Result<Option<String>, String> {
            self.state
                .lock()
                .unwrap()
                .calls
                .push((account.to_string(), intent.clone()));
            self.result.clone()
        }
    }

    #[derive(Debug, Default)]
    struct ShareRuntimeState {
        link_calls: Vec<(String, String, String, String, String)>,
        invite_calls: Vec<(String, String, String, Vec<String>, String)>,
    }

    #[derive(Clone)]
    struct RecordingShareRuntime {
        state: Arc<StdMutex<ShareRuntimeState>>,
        link_result: Result<String, String>,
        invite_result: Result<String, String>,
    }

    impl RecordingShareRuntime {
        fn new() -> Self {
            Self {
                state: Arc::new(StdMutex::new(ShareRuntimeState::default())),
                link_result: Ok("https://tenant.sharepoint.com/:w:/r/sites/x?code=secret".into()),
                invite_result: Ok(
                    "invited alpha@example.com via https://tenant.example/link".into()
                ),
            }
        }

        fn state(&self) -> std::sync::MutexGuard<'_, ShareRuntimeState> {
            self.state.lock().unwrap()
        }
    }

    impl ShareRuntime for RecordingShareRuntime {
        fn share_link(
            &self,
            _cfg: &Config,
            account: &str,
            service: &str,
            id: &str,
            link_type: &str,
            scope: &str,
        ) -> Result<String, String> {
            self.state.lock().unwrap().link_calls.push((
                account.to_string(),
                service.to_string(),
                id.to_string(),
                link_type.to_string(),
                scope.to_string(),
            ));
            self.link_result.clone()
        }

        fn invite(
            &self,
            _cfg: &Config,
            account: &str,
            service: &str,
            id: &str,
            recipients: &[String],
            role: &str,
        ) -> Result<String, String> {
            self.state.lock().unwrap().invite_calls.push((
                account.to_string(),
                service.to_string(),
                id.to_string(),
                recipients.to_vec(),
                role.to_string(),
            ));
            self.invite_result.clone()
        }
    }

    fn restore_enabled_config() -> Config {
        let mut cfg = Config::default();
        cfg.restore.cloud_restore_enabled = true;
        cfg
    }

    fn run_recorded_live_write(
        action: &isyncyou_agent::ToolAction,
        runtime: &RecordingLiveWriteRuntime,
    ) -> LiveWriteRun {
        let gate = Arc::new(Mutex::new(()));
        run_live_write_with_runtime(&Config::default(), action, &gate, runtime).unwrap()
    }

    #[test]
    fn agent_live_write_mail_set_read_routes_existing_writer() {
        let action = isyncyou_agent::parse_action(&json!({
            "op": "live-write",
            "account": "me",
            "service": "mail",
            "target": "msg-1",
            "change": { "verb": "set_read", "is_read": true }
        }))
        .unwrap();
        let runtime = RecordingLiveWriteRuntime::new();

        let run = run_recorded_live_write(&action, &runtime);

        assert_eq!(run.service, "mail");
        assert_eq!(run.verb, "set_read");
        assert_eq!(
            runtime.state().calls,
            vec![(
                "me".to_string(),
                AgentLiveWriteIntent::Mail(MailLiveWrite::SetRead {
                    target: "msg-1".to_string(),
                    is_read: true,
                })
            )]
        );
    }

    #[test]
    fn agent_live_write_mail_move_routes_existing_writer() {
        let action = isyncyou_agent::parse_action(&json!({
            "op": "live-write",
            "account": "me",
            "service": "mail",
            "target": "msg-1",
            "change": { "verb": "move", "destination_id": "archive-folder" }
        }))
        .unwrap();
        let runtime = RecordingLiveWriteRuntime::with_result(Some("msg-1-moved"));

        let run = run_recorded_live_write(&action, &runtime);

        assert_eq!(run.result_id.as_deref(), Some("msg-1-moved"));
        assert_eq!(
            runtime.state().calls,
            vec![(
                "me".to_string(),
                AgentLiveWriteIntent::Mail(MailLiveWrite::Move {
                    target: "msg-1".to_string(),
                    destination_id: "archive-folder".to_string(),
                })
            )]
        );
    }

    #[test]
    fn agent_live_write_calendar_update_routes_existing_writer() {
        let event = json!({ "subject": "Renamed" });
        let action = isyncyou_agent::parse_action(&json!({
            "op": "live-write",
            "account": "me",
            "service": "calendar",
            "target": "event-1",
            "change": { "verb": "update", "event": event }
        }))
        .unwrap();
        let runtime = RecordingLiveWriteRuntime::new();

        let run = run_recorded_live_write(&action, &runtime);

        assert_eq!(run.service, "calendar");
        assert_eq!(
            runtime.state().calls,
            vec![(
                "me".to_string(),
                AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Update {
                    target: "event-1".to_string(),
                    event,
                })
            )]
        );
    }

    #[test]
    fn agent_live_write_contact_update_routes_existing_writer() {
        let contact = json!({ "displayName": "Updated Name" });
        let action = isyncyou_agent::parse_action(&json!({
            "op": "live-write",
            "account": "me",
            "service": "contacts",
            "target": "contact-1",
            "change": { "verb": "update", "contact": contact }
        }))
        .unwrap();
        let runtime = RecordingLiveWriteRuntime::new();

        let run = run_recorded_live_write(&action, &runtime);

        assert_eq!(run.service, "contacts");
        assert_eq!(
            runtime.state().calls,
            vec![(
                "me".to_string(),
                AgentLiveWriteIntent::Contacts(ContactLiveWrite::Update {
                    target: "contact-1".to_string(),
                    contact,
                })
            )]
        );
    }

    #[test]
    fn agent_live_write_todo_complete_routes_existing_writer() {
        let action = isyncyou_agent::parse_action(&json!({
            "op": "live-write",
            "account": "me",
            "service": "todo",
            "target": "task-1",
            "change": { "verb": "complete", "list_id": "list-1" }
        }))
        .unwrap();
        let runtime = RecordingLiveWriteRuntime::new();

        let run = run_recorded_live_write(&action, &runtime);

        assert_eq!(run.service, "todo");
        assert_eq!(
            runtime.state().calls,
            vec![(
                "me".to_string(),
                AgentLiveWriteIntent::Todo(TodoLiveWrite::Complete {
                    list_id: "list-1".to_string(),
                    target: "task-1".to_string(),
                })
            )]
        );
    }

    #[test]
    fn agent_live_write_does_not_wait_for_mobile_store_gate() {
        let action = isyncyou_agent::parse_action(&json!({
            "op": "live-write",
            "account": "me",
            "service": "todo",
            "target": "task-1",
            "change": { "verb": "complete", "list_id": "list-1" }
        }))
        .unwrap();
        let runtime = RecordingLiveWriteRuntime::new();
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let (tx, rx) = mpsc::channel();
        let worker_gate = Arc::clone(&gate);

        let worker = std::thread::spawn(move || {
            let result =
                run_live_write_with_runtime(&Config::default(), &action, &worker_gate, &runtime);
            tx.send(result.is_ok()).unwrap();
        });

        assert!(rx.recv_timeout(Duration::from_secs(1)).unwrap());
        drop(held);
        worker.join().unwrap();
    }

    #[test]
    fn agent_live_write_onenote_append_routes_existing_writer() {
        let action = isyncyou_agent::parse_action(&json!({
            "op": "live-write",
            "account": "me",
            "service": "onenote",
            "target": "page-1",
            "change": { "verb": "append", "text": "Append this paragraph" }
        }))
        .unwrap();
        let runtime = RecordingLiveWriteRuntime::new();

        let run = run_recorded_live_write(&action, &runtime);

        assert_eq!(run.service, "onenote");
        assert_eq!(
            runtime.state().calls,
            vec![(
                "me".to_string(),
                AgentLiveWriteIntent::OneNote(OneNoteLiveWrite::Append {
                    target: "page-1".to_string(),
                    text: "Append this paragraph".to_string(),
                })
            )]
        );
    }

    #[test]
    fn agent_live_write_rejects_unknown_service_or_verb_before_pending() {
        let bad_service = isyncyou_agent::parse_action(&json!({
            "op": "live-write",
            "account": "me",
            "service": "onedrive",
            "target": "item-1",
            "change": { "verb": "rename" }
        }))
        .unwrap();
        let err = preview_for_pending_action(&bad_service).unwrap_err();
        assert!(err.contains("unsupported_live_write_service"));

        let bad_verb = isyncyou_agent::parse_action(&json!({
            "op": "live-write",
            "account": "me",
            "service": "mail",
            "target": "msg-1",
            "change": { "verb": "delete_forever" }
        }))
        .unwrap();
        let err = preview_for_pending_action(&bad_verb).unwrap_err();
        assert!(err.contains("unsupported_live_write_verb"));

        let runtime = RecordingLiveWriteRuntime::new();
        let gate = Arc::new(Mutex::new(()));
        let err = run_live_write_with_runtime(&Config::default(), &bad_verb, &gate, &runtime)
            .unwrap_err();
        assert!(err.contains("unsupported_live_write_verb"));
        assert!(runtime.state().calls.is_empty());
    }

    #[test]
    fn mobile_live_write_allowlist_accepts_only_metadata_verbs() {
        for action in [
            json!({
                "op": "live-write",
                "account": "me",
                "service": "mail",
                "target": "msg-1",
                "change": { "verb": "set_read", "is_read": true }
            }),
            json!({
                "op": "live-write",
                "account": "me",
                "service": "mail",
                "target": "msg-1",
                "change": { "verb": "set_flag", "flag_status": "flagged" }
            }),
            json!({
                "op": "live-write",
                "account": "me",
                "service": "mail",
                "target": "msg-1",
                "change": { "verb": "set_categories", "categories": ["Follow-up"] }
            }),
            json!({
                "op": "live-write",
                "account": "me",
                "service": "todo",
                "target": "task-1",
                "change": { "verb": "complete", "list_id": "list-1" }
            }),
            json!({
                "op": "live-write",
                "account": "me",
                "service": "todo",
                "target": "task-1",
                "change": {
                    "verb": "checklist_toggle",
                    "list_id": "list-1",
                    "item_id": "check-1",
                    "checked": true
                }
            }),
        ] {
            let action = isyncyou_agent::parse_action(&action).unwrap();
            ensure_mobile_live_write_allowlisted(&action).unwrap();
        }
    }

    #[test]
    fn mobile_live_write_disallowed_create_send_delete_append_verbs_fail_closed() {
        for action in [
            json!({
                "op": "live-write",
                "account": "me",
                "service": "mail",
                "target": "msg-1",
                "change": { "verb": "move", "destination_id": "archive-folder" }
            }),
            json!({
                "op": "live-write",
                "account": "me",
                "service": "mail",
                "change": {
                    "verb": "create_draft",
                    "body_html": "<p>private</p>",
                    "to": ["recipient@example.com"]
                }
            }),
            json!({
                "op": "live-write",
                "account": "me",
                "service": "calendar",
                "target": "event-1",
                "change": { "verb": "delete" }
            }),
            json!({
                "op": "live-write",
                "account": "me",
                "service": "todo",
                "target": "task-1",
                "change": { "verb": "delete", "list_id": "list-1" }
            }),
            json!({
                "op": "live-write",
                "account": "me",
                "service": "onenote",
                "target": "page-1",
                "change": { "verb": "append", "text": "private note body" }
            }),
        ] {
            let action = isyncyou_agent::parse_action(&action).unwrap();
            let err = ensure_mobile_live_write_allowlisted(&action).unwrap_err();
            assert!(
                err.contains("not_available_on_mobile"),
                "disallowed mobile live-write must fail closed: {err}"
            );
            assert!(
                !err.contains("private")
                    && !err.contains("recipient@example.com")
                    && !err.contains("private note body"),
                "policy error must be redacted: {err}"
            );
        }
    }

    #[test]
    fn agent_live_write_body_html_is_not_audited() {
        let action = isyncyou_agent::parse_action(&json!({
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
        let preview = preview_for_pending_action(&action).unwrap();
        let runtime = RecordingLiveWriteRuntime::with_result(Some("draft-1"));
        let run = run_recorded_live_write(&action, &runtime);
        let text = format!("{} {}", preview.text, run.summary);

        assert_eq!(preview.text, "Create an email draft");
        assert!(!preview.text.contains("owner@example.com"));
        assert!(!preview.text.contains("live-write"));
        assert!(!preview.text.contains("create_draft"));
        assert!(!text.contains("raw-body-sentinel"));
        assert!(!text.contains("recipient@example.com"));
        assert!(!text.contains("private subject"));
        for internal in ["live-write", "create_draft", "owner@example.com"] {
            assert!(!preview.text.contains(internal));
        }
    }

    #[test]
    fn agent_live_write_confirmation_uses_plain_language_without_internal_fields() {
        for (is_read, expected) in [
            (true, "Mark this email as read"),
            (false, "Mark this email as unread"),
        ] {
            let action = isyncyou_agent::parse_action(&json!({
                "op": "live-write",
                "account": "me",
                "service": "mail",
                "target": "private-item-id",
                "change": { "verb": "set_read", "is_read": is_read }
            }))
            .unwrap();

            let preview = preview_for_pending_action(&action).unwrap();

            assert_eq!(preview.text, expected);
            for internal in ["live-write", "set_read", "account", "private-item-id"] {
                assert!(!preview.text.contains(internal));
            }
        }
    }

    #[test]
    fn desktop_agent_operations_impl_does_not_call_graphclient_mutations_directly() {
        let source = include_str!("agent_ops.rs");
        let start = source
            .find("impl AgentConfirmedActionExecutor for DesktopAgentOperations")
            .expect("desktop executor impl");
        let end = source[start..]
            .find("impl AgentConfirmedActionExecutor for MobileDisabledAgentOperations")
            .expect("mobile executor impl");
        let block = &source[start..start + end];

        assert!(!block.contains("GraphClient"));
        assert!(!block.contains(".create_link("));
        assert!(!block.contains(".invite("));
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

        assert_eq!(
            preview.text,
            "Give 1 recipient view access to this OneDrive item"
        );
        assert!(!preview.text.contains("recipient@example.com"));
        assert!(!preview.text.contains("file-1"));
        assert!(!preview.text.contains("account"));
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
    fn agent_share_rejects_non_onedrive_before_pending() {
        let action = isyncyou_agent::parse_action(&json!({
            "op": "share",
            "account": "me",
            "service": "mail",
            "id": "item-1",
            "mode": "link"
        }))
        .unwrap();

        let err = preview_for_pending_action(&action).unwrap_err();

        assert!(err.contains("unsupported_share_service"));

        let runtime = RecordingShareRuntime::new();
        let gate = Arc::new(Mutex::new(()));
        let err = run_share_with_runtime(&Config::default(), &action, &gate, &runtime).unwrap_err();
        assert!(err.contains("unsupported_share_service"));
        let state = runtime.state();
        assert!(state.link_calls.is_empty());
        assert!(state.invite_calls.is_empty());
    }

    #[test]
    fn agent_share_link_accepts_organization_scope() {
        let action = isyncyou_agent::parse_action(&json!({
            "op": "share",
            "account": "me",
            "service": "onedrive",
            "id": "item-1",
            "mode": "link",
            "link_type": "edit",
            "scope": "organization"
        }))
        .unwrap();

        let preview = preview_for_pending_action(&action).unwrap();

        assert_eq!(
            preview.text,
            "Create an organization-wide edit link for this OneDrive item"
        );
        assert!(!preview.text.contains("item-1"));
        assert!(!preview.text.contains("account"));
        assert_eq!(preview.risk, "destructive");
    }

    #[test]
    fn agent_share_link_confirm_routes_through_ledger_handler() {
        let action = isyncyou_agent::parse_action(&json!({
            "op": "share",
            "account": "me",
            "service": "onedrive",
            "id": "item-1",
            "mode": "link",
            "link_type": "view",
            "scope": "organization"
        }))
        .unwrap();
        let runtime = RecordingShareRuntime::new();
        let gate = Arc::new(Mutex::new(()));

        let run = run_share_with_runtime(&Config::default(), &action, &gate, &runtime).unwrap();

        assert_eq!(run.mode, "link");
        assert_eq!(run.link_type.as_deref(), Some("view"));
        assert_eq!(run.scope.as_deref(), Some("organization"));
        assert_eq!(run.summary, "sharing link created");
        assert!(!run.summary.contains("sharepoint.com"));
        let state = runtime.state();
        assert_eq!(
            state.link_calls,
            vec![(
                "me".to_string(),
                "onedrive".to_string(),
                "item-1".to_string(),
                "view".to_string(),
                "organization".to_string(),
            )]
        );
        assert!(state.invite_calls.is_empty());
    }

    #[test]
    fn agent_share_invite_confirm_routes_through_ledger_handler() {
        let action = isyncyou_agent::parse_action(&json!({
            "op": "share",
            "account": "me",
            "service": "onedrive",
            "id": "item-1",
            "mode": "invite",
            "recipients": ["Beta@example.com", "alpha@example.com", "ALPHA@example.com"],
            "role": "write"
        }))
        .unwrap();
        let runtime = RecordingShareRuntime::new();
        let gate = Arc::new(Mutex::new(()));

        let run = run_share_with_runtime(&Config::default(), &action, &gate, &runtime).unwrap();

        assert_eq!(run.mode, "invite");
        assert_eq!(run.role.as_deref(), Some("write"));
        assert_eq!(run.recipient_count, 2);
        assert!(!run.summary.contains("alpha@example.com"));
        assert!(!run.summary.contains("tenant.example"));
        assert!(run.summary.contains("<redacted-email>"));
        assert!(run.summary.contains("<redacted-url>"));
        let state = runtime.state();
        assert!(state.link_calls.is_empty());
        assert_eq!(
            state.invite_calls,
            vec![(
                "me".to_string(),
                "onedrive".to_string(),
                "item-1".to_string(),
                vec![
                    "alpha@example.com".to_string(),
                    "beta@example.com".to_string()
                ],
                "write".to_string(),
            )]
        );
    }

    #[test]
    fn agent_share_invite_preview_and_result_redact_emails() {
        let action = isyncyou_agent::parse_action(&json!({
            "op": "share",
            "account": "me",
            "service": "onedrive",
            "id": "item-1",
            "mode": "invite",
            "recipients": ["recipient@example.com"],
            "role": "read"
        }))
        .unwrap();

        let preview = preview_for_pending_action(&action).unwrap();
        let runtime = RecordingShareRuntime::new();
        let gate = Arc::new(Mutex::new(()));
        let run = run_share_with_runtime(&Config::default(), &action, &gate, &runtime).unwrap();

        assert_eq!(
            preview.text,
            "Give 1 recipient view access to this OneDrive item"
        );
        assert!(!preview.text.contains("recipient@example.com"));
        assert_eq!(run.recipient_count, 1);
        assert!(!run.summary.contains("recipient@example.com"));
        assert!(run.summary.contains("<redacted-email>"));
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
    fn agent_backup_failure_records_only_closed_summary() {
        let sensitive = "refresh failed for user@example.invalid at https://provider.invalid/token";
        let runtime = RecordingBackupRuntime::failing_read(sensitive);
        let gate = Arc::new(Mutex::new(()));

        let error = run_backup_account_with_runtime(
            &Config::default(),
            "private-alias",
            &gate,
            &[],
            &runtime,
        )
        .unwrap_err();

        assert_eq!(error, sensitive);
        assert_eq!(
            runtime.state().recorded,
            vec![("error".to_string(), "backup failed".to_string())]
        );
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
