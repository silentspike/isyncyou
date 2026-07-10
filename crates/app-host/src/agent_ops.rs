use std::sync::Arc;

use isyncyou_core::Config;

use crate::{AgentConfirmedActionExecutor, ConfirmedActionResult};

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
    if allowed.iter().any(|candidate| *candidate == service) {
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
    if allowed.iter().any(|candidate| *candidate == verb) {
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
