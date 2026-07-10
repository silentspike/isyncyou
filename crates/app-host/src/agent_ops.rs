use std::sync::Arc;

use isyncyou_core::Config;

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
) -> Arc<dyn AgentConfirmedActionExecutor> {
    match policy {
        AgentOperationPolicy::DesktopEnabled => Arc::new(DesktopAgentOperations::new(cfg)),
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
    _cfg: Config,
}

impl DesktopAgentOperations {
    pub(crate) fn new(cfg: Config) -> Self {
        Self { _cfg: cfg }
    }
}

impl AgentConfirmedActionExecutor for DesktopAgentOperations {
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
}
