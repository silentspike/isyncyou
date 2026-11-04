use std::sync::{Arc, Mutex};

use isyncyou_core::Config;
use isyncyou_store::MobileJob;
use isyncyou_store::Store;

use crate::mobile_jobs::MobileJobRuntime;
use crate::{
    AgentConfirmedActionExecutor, ClosedExecutionCode, ConfirmedActionResult,
    ConfirmedExecutionOutcome,
};

fn classify_confirmed_execution(
    action: &isyncyou_agent::ToolAction,
    execute: impl FnOnce() -> Result<ConfirmedActionResult, String>,
) -> ConfirmedExecutionOutcome {
    if action.policy() != isyncyou_agent::ToolPolicy::ConfirmedEffectNeverRepeat
        || preview_for_pending_action(action).is_err()
    {
        return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected);
    }
    match execute() {
        Ok(result) => ConfirmedExecutionOutcome::Completed(result),
        // The existing writers/jobs expose strings, not authoritative execution
        // certainty. Once dispatched, an error can mean a response was lost after
        // acceptance. Never turn that uncertainty into a terminal failed claim.
        Err(_) => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn submission_outcome(outcome: isyncyou_engine::self_hosted_mail::SendOutcome) -> ConfirmedExecutionOutcome {
    match outcome {
        isyncyou_engine::self_hosted_mail::SendOutcome::Submitted { sent_update, .. } => ConfirmedExecutionOutcome::Completed(
            ConfirmedActionResult::new(format!("Message submitted; delivery not confirmed; Sent update {sent_update}")),
        ),
        isyncyou_engine::self_hosted_mail::SendOutcome::Rejected => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderRejected),
        isyncyou_engine::self_hosted_mail::SendOutcome::Unknown | isyncyou_engine::self_hosted_mail::SendOutcome::Confirmed(_) => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn bound_submission_operation_id(pending_id: &str) -> Option<String> {
    if pending_id.is_empty() || pending_id.len() > 128 || pending_id.chars().any(char::is_control) { None }
    else { Some(format!("agent:{pending_id}")) }
}

fn execute_bound_provider_submission(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let intent = match validate_live_write_action(action) {
        Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::SendApproved { origin, email_id, blob_id, identity_id, mail_from, recipients, draft_mailbox_id, sent_mailbox_id })) =>
            (origin, email_id, blob_id, identity_id, mail_from, recipients, draft_mailbox_id, sent_mailbox_id),
        _ => return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected),
    };
    let account = action.account();
    if !matches!(cfg.provider_accounts.get(account), Some(isyncyou_core::config::ProviderConfig::SelfHosted { origin, .. }) if origin == &intent.0) {
        return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected);
    }
    let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    let result = (|| {
        let store = isyncyou_engine::provider_store::open(cfg, account)?;
        let client = isyncyou_engine::self_hosted_mail::client(cfg, account)?;
        let approved = isyncyou_engine::self_hosted_mail::ApprovedSubmission {
            operation_id, email_id:intent.1, blob_id:intent.2, identity_id:intent.3,
            mail_from:intent.4, recipients:intent.5, draft_mailbox_id:intent.6, sent_mailbox_id:intent.7,
        };
        isyncyou_engine::self_hosted_mail::send_approved(cfg, account, &store, &client, &approved)
    })();
    match result { Ok(outcome) => submission_outcome(outcome), Err(_) => ConfirmedExecutionOutcome::OutcomeUnknown }
}

fn execute_bound_provider_read_state(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let isyncyou_agent::ToolAction::LiveWrite { account, change, .. } = action else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::SetRead { target, is_read })) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    if !matches!(cfg.provider_accounts.get(account), Some(isyncyou_core::config::ProviderConfig::SelfHosted { origin, .. }) if change.get("origin").and_then(serde_json::Value::as_str) == Some(origin.as_str())) {
        return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected);
    }
    let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    let result = (|| {
        let store = isyncyou_engine::provider_store::open(cfg, account)?;
        let client = isyncyou_engine::self_hosted_mail::client(cfg, account)?;
        let outcome = isyncyou_engine::self_hosted_mail::set_read(cfg, account, &store, &client, &operation_id, &target, is_read)?;
        let synced = if matches!(&outcome, isyncyou_engine::self_hosted_mail::SendOutcome::Confirmed(_)) { isyncyou_engine::self_hosted_mail::sync(cfg, account, &store, &client).is_ok() } else { false };
        Ok::<_, String>((outcome, synced))
    })();
    match result {
        Ok((isyncyou_engine::self_hosted_mail::SendOutcome::Confirmed(_), synced)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new(if synced { "Email read state updated" } else { "Email read state updated; local synchronization pending" }.to_string())),
        Ok((isyncyou_engine::self_hosted_mail::SendOutcome::Rejected, _)) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected),
        _ => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn execute_bound_provider_mail_action(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::ProviderAction { preview })) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(preview) = serde_json::from_value::<isyncyou_engine::self_hosted_mail::MailActionPreview>(preview) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let account = action.account();
    if preview.account != account || !matches!(cfg.provider_accounts.get(account), Some(isyncyou_core::config::ProviderConfig::SelfHosted { origin, .. }) if origin == &preview.origin) { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); }
    let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    let result = (|| { let store = isyncyou_engine::provider_store::open(cfg,account)?; let client = isyncyou_engine::self_hosted_mail::client(cfg,account)?;
        isyncyou_engine::self_hosted_mail::apply_mail_action(cfg,account,&store,&client,&operation_id,&preview) })();
    match result {
        Ok(isyncyou_engine::self_hosted_mail::SendOutcome::Confirmed(_)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new("Email updated; local synchronization pending")),
        Ok(isyncyou_engine::self_hosted_mail::SendOutcome::Rejected) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected),
        _ => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn execute_bound_related_draft(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::RelatedDraft { preview, email })) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(preview) = serde_json::from_value::<isyncyou_engine::self_hosted_mail::RelatedDraftPreview>(preview) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let account = action.account();
    if preview.account != account || !matches!(cfg.provider_accounts.get(account), Some(isyncyou_core::config::ProviderConfig::SelfHosted { origin, .. }) if origin == &preview.origin) { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); }
    let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    if provider_drafts_mailbox(cfg,account).ok().as_deref() != email["mailboxIds"].as_object().and_then(|m| m.keys().next()).map(String::as_str) { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict); }
    let result = (|| { let store = isyncyou_engine::provider_store::open(cfg,account)?; let client = isyncyou_engine::self_hosted_mail::client(cfg,account)?;
        isyncyou_engine::self_hosted_mail::create_related_draft(cfg,account,&store,&client,&operation_id,&preview,&email) })();
    match result {
        Ok(isyncyou_engine::self_hosted_mail::SendOutcome::Confirmed(_)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new("Draft created; message not sent; local synchronization pending")),
        Ok(isyncyou_engine::self_hosted_mail::SendOutcome::Rejected) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected),
        _ => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn execute_bound_task_parent(cfg:&Config, action:&isyncyou_agent::ToolAction, pending_id:&str, gate:&Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::TaskParent {target,origin,collection,uid,summary,old_parent,new_parent,etag,checkpoint,raw_sha256})) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let account = action.account(); let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    let prepared = match isyncyou_engine::dav_live::prepare_task_parent_preview(cfg,account,&collection,&target,&uid,new_parent.as_deref()) { Ok(value)=>value, Err(_)=>return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
    if prepared.origin != origin || prepared.uid != uid || prepared.summary != summary || prepared.old_parent != old_parent || prepared.new_parent != new_parent || prepared.edit.href != target || prepared.edit.expected_etag != etag || prepared.edit.expected_collection_token.as_deref() != Some(checkpoint.as_str()) || sha256_hex(&prepared.edit.raw) != raw_sha256 { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict); }
    let op = isyncyou_engine::dav_write::ApprovedDavWrite { operation_id, account_id:account.into(), origin, collection, service:"calendar".into(), href:target, expected_etag:Some(etag), expected_collection_token:Some(checkpoint), raw:Some(prepared.edit.raw) };
    match isyncyou_engine::dav_write::execute_task_parent(cfg,"/.well-known/caldav",&op,&uid,new_parent.as_deref(),None) {
        Ok(isyncyou_engine::dav_write::WriteOutcome::Confirmed(_)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new("Task parent updated; local synchronization pending")),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Conflict) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Rejected) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected),
        _ => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn execute_bound_task_attributes(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::TaskAttributes { target, origin, collection, uid, name, priority, due, due_date, start, old_start, new_start, old_priority, old_due, etag, raw_sha256 })) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let account = action.account(); let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    let prepared = match isyncyou_engine::dav_live::prepare_task_attributes_with_start_preview(cfg,account,&collection,&target,&uid,priority,due.as_deref().map(|d| (d,due_date)),start.as_deref()) { Ok(value) => value, Err(_) => return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
    if prepared.origin != origin || prepared.name != name || prepared.old_priority != old_priority || prepared.old_due != old_due || prepared.old_start != old_start || prepared.new_start != new_start || prepared.edit.href != target || prepared.edit.expected_etag != etag || prepared.edit.expected_collection_token.is_some() || sha256_hex(&prepared.edit.raw) != raw_sha256 { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict); }
    let op = isyncyou_engine::dav_write::ApprovedDavWrite { operation_id, account_id:account.into(), origin, collection, service:"calendar".into(), href:target, expected_etag:Some(etag), expected_collection_token:None, raw:Some(prepared.edit.raw) };
    match isyncyou_engine::dav_write::execute_task_attributes_with_start(cfg,"/.well-known/caldav",&op,&uid,priority,due.as_deref().map(|d| (d,due_date)),start.as_deref(),None) {
        Ok(isyncyou_engine::dav_write::WriteOutcome::Confirmed(_)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new("Task updated; local synchronization pending")),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Conflict) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Rejected) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected),
        _ => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn execute_bound_zoned_time(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::ZonedTime { target, origin, collection, uid, start, end, tzid, old_start, old_start_zone, old_end, old_end_zone, etag, raw_sha256 })) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let account = action.account(); let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    let prepared = match isyncyou_engine::dav_live::prepare_zoned_time_preview(cfg,account,&collection,&target,&uid,&start,&end,&tzid) { Ok(value) => value, Err(_) => return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
    if prepared.origin != origin || prepared.old_start != old_start || prepared.old_start_zone != old_start_zone || prepared.old_end != old_end || prepared.old_end_zone != old_end_zone || prepared.edit.href != target || prepared.edit.expected_etag != etag || prepared.edit.expected_collection_token.is_some() || sha256_hex(&prepared.edit.raw) != raw_sha256 { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict); }
    let op = isyncyou_engine::dav_write::ApprovedDavWrite { operation_id, account_id:account.into(), origin, collection, service:"calendar".into(), href:target, expected_etag:Some(etag), expected_collection_token:None, raw:Some(prepared.edit.raw) };
    match isyncyou_engine::dav_write::execute_zoned_time(cfg,"/.well-known/caldav",&op,&uid,&start,&end,&tzid,None) {
        Ok(isyncyou_engine::dav_write::WriteOutcome::Confirmed(_)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new("Event time updated; local synchronization pending")),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Conflict) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Rejected) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected),
        _ => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn execute_bound_cancellation(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Cancellation { target, collection, uid, recurrence_id, sequence, effect, etag, raw_sha256 })) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let account = action.account(); let entry = "/.well-known/caldav"; let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    let p = match isyncyou_engine::dav_live::prepare_cancellation_preview(cfg,account,entry,&collection,&target,&uid,recurrence_id.as_deref(),sequence,None) { Ok(value) => value, Err(_) => return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
    let current = serde_json::json!({"origin":p.origin,"summary":p.summary,"organizer":p.organizer,"attendees":p.attendees,"old_sequence":p.old_sequence,"new_sequence":p.new_sequence,"has_alarms":p.has_alarms});
    if current != effect || p.edit.href != target || p.edit.expected_etag != etag || p.edit.expected_collection_token.is_some() || sha256_hex(&p.edit.raw) != raw_sha256 { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict); }
    let op = isyncyou_engine::dav_write::ApprovedDavWrite { operation_id, account_id:account.into(), origin:p.origin, collection, service:"calendar".into(), href:target, expected_etag:Some(etag), expected_collection_token:None, raw:Some(p.edit.raw) };
    match isyncyou_engine::dav_write::execute_cancellation(cfg,entry,&op,&uid,recurrence_id.as_deref(),sequence,None) {
        Ok(isyncyou_engine::dav_write::WriteOutcome::Confirmed(_)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new("Cancellation saved; notice delivery is not confirmed")),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Conflict) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Rejected) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected),
        _ => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn valid_expansion_month(start: &str, end: &str) -> bool {
    if start.len() != 16 || end.len() != 16 || !start.ends_with("01T000000Z") || !end.ends_with("01T000000Z") || !start.as_bytes()[..6].iter().all(u8::is_ascii_digit) { return false; }
    let Ok(year) = start[..4].parse::<u32>() else { return false; }; let Ok(month) = start[4..6].parse::<u32>() else { return false; };
    if !(1900..=9998).contains(&year) || !(1..=12).contains(&month) { return false; }
    let (next_year,next_month) = if month == 12 { (year+1,1) } else { (year,month+1) };
    end == format!("{next_year:04}{next_month:02}01T000000Z")
}

fn execute_bound_exception(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Exception { target, origin, collection, uid, recurrence_id, summary, old_start, old_end, start, end, window_start, window_end, etag, raw_sha256 })) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    if !valid_expansion_month(&window_start,&window_end) { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); }
    let account = action.account(); let entry = "/.well-known/caldav"; let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    let p = match isyncyou_engine::dav_live::prepare_exception_preview(cfg,account,entry,&collection,&target,&uid,&recurrence_id,&start,&end,&window_start,&window_end,None) { Ok(value) => value, Err(_) => return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
    if p.origin != origin || p.summary != summary || p.old_start != old_start || p.old_end != old_end || p.edit.href != target || p.edit.expected_etag != etag || p.edit.expected_collection_token.is_some() || sha256_hex(&p.edit.raw) != raw_sha256 { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict); }
    let op = isyncyou_engine::dav_write::ApprovedDavWrite { operation_id, account_id:account.into(), origin, collection, service:"calendar".into(), href:target, expected_etag:Some(etag), expected_collection_token:None, raw:Some(p.edit.raw) };
    match isyncyou_engine::dav_write::execute_exception(cfg,entry,&op,&uid,&recurrence_id,&start,&end,&window_start,&window_end,None) {
        Ok(isyncyou_engine::dav_write::WriteOutcome::Confirmed(_)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new("Occurrence changed; local synchronization pending")),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Conflict) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Rejected) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected),
        _ => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn execute_bound_series_rule(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SeriesRule { target, collection, uid, rule, effect, etag, raw_sha256 })) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let account = action.account(); let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    let p = match isyncyou_engine::dav_live::prepare_series_rule_preview(cfg,account,&collection,&target,&uid,&rule) { Ok(value) => value, Err(_) => return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
    let current = serde_json::json!({"origin":p.origin,"summary":p.summary,"start":p.start,"end":p.end,"old_rule":p.old_rule,"new_rule":p.new_rule,"old_occurrences":p.old_occurrences,"new_occurrences":p.new_occurrences});
    if current != effect || p.edit.href != target || p.edit.expected_etag != etag || p.edit.expected_collection_token.is_some() || sha256_hex(&p.edit.raw) != raw_sha256 { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict); }
    let op = isyncyou_engine::dav_write::ApprovedDavWrite { operation_id, account_id:account.into(), origin:p.origin, collection, service:"calendar".into(), href:target, expected_etag:Some(etag), expected_collection_token:None, raw:Some(p.edit.raw) };
    match isyncyou_engine::dav_write::execute_series_rule(cfg,"/.well-known/caldav",&op,&uid,&rule,None) {
        Ok(isyncyou_engine::dav_write::WriteOutcome::Confirmed(_)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new("Series updated; server occurrence expansion not yet verified")),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Conflict) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Rejected) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected),
        _ => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn execute_bound_provider_contact_name(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::Contacts(ContactLiveWrite::SetName { target, origin, collection, old_name, new_name, etag, raw_sha256 })) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let account = action.account();
    if !matches!(cfg.provider_accounts.get(account), Some(isyncyou_core::config::ProviderConfig::SelfHosted { origin:configured, .. }) if configured == &origin) { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); }
    let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    let prepared = match isyncyou_engine::dav_live::prepare_contact_name(cfg, account, &collection, &target, &new_name) {
        Ok(value) => value,
        Err(_) => return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected),
    };
    if prepared.origin != origin || prepared.old_name != old_name || prepared.edit.href != target || prepared.edit.expected_etag != etag || prepared.edit.expected_collection_token.is_some() || sha256_hex(&prepared.edit.raw) != raw_sha256 {
        return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict);
    }
    let approved = isyncyou_engine::dav_write::ApprovedDavWrite { operation_id, account_id:account.into(), origin, collection, service:"contacts".into(), href:target, expected_etag:Some(etag), expected_collection_token:None, raw:Some(prepared.edit.raw) };
    match isyncyou_engine::dav_write::execute(cfg, "/.well-known/carddav", &approved) {
        Ok(isyncyou_engine::dav_write::WriteOutcome::Confirmed(_)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new("Contact name updated; local synchronization pending".to_string())),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Conflict) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Rejected) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected),
        _ => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn execute_bound_contact_field(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::Contacts(ContactLiveWrite::SetField { target, origin, collection, key, index, version, property_name, parameters, value_type, old_value, new_value, etag, raw_sha256 })) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let account = action.account(); let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    let prepared = match isyncyou_engine::dav_live::prepare_contact_text_preview(cfg,account,&collection,&target,&key,index,&new_value) { Ok(value) => value, Err(_) => return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
    if prepared.origin != origin || prepared.version != version || prepared.property_name != property_name || serde_json::json!(prepared.parameters) != parameters || prepared.value_type != value_type || prepared.old_value != old_value || prepared.new_value != new_value || prepared.edit.href != target || prepared.edit.expected_etag != etag || prepared.edit.expected_collection_token.is_some() || sha256_hex(&prepared.edit.raw) != raw_sha256 { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict); }
    let op = isyncyou_engine::dav_write::ApprovedDavWrite { operation_id, account_id:account.into(), origin, collection, service:"contacts".into(), href:target, expected_etag:Some(etag), expected_collection_token:None, raw:Some(prepared.edit.raw) };
    match isyncyou_engine::dav_write::execute_contact_field(cfg,"/.well-known/carddav",&op,&key,index,&new_value,None) {
        Ok(isyncyou_engine::dav_write::WriteOutcome::Confirmed(_)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new("Contact field updated; local synchronization pending")),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Conflict) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Rejected) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected),
        _ => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn execute_bound_contact_add(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::Contacts(ContactLiveWrite::AddField { target, origin, collection, key, version, value_type, new_value, etag, raw_sha256 })) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let account = action.account(); let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    let prepared = match isyncyou_engine::dav_live::prepare_contact_add_preview(cfg,account,&collection,&target,&key,&new_value,&value_type) { Ok(value) => value, Err(_) => return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
    if prepared.origin != origin || prepared.version != version || prepared.value_type != value_type || prepared.new_value != new_value || prepared.edit.href != target || prepared.edit.expected_etag != etag || prepared.edit.expected_collection_token.is_some() || sha256_hex(&prepared.edit.raw) != raw_sha256 { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict); }
    let op = isyncyou_engine::dav_write::ApprovedDavWrite { operation_id, account_id:account.into(), origin, collection, service:"contacts".into(), href:target, expected_etag:Some(etag), expected_collection_token:None, raw:Some(prepared.edit.raw) };
    match isyncyou_engine::dav_write::execute_contact_add(cfg,"/.well-known/carddav",&op,&key,&new_value,&value_type,None) {
        Ok(isyncyou_engine::dav_write::WriteOutcome::Confirmed(_)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new("Contact field added; local synchronization pending")),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Conflict) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Rejected) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected),
        _ => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn execute_bound_contact_remove(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::Contacts(ContactLiveWrite::RemoveField { target, origin, collection, key, index, version, property_name, parameters, value_type, old_value, etag, raw_sha256 })) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let account = action.account(); let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    let prepared = match isyncyou_engine::dav_live::prepare_contact_remove_preview(cfg,account,&collection,&target,&key,index) { Ok(value) => value, Err(_) => return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
    if prepared.origin != origin || prepared.version != version || prepared.property_name != property_name || serde_json::json!(prepared.parameters) != parameters || prepared.value_type != value_type || prepared.old_value != old_value || prepared.edit.href != target || prepared.edit.expected_etag != etag || prepared.edit.expected_collection_token.is_some() || sha256_hex(&prepared.edit.raw) != raw_sha256 { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict); }
    let op = isyncyou_engine::dav_write::ApprovedDavWrite { operation_id, account_id:account.into(), origin, collection, service:"contacts".into(), href:target, expected_etag:Some(etag), expected_collection_token:None, raw:Some(prepared.edit.raw) };
    match isyncyou_engine::dav_write::execute_contact_remove(cfg,"/.well-known/carddav",&op,&key,index,None) {
        Ok(isyncyou_engine::dav_write::WriteOutcome::Confirmed(_)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new("Contact field removed; local synchronization pending")),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Conflict) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Rejected) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected),
        _ => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn execute_bound_task_occurrence(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::TaskOccurrence { target, collection, uid, recurrence_id, completed_at, window_start, window_end, effect, etag, raw_sha256 })) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let account = action.account(); let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    let p = match isyncyou_engine::dav_live::prepare_task_occurrence_completion_preview(cfg,account,"/.well-known/caldav",&collection,&target,&uid,&recurrence_id,Some(&completed_at),&window_start,&window_end,None) { Ok(p) => p, Err(_) => return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
    let current = serde_json::json!({"origin":p.origin,"summary":p.summary,"start":p.start,"due":p.due,"completed_at":p.completed_at});
    if current != effect || p.edit.href != target || p.edit.expected_etag != etag || p.edit.expected_collection_token.is_some() || sha256_hex(&p.edit.raw) != raw_sha256 { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict); }
    let Some(origin) = current.get("origin").and_then(serde_json::Value::as_str) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    if !matches!(cfg.provider_accounts.get(account), Some(isyncyou_core::config::ProviderConfig::SelfHosted { origin: configured, .. }) if configured == origin) { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); }
    let op = isyncyou_engine::dav_write::ApprovedDavWrite { operation_id, account_id:account.into(), origin:origin.into(), collection, service:"calendar".into(), href:target, expected_etag:Some(etag), expected_collection_token:None, raw:Some(p.edit.raw) };
    match isyncyou_engine::dav_write::execute_task_occurrence_completion(cfg,"/.well-known/caldav",&op,&uid,&recurrence_id,Some(&completed_at),&window_start,&window_end,None) {
        Ok(isyncyou_engine::dav_write::WriteOutcome::Confirmed(_)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new("Task occurrence completed; local synchronization pending")),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Conflict) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Rejected) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected),
        _ => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn execute_bound_alarm(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Alarm { target, collection, uid, recurrence_id, index, trigger, description, effect, etag, raw_sha256 })) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let account = action.account();
    let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    let p = match isyncyou_engine::dav_live::prepare_alarm_preview(cfg,account,&collection,&target,&uid,recurrence_id.as_deref(),index,trigger.as_deref(),description.as_deref()) { Ok(p) => p, Err(_) => return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
    let current = serde_json::json!({"origin":p.origin,"summary":p.summary,"old_trigger":p.old_trigger,"old_description":p.old_description,"new_trigger":p.new_trigger,"new_description":p.new_description});
    if current != effect || p.edit.href != target || p.edit.expected_etag != etag || p.edit.expected_collection_token.is_some() || sha256_hex(&p.edit.raw) != raw_sha256 { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict); }
    let Some(origin) = current.get("origin").and_then(serde_json::Value::as_str) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    if !matches!(cfg.provider_accounts.get(account), Some(isyncyou_core::config::ProviderConfig::SelfHosted { origin: configured, .. }) if configured == origin) { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); }
    let approved = isyncyou_engine::dav_write::ApprovedDavWrite { operation_id, account_id:account.into(), origin:origin.into(), collection, service:"calendar".into(), href:target, expected_etag:Some(etag), expected_collection_token:None, raw:Some(p.edit.raw) };
    match isyncyou_engine::dav_write::execute_alarm(cfg,"/.well-known/caldav",&approved,&uid,recurrence_id.as_deref(),index,trigger.as_deref(),description.as_deref(),None) {
        Ok(isyncyou_engine::dav_write::WriteOutcome::Confirmed(_)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new("Reminder updated; local synchronization pending")),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Conflict) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Rejected) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected),
        _ => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn execute_bound_task_completion(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SetTaskCompletion { target, origin, collection, uid, recurrence_id, complete, completed_at, summary, old_status, old_completed, old_percent, etag, raw_sha256 })) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let account = action.account();
    if complete != completed_at.is_some() || !matches!(cfg.provider_accounts.get(account), Some(isyncyou_core::config::ProviderConfig::SelfHosted { origin: configured, .. }) if configured == &origin) { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); }
    let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    let prepared = match isyncyou_engine::dav_live::prepare_task_completion_preview(cfg, account, &collection, &target, &uid, recurrence_id.as_deref(), completed_at.as_deref()) {
        Ok(value) => value,
        Err(_) => return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected),
    };
    if prepared.origin != origin || prepared.summary != summary || prepared.old_status != old_status || prepared.old_completed != old_completed || prepared.old_percent != old_percent || prepared.edit.href != target || prepared.edit.expected_etag != etag || prepared.edit.expected_collection_token.is_some() || sha256_hex(&prepared.edit.raw) != raw_sha256 {
        return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict);
    }
    let approved = isyncyou_engine::dav_write::ApprovedDavWrite { operation_id, account_id: account.into(), origin, collection, service: "calendar".into(), href: target, expected_etag: Some(etag), expected_collection_token: None, raw: Some(prepared.edit.raw) };
    match isyncyou_engine::dav_write::execute(cfg, "/.well-known/caldav", &approved) {
        Ok(isyncyou_engine::dav_write::WriteOutcome::Confirmed(_)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new("Task completion updated; local synchronization pending".to_string())),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Conflict) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Rejected) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected),
        _ => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn provider_drafts_mailbox(cfg: &Config, account: &str) -> Result<String, String> {
    let store = isyncyou_engine::provider_store::open_readonly(cfg, account)?;
    let client = isyncyou_engine::self_hosted_mail::client(cfg, account)?;
    let scope = isyncyou_engine::self_hosted_mail::mailbox_scope(cfg, account, &client)?;
    let mut after = String::new();
    let mut found = None;
    let mut count = 0usize;
    loop {
        let page = store.provider_objects_page(&scope, &after, 200).map_err(|_| "provider_mailboxes_unavailable")?;
        if page.is_empty() { break; }
        for object in page {
            after = object.remote_id.clone();
            count += 1;
            if count > 1000 { return Err("provider_mailboxes_limit".into()); }
            let value: serde_json::Value = serde_json::from_slice(&object.raw).map_err(|_| "provider_mailbox_invalid")?;
            if value["role"].as_str() == Some("drafts") && found.replace(object.remote_id).is_some() { return Err("provider_drafts_ambiguous".into()); }
        }
    }
    found.ok_or_else(|| "provider_drafts_unavailable".into())
}

fn execute_bound_provider_draft(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::ProviderCreateDraft { origin, draft_mailbox_id, subject, body, to, upload_ids, uploads })) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let account = action.account();
    if !matches!(cfg.provider_accounts.get(account), Some(isyncyou_core::config::ProviderConfig::SelfHosted { origin: configured, .. }) if configured == &origin) { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); }
    // Plain text is escaped before it becomes HTML; the approved text is the entire effect.
    let html = body.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('\n', "<br>");
    let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    if provider_drafts_mailbox(cfg, account).ok().as_deref() != Some(draft_mailbox_id.as_str()) { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict); }
    if !upload_ids.is_empty() {
        let result = (|| {
            let store = isyncyou_engine::provider_store::open(cfg,account)?; let client = isyncyou_engine::self_hosted_mail::client(cfg,account)?;
            let current = isyncyou_engine::self_hosted_mail::resolve_uploaded_attachments(cfg,account,&store,&upload_ids)?;
            let native_account = isyncyou_engine::self_hosted_mail::scope(cfg,account,&client)?.collection;
            if current.iter().any(|part| part.remote_account != native_account) { return Err("attachment_account_changed".into()); }
            if serde_json::json!(current) != uploads { return Err("attachment_changed".into()); }
            let parts:Vec<_> = current.iter().map(|part| serde_json::json!({"blobId":part.blob_id,"type":part.content_type,"name":part.filename,"disposition":"attachment"})).collect();
            let email = serde_json::json!({"mailboxIds":{draft_mailbox_id:true},"keywords":{"$draft":true},"subject":subject,"to":to.iter().map(|email| serde_json::json!({"email":email})).collect::<Vec<_>>(),"cc":[],"bcc":[],"bodyValues":{"body":{"value":html}},"htmlBody":[{"partId":"body","type":"text/html"}],"attachments":parts});
            isyncyou_engine::self_hosted_mail::create_draft(cfg,account,&store,&client,&operation_id,&email)
        })();
        return match result { Ok(isyncyou_engine::self_hosted_mail::SendOutcome::Confirmed(_)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new("Draft created with attachments; message not sent; local synchronization pending")), Ok(isyncyou_engine::self_hosted_mail::SendOutcome::Rejected) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected), _ => ConfirmedExecutionOutcome::OutcomeUnknown };
    }
    let writer = crate::DaemonMailWrite { cfg: cfg.clone(), config_path: None };
    match isyncyou_webui::MailWriteHandler::provider_create_draft(&writer, account, &operation_id, &subject, &html, &to, &[], &[], None, false, None, Some(&draft_mailbox_id)) {
        Ok(result) => match result["draft_id"].as_str() {
            Some(id) if !id.is_empty() => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new(format!("Draft created ({}); local synchronization {}. Message not sent.", redact_agent_operation_text(id), result["sync_state"].as_str().unwrap_or("pending")))),
            _ => ConfirmedExecutionOutcome::OutcomeUnknown,
        },
        // The provider may have created the draft before a response was lost.
        Err(_) => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn execute_bound_calendar_text(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SetText { target, origin, collection, uid, recurrence_id, field, new_value, old_value, etag, raw_sha256 })) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let account = action.account();
    if !matches!(cfg.provider_accounts.get(account), Some(isyncyou_core::config::ProviderConfig::SelfHosted { origin: configured, .. }) if configured == &origin) { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); }
    let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    let prepared = match isyncyou_engine::dav_live::prepare_text_preview(cfg, account, &collection, &target, &uid, recurrence_id.as_deref(), &field, &new_value) {
        Ok(value) => value,
        Err(_) => return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected),
    };
    if prepared.origin != origin || prepared.old_value != old_value || prepared.new_value != new_value || prepared.edit.href != target || prepared.edit.expected_etag != etag || prepared.edit.expected_collection_token.is_some() || sha256_hex(&prepared.edit.raw) != raw_sha256 { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict); }
    let approved = isyncyou_engine::dav_write::ApprovedDavWrite { operation_id, account_id:account.into(), origin, collection, service:"calendar".into(), href:target, expected_etag:Some(etag), expected_collection_token:None, raw:Some(prepared.edit.raw) };
    match isyncyou_engine::dav_write::execute(cfg, "/.well-known/caldav", &approved) {
        Ok(isyncyou_engine::dav_write::WriteOutcome::Confirmed(_)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new("Calendar text updated; local synchronization pending".to_string())),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Conflict) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Rejected) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected),
        _ => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn execute_bound_calendar_time(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SetTime { target, origin, collection, uid, start, end, date_only, old_start, old_end, etag, raw_sha256 })) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let account = action.account();
    if !matches!(cfg.provider_accounts.get(account), Some(isyncyou_core::config::ProviderConfig::SelfHosted { origin: configured, .. }) if configured == &origin) { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); }
    let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    let prepared = match isyncyou_engine::dav_live::prepare_event_time_preview(cfg, account, &collection, &target, &uid, &start, &end, date_only) { Ok(value) => value, Err(_) => return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
    if prepared.origin != origin || prepared.old_start != old_start || prepared.old_end != old_end || prepared.edit.href != target || prepared.edit.expected_etag != etag || prepared.edit.expected_collection_token.is_some() || sha256_hex(&prepared.edit.raw) != raw_sha256 { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict); }
    let approved = isyncyou_engine::dav_write::ApprovedDavWrite { operation_id, account_id:account.into(), origin, collection, service:"calendar".into(), href:target, expected_etag:Some(etag), expected_collection_token:None, raw:Some(prepared.edit.raw) };
    match isyncyou_engine::dav_write::execute(cfg, "/.well-known/caldav", &approved) {
        Ok(isyncyou_engine::dav_write::WriteOutcome::Confirmed(_)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new("Event time updated; local synchronization pending")),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Conflict) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Rejected) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected),
        _ => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn execute_bound_dav_acl(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::DavAcl { service, origin, collection, principal, privilege, expected }) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Some(entries) = expected.as_array() else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let expected = entries.iter().map(|v| Some(isyncyou_connectors::dav::AclEntry {
        principal:v.get("principal")?.as_str()?.to_owned(), granted:v.get("granted")?.as_bool()?,
        privileges:v.get("privileges")?.as_array()?.iter().map(|x| x.as_str().map(str::to_owned)).collect::<Option<Vec<_>>>()?,
        inherited:v.get("inherited")?.as_bool()?, protected:v.get("protected")?.as_bool()?,
    })).collect::<Option<Vec<_>>>();
    let Some(expected) = expected else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let op = isyncyou_engine::dav_write::ApprovedDavAcl { operation_id, account_id:action.account().into(), origin, collection, service:service.clone(), expected, principal, privilege };
    let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    match isyncyou_engine::dav_write::execute_acl_with_store(cfg, if service == "calendar" { "/.well-known/caldav" } else { "/.well-known/carddav" }, &op, None) {
        Ok(isyncyou_engine::dav_write::WriteOutcome::Confirmed(_)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new("Collection access updated")),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Conflict) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Rejected) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected),
        _ => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn execute_bound_invitation_response(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::InvitationResponse { target, origin, collection, uid, recurrence_id, attendee, organizer, old_response, response, etag, raw_sha256 })) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let account = action.account(); let entry = "/.well-known/caldav";
    let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    let prepared = match isyncyou_engine::dav_live::prepare_invitation_response(cfg,account,entry,&collection,&target,&uid,recurrence_id.as_deref(),&attendee,&response,None) { Ok(value) => value, Err(_) => return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
    if prepared.origin != origin || prepared.organizer != organizer || prepared.attendee != attendee || prepared.old_response != old_response || prepared.new_response != response || prepared.edit.href != target || prepared.edit.expected_etag != etag || prepared.edit.expected_collection_token.is_some() || sha256_hex(&prepared.edit.raw) != raw_sha256 { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict); }
    let op = isyncyou_engine::dav_write::ApprovedDavWrite { operation_id, account_id:account.into(), origin, collection, service:"calendar".into(), href:target, expected_etag:Some(etag), expected_collection_token:None, raw:Some(prepared.edit.raw) };
    match isyncyou_engine::dav_write::execute_invitation_response(cfg,entry,&op,&uid,recurrence_id.as_deref(),&attendee,&response,None) {
        Ok(isyncyou_engine::dav_write::WriteOutcome::Confirmed(_)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new("Invitation response updated; local synchronization pending")),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Conflict) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Rejected) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected),
        _ => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn execute_bound_dav_delete(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::DavDelete { service, origin, collection, target, kind, uid, name, etag, source_sha256, projection }) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let account = action.account(); let entry = if service == "calendar" { "/.well-known/caldav" } else { "/.well-known/carddav" };
    let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    let prepared = match isyncyou_engine::dav_live::prepare_delete_preview(cfg,account,&collection,&service,&target) { Ok(value) => value, Err(_) => return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
    let displayed = serde_json::json!({"object":prepared.projection,"photos":prepared.photos.iter().map(|p| serde_json::json!({"value_type":p.value_type,"encoded_size":p.encoded_size,"image_size":p.image_size,"value_sha256":p.value_sha256})).collect::<Vec<_>>()});
    if prepared.origin != origin || prepared.href != target || prepared.kind != kind || prepared.uid != uid || prepared.name != name || prepared.expected_etag != etag || prepared.source_sha256 != source_sha256 || displayed != projection { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict); }
    let op = isyncyou_engine::dav_write::ApprovedDavWrite { operation_id, account_id:account.into(), origin, collection, service, href:target, expected_etag:Some(etag), expected_collection_token:None, raw:None };
    match isyncyou_engine::dav_write::execute_delete_preview(cfg,entry,&op,&uid,&source_sha256,None) {
        Ok(isyncyou_engine::dav_write::WriteOutcome::Confirmed(_)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new("Resource deleted; local synchronization pending")),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Conflict) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Rejected) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected),
        _ => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn execute_bound_dav_create(cfg: &Config, action: &isyncyou_agent::ToolAction, pending_id: &str, gate: &Arc<Mutex<()>>) -> ConfirmedExecutionOutcome {
    let Some(operation_id) = bound_submission_operation_id(pending_id) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let Ok(AgentLiveWriteIntent::DavCreate { service, origin, collection, target, kind, uid, timestamp, name, start, end, organizer, attendees, raw_sha256 }) = validate_live_write_action(action) else { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected); };
    let raw = match organizer.as_deref() {
        Some(organizer) => match (start.as_deref(),end.as_deref()) { (Some(start),Some(end)) => isyncyou_connectors::dav::edit::create_invitation(&uid,&timestamp,&name,start,end,organizer,&attendees), _ => return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) },
        None => isyncyou_connectors::dav::edit::create_simple(&kind,&uid,&timestamp,&name,start.as_deref(),end.as_deref()),
    };
    let raw = match raw { Ok(value) => value, Err(_) => return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
    if sha256_hex(&raw) != raw_sha256 { return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict); }
    let preview = isyncyou_engine::dav_live::PreparedCreatePreview { origin:origin.clone(), collection:collection.clone(), service:service.clone(), href:target.clone(), kind, uid, timestamp, name, start, end, raw:raw.clone() };
    let op = isyncyou_engine::dav_write::ApprovedDavWrite { operation_id, account_id:action.account().into(), origin, collection, service:service.clone(), href:target, expected_etag:None, expected_collection_token:None, raw:Some(raw) };
    let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    let scheduled = organizer.is_some();
    let result = match organizer { Some(organizer) => isyncyou_engine::dav_write::execute_invitation_create(cfg,"/.well-known/caldav",&op,&isyncyou_engine::dav_live::PreparedInvitationCreate { create:preview, organizer, attendees },None), None => isyncyou_engine::dav_write::execute_create_simple(cfg,if service == "calendar" { "/.well-known/caldav" } else { "/.well-known/carddav" },&op,&preview,None) };
    match result {
        Ok(isyncyou_engine::dav_write::WriteOutcome::Confirmed(_)) => ConfirmedExecutionOutcome::Completed(ConfirmedActionResult::new(if scheduled { "Event created; invitation delivery is not confirmed" } else { "Resource created; local synchronization pending" })),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Conflict) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderConflict),
        Ok(isyncyou_engine::dav_write::WriteOutcome::Rejected) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderActionRejected),
        _ => ConfirmedExecutionOutcome::OutcomeUnknown,
    }
}

fn is_provider_submission(action: &isyncyou_agent::ToolAction) -> bool {
    matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::SendApproved { .. })))
}

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
        AgentOperationPolicy::MobileDisabled => Arc::new(MobileDisabledAgentOperations { cfg }),
        AgentOperationPolicy::MobileFullNode { mobile_jobs } => {
            Arc::new(MobileFullNodeAgentOperations::new(cfg, gate, mobile_jobs))
        }
    }
}

fn exact_account_key(
    cfg: &Config,
    action: &isyncyou_agent::ToolAction,
) -> Result<String, ClosedExecutionCode> {
    let requested = action.account();
    let mut matching = cfg
        .accounts
        .iter()
        .filter(|account| account.id == requested);
    let Some(account) = matching.next() else {
        return Err(ClosedExecutionCode::ExecutorRejected);
    };
    if matching.next().is_some() {
        return Err(ClosedExecutionCode::ExecutorRejected);
    }
    Ok(account.id.clone())
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
    DavCreate { service:String, origin:String, collection:String, target:String, kind:String, uid:String, timestamp:String, name:String, start:Option<String>, end:Option<String>, organizer:Option<String>, attendees:Vec<String>, raw_sha256:String },
    DavDelete { service:String, origin:String, collection:String, target:String, kind:String, uid:String, name:String, etag:String, source_sha256:String, projection:serde_json::Value },
    DavAcl { service:String, origin:String, collection:String, principal:String, privilege:Option<String>, expected:serde_json::Value },
    Mail(MailLiveWrite),
    Calendar(CalendarLiveWrite),
    Contacts(ContactLiveWrite),
    Todo(TodoLiveWrite),
    OneNote(OneNoteLiveWrite),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MailLiveWrite {
    RelatedDraft { preview:serde_json::Value, email:serde_json::Value },
    ProviderAction { preview:serde_json::Value },
    ProviderCreateDraft { origin: String, draft_mailbox_id: String, subject: String, body: String, to: Vec<String>, upload_ids:Vec<String>, uploads:serde_json::Value },
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
    SendApproved {
        origin: String, email_id: String, blob_id: String,
        identity_id: String, mail_from: String, recipients: Vec<String>,
        draft_mailbox_id: String, sent_mailbox_id: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum CalendarLiveWrite {
    TaskOccurrence { target:String, collection:String, uid:String, recurrence_id:String, completed_at:String, window_start:String, window_end:String, effect:serde_json::Value, etag:String, raw_sha256:String },
    Alarm { target:String, collection:String, uid:String, recurrence_id:Option<String>, index:Option<usize>, trigger:Option<String>, description:Option<String>, effect:serde_json::Value, etag:String, raw_sha256:String },
    SeriesRule { target:String, collection:String, uid:String, rule:String, effect:serde_json::Value, etag:String, raw_sha256:String },
    Exception { target:String, origin:String, collection:String, uid:String, recurrence_id:String, summary:String, old_start:String, old_end:String, start:String, end:String, window_start:String, window_end:String, etag:String, raw_sha256:String },
    Cancellation { target:String, collection:String, uid:String, recurrence_id:Option<String>, sequence:u32, effect:serde_json::Value, etag:String, raw_sha256:String },
    ZonedTime { target:String, origin:String, collection:String, uid:String, start:String, end:String, tzid:String, old_start:String, old_start_zone:Option<String>, old_end:String, old_end_zone:Option<String>, etag:String, raw_sha256:String },
    TaskParent { target:String, origin:String, collection:String, uid:String, summary:String, old_parent:Option<String>, new_parent:Option<String>, etag:String, checkpoint:String, raw_sha256:String },
    TaskAttributes { target:String, origin:String, collection:String, uid:String, name:String, priority:Option<u8>, due:Option<String>, due_date:bool, start:Option<String>, old_start:Option<String>, new_start:Option<String>, old_priority:Option<String>, old_due:Option<String>, etag:String, raw_sha256:String },
    InvitationResponse { target:String, origin:String, collection:String, uid:String, recurrence_id:Option<String>, attendee:String, organizer:String, old_response:String, response:String, etag:String, raw_sha256:String },
    SetTime { target:String, origin:String, collection:String, uid:String, start:String, end:String, date_only:bool, old_start:String, old_end:String, etag:String, raw_sha256:String },
    SetText { target:String, origin:String, collection:String, uid:String, recurrence_id:Option<String>, field:String, new_value:String, old_value:Option<String>, etag:String, raw_sha256:String },
    SetTaskCompletion { target:String, origin:String, collection:String, uid:String, recurrence_id:Option<String>, complete:bool, completed_at:Option<String>, summary:String, old_status:Option<String>, old_completed:Option<String>, old_percent:Option<String>, etag:String, raw_sha256:String },
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
    RemoveField { target:String, origin:String, collection:String, key:String, index:usize, version:String, property_name:String, parameters:serde_json::Value, value_type:String, old_value:String, etag:String, raw_sha256:String },
    AddField { target:String, origin:String, collection:String, key:String, version:String, value_type:String, new_value:String, etag:String, raw_sha256:String },
    SetField { target:String, origin:String, collection:String, key:String, index:usize, version:String, property_name:String, parameters:serde_json::Value, value_type:String, old_value:String, new_value:String, etag:String, raw_sha256:String },
    SetName { target:String, origin:String, collection:String, old_name:String, new_name:String, etag:String, raw_sha256:String },
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
            Self::DavCreate { service, .. } => if service == "calendar" { "calendar" } else { "contacts" },
            Self::DavDelete { service, .. } => if service == "calendar" { "calendar" } else { "contacts" },
            Self::DavAcl { service, .. } => if service == "calendar" { "calendar" } else { "contacts" },
            Self::Mail(_) => "mail",
            Self::Calendar(_) => "calendar",
            Self::Contacts(_) => "contacts",
            Self::Todo(_) => "todo",
            Self::OneNote(_) => "onenote",
        }
    }

    fn verb(&self) -> &'static str {
        match self {
            Self::DavCreate { .. } => "dav_create",
            Self::DavDelete { .. } => "dav_delete",
            Self::DavAcl { .. } => "dav_acl",
            Self::Mail(MailLiveWrite::SetRead { .. }) => "set_read",
            Self::Mail(MailLiveWrite::ProviderAction { .. }) => "provider_mail_action",
            Self::Mail(MailLiveWrite::RelatedDraft { .. }) => "provider_related_draft",
            Self::Mail(MailLiveWrite::SetFlag { .. }) => "set_flag",
            Self::Mail(MailLiveWrite::SetCategories { .. }) => "set_categories",
            Self::Mail(MailLiveWrite::Move { .. }) => "move",
            Self::Mail(MailLiveWrite::CreateDraft { .. }) => "create_draft",
            Self::Mail(MailLiveWrite::ProviderCreateDraft { .. }) => "provider_create_draft",
            Self::Mail(MailLiveWrite::SendDraft { .. }) => "send_draft",
            Self::Mail(MailLiveWrite::SendApproved { .. }) => "send_approved",
            Self::Calendar(CalendarLiveWrite::SetTaskCompletion { .. }) => "task_set_completion",
            Self::Calendar(CalendarLiveWrite::SetText { .. }) => "calendar_set_text",
            Self::Calendar(CalendarLiveWrite::SetTime { .. }) => "calendar_set_time",
            Self::Calendar(CalendarLiveWrite::InvitationResponse { .. }) => "invitation_response",
            Self::Calendar(CalendarLiveWrite::TaskParent { .. }) => "task_parent",
            Self::Calendar(CalendarLiveWrite::TaskAttributes { .. }) => "task_attributes",
            Self::Calendar(CalendarLiveWrite::ZonedTime { .. }) => "calendar_set_zoned_time",
            Self::Calendar(CalendarLiveWrite::Cancellation { .. }) => "calendar_cancel",
            Self::Calendar(CalendarLiveWrite::Exception { .. }) => "calendar_exception",
            Self::Calendar(CalendarLiveWrite::SeriesRule { .. }) => "calendar_series_rule",
            Self::Calendar(CalendarLiveWrite::Alarm { .. }) => "calendar_alarm",
            Self::Calendar(CalendarLiveWrite::TaskOccurrence { .. }) => "task_occurrence_completion",
            Self::Calendar(CalendarLiveWrite::Create { .. }) => "create",
            Self::Calendar(CalendarLiveWrite::Update { .. }) => "update",
            Self::Calendar(CalendarLiveWrite::Delete { .. }) => "delete",
            Self::Calendar(CalendarLiveWrite::Respond { .. }) => "respond",
            Self::Contacts(ContactLiveWrite::SetName { .. }) => "contact_set_name",
            Self::Contacts(ContactLiveWrite::SetField { .. }) => "contact_set_field",
            Self::Contacts(ContactLiveWrite::AddField { .. }) => "contact_add_field",
            Self::Contacts(ContactLiveWrite::RemoveField { .. }) => "contact_remove_field",
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
            Self::DavCreate { .. } => "new item",
            Self::Mail(MailLiveWrite::CreateDraft { .. })
            | Self::Mail(MailLiveWrite::ProviderCreateDraft { .. })
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
            Self::DavCreate { .. } => "Create DAV resource",
            Self::DavDelete { .. } => "Delete DAV resource",
            Self::DavAcl { .. } => "Change collection access",
            Self::Mail(MailLiveWrite::SetRead { is_read: true, .. }) => "Mark this email as read",
            Self::Mail(MailLiveWrite::ProviderAction { .. }) => "Change email state",
            Self::Mail(MailLiveWrite::RelatedDraft { .. }) => "Create reply or forward draft",
            Self::Mail(MailLiveWrite::SetRead { is_read: false, .. }) => {
                "Mark this email as unread"
            }
            Self::Mail(MailLiveWrite::SetFlag { .. }) => "Update the follow-up flag on this email",
            Self::Mail(MailLiveWrite::SetCategories { .. }) => {
                "Update the categories on this email"
            }
            Self::Mail(MailLiveWrite::Move { .. }) => "Move this email",
            Self::Mail(MailLiveWrite::CreateDraft { .. }) => "Create an email draft",
            Self::Mail(MailLiveWrite::ProviderCreateDraft { .. }) => "Create an email draft",
            Self::Mail(MailLiveWrite::SendDraft { .. }) => "Send this email draft",
            Self::Mail(MailLiveWrite::SendApproved { .. }) => "Submit this approved email",
            Self::Calendar(CalendarLiveWrite::SetTaskCompletion { .. }) => "Change task completion",
            Self::Calendar(CalendarLiveWrite::SetText { .. }) => "Change calendar text",
            Self::Calendar(CalendarLiveWrite::SetTime { .. }) => "Change event time",
            Self::Calendar(CalendarLiveWrite::InvitationResponse { .. }) => "Respond to invitation",
            Self::Calendar(CalendarLiveWrite::TaskParent { .. }) => "Change task parent",
            Self::Calendar(CalendarLiveWrite::TaskAttributes { .. }) => "Change task attributes",
            Self::Calendar(CalendarLiveWrite::ZonedTime { .. }) => "Change event time and zone",
            Self::Calendar(CalendarLiveWrite::Cancellation { .. }) => "Cancel event",
            Self::Calendar(CalendarLiveWrite::Exception { .. }) => "Change one occurrence",
            Self::Calendar(CalendarLiveWrite::SeriesRule { .. }) => "Change event series",
            Self::Calendar(CalendarLiveWrite::Alarm { .. }) => "Change reminder",
            Self::Calendar(CalendarLiveWrite::TaskOccurrence { .. }) => "Complete one task occurrence",
            Self::Calendar(CalendarLiveWrite::Create { .. }) => "Create a calendar event",
            Self::Calendar(CalendarLiveWrite::Update { .. }) => "Update this calendar event",
            Self::Calendar(CalendarLiveWrite::Delete { .. }) => "Delete this calendar event",
            Self::Calendar(CalendarLiveWrite::Respond { .. }) => {
                "Respond to this calendar invitation"
            }
            Self::Contacts(ContactLiveWrite::SetName { .. }) => "Change the contact name",
            Self::Contacts(ContactLiveWrite::SetField { .. }) => "Change a contact field",
            Self::Contacts(ContactLiveWrite::AddField { .. }) => "Add a contact field",
            Self::Contacts(ContactLiveWrite::RemoveField { .. }) => "Remove a contact field",
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
                let writer = crate::DaemonMailWrite { cfg: cfg.clone(), config_path: None };
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
                    MailLiveWrite::SendApproved { .. } => Err("bound_confirmation_required".into()),
                    MailLiveWrite::ProviderCreateDraft { .. } => Err("bound_confirmation_required".into()),
                    MailLiveWrite::ProviderAction { .. } => Err("bound_confirmation_required".into()),
                    MailLiveWrite::RelatedDraft { .. } => Err("bound_confirmation_required".into()),
                }
            }
            AgentLiveWriteIntent::Calendar(op) => {
                let writer = crate::DaemonCalendarWrite { cfg: cfg.clone(), config_path:None };
                match op {
                    CalendarLiveWrite::SetTaskCompletion { .. } => Err("bound_confirmation_required".into()),
                    CalendarLiveWrite::SetText { .. } => Err("bound_confirmation_required".into()),
                    CalendarLiveWrite::SetTime { .. } => Err("bound_confirmation_required".into()),
                    CalendarLiveWrite::InvitationResponse { .. } => Err("bound_confirmation_required".into()),
                    CalendarLiveWrite::TaskParent { .. } => Err("bound_confirmation_required".into()),
                    CalendarLiveWrite::TaskAttributes { .. } => Err("bound_confirmation_required".into()),
                    CalendarLiveWrite::ZonedTime { .. } => Err("bound_confirmation_required".into()),
                    CalendarLiveWrite::Cancellation { .. } => Err("bound_confirmation_required".into()),
                    CalendarLiveWrite::Exception { .. } => Err("bound_confirmation_required".into()),
                    CalendarLiveWrite::SeriesRule { .. } => Err("bound_confirmation_required".into()),
                    CalendarLiveWrite::Alarm { .. } => Err("bound_confirmation_required".into()),
                    CalendarLiveWrite::TaskOccurrence { .. } => Err("bound_confirmation_required".into()),
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
            AgentLiveWriteIntent::DavAcl { .. } | AgentLiveWriteIntent::DavDelete { .. } | AgentLiveWriteIntent::DavCreate { .. } => Err("bound_confirmation_required".into()),
            AgentLiveWriteIntent::Contacts(op) => {
                let writer = crate::DaemonContactWrite { cfg: cfg.clone(), config_path:None };
                match op {
                    ContactLiveWrite::SetName { .. } => Err("bound_confirmation_required".into()),
                    ContactLiveWrite::SetField { .. } => Err("bound_confirmation_required".into()),
                    ContactLiveWrite::AddField { .. } => Err("bound_confirmation_required".into()),
                    ContactLiveWrite::RemoveField { .. } => Err("bound_confirmation_required".into()),
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
    let summary = if verb == "send_approved" {
        "mail submitted; delivery is not yet confirmed".to_string()
    } else { match &result_id {
        Some(id) => format!("{service} {verb} ok id={}", redact_agent_operation_text(id)),
        None => format!("{service} {verb} ok"),
    }};
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
        ("mail", "provider_mail_action") => Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::ProviderAction { preview:change.get("preview").filter(|v| v.is_object()).cloned().ok_or("invalid_mail_preview")? })),
        ("mail", "provider_related_draft") => Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::RelatedDraft { preview:change.get("preview").filter(|v| v.is_object()).cloned().ok_or("invalid_mail_preview")?, email:change.get("email").filter(|v| v.is_object()).cloned().ok_or("invalid_mail_draft")? })),
        ("calendar" | "contacts", "dav_create") => Ok(AgentLiveWriteIntent::DavCreate { service:service.clone(), origin:required_change_str(change,"origin")?, collection:required_change_str(change,"collection")?, target:required_target(target,change)?, kind:required_change_str(change,"kind")?, uid:required_change_str(change,"uid")?, timestamp:required_change_str(change,"timestamp")?, name:required_change_str(change,"name")?, start:optional_change_str(change,"start")?, end:optional_change_str(change,"end")?, organizer:optional_change_str(change,"organizer")?, attendees:optional_string_array(change,"attendees")?.unwrap_or_default(), raw_sha256:required_change_str(change,"raw_sha256")? }),
        ("calendar" | "contacts", "dav_delete") => Ok(AgentLiveWriteIntent::DavDelete { service:service.clone(), origin:required_change_str(change,"origin")?, collection:required_change_str(change,"collection")?, target:required_target(target,change)?, kind:required_change_str(change,"kind")?, uid:required_change_str(change,"uid")?, name:required_change_str(change,"name")?, etag:required_change_str(change,"etag")?, source_sha256:required_change_str(change,"source_sha256")?, projection:change.get("projection").cloned().ok_or("invalid_delete_projection")? }),
        ("calendar" | "contacts", "dav_acl") => {
            let privilege = optional_change_str(change,"privilege")?;
            if !matches!(privilege.as_deref(), None | Some("read") | Some("write") | Some("read-write")) { return Err("invalid_acl_privilege".into()); }
            let expected = change.get("expected").filter(|v| v.is_array()).cloned().ok_or("invalid_acl_expected")?;
            Ok(AgentLiveWriteIntent::DavAcl { service:service.clone(), origin:required_change_str(change,"origin")?, collection:required_change_str(change,"collection")?, principal:required_change_str(change,"principal")?, privilege, expected })
        },
        ("mail", "set_read") => Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::SetRead {
            target: required_target(target, change)?,
            is_read: required_change_bool_with_legacy_alias(change, "is_read", "value")?,
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
        ("mail", "provider_create_draft") => {
            let to = required_string_array(change, "to")?;
            if to.len() > 100 || to.iter().any(|s| s.len() > 320 || s.chars().any(char::is_control) || !s.contains('@')) { return Err("invalid_live_write: invalid recipients".into()); }
            let body = required_body_str(change, &["body"])?;
            if body.chars().any(|ch| ch.is_control() && ch != '\n' && ch != '\t') { return Err("invalid_live_write: invalid draft text".into()); }
            Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::ProviderCreateDraft {
                origin:required_change_str(change, "origin")?, draft_mailbox_id:required_change_str(change, "draft_mailbox_id")?, subject:optional_change_str(change, "subject")?.unwrap_or_default(),
                body, to, upload_ids:optional_string_array(change,"upload_ids")?.unwrap_or_default(), uploads:change.get("uploads").cloned().unwrap_or_else(|| serde_json::json!([])),
            }))
        },
        ("mail", "send_draft") => Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::SendDraft {
            target: required_target(target, change)?,
        })),
        ("mail", "send_approved") => {
            let recipients = change_object(change)?.get("recipients").and_then(serde_json::Value::as_array).ok_or("invalid_live_write: missing recipients")?.iter().map(|value| value.as_str().ok_or("invalid_live_write: invalid recipient").map(str::to_string)).collect::<Result<Vec<_>, _>>()?;
            let mail_from = required_change_str(change, "mail_from")?;
            if recipients.is_empty() || recipients.len() > 1000 || recipients.iter().map(String::len).sum::<usize>() > 64 * 1024 || recipients.iter().chain(std::iter::once(&mail_from)).any(|s| s.is_empty() || s.len() > 320 || s.chars().any(char::is_control)) {
                return Err("invalid_live_write: invalid envelope".into());
            }
            let email_id = required_change_str(change, "email_id")?;
            let blob_id = required_change_str(change, "blob_id")?;
            if email_id.len() > 256 || blob_id.len() > 256 { return Err("invalid_live_write: native identity too long".into()); }
            Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::SendApproved {
                origin: required_change_str(change, "origin")?, email_id,
                blob_id, identity_id: required_change_str(change, "identity_id")?, mail_from, recipients,
                draft_mailbox_id: required_change_str(change, "draft_mailbox_id")?, sent_mailbox_id: required_change_str(change, "sent_mailbox_id")?,
            }))
        },
        ("calendar", "task_set_completion") => Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SetTaskCompletion {
            target:required_target(target, change)?, origin:required_change_str(change,"origin")?, collection:required_change_str(change,"collection")?, uid:required_change_str(change,"uid")?,
            recurrence_id:optional_change_str(change,"recurrence_id")?, complete:required_change_bool(change,"complete")?, completed_at:optional_change_str(change,"completed_at")?,
            summary:required_change_str(change,"summary")?, old_status:optional_change_str(change,"old_status")?, old_completed:optional_change_str(change,"old_completed")?, old_percent:optional_change_str(change,"old_percent")?,
            etag:required_change_str(change,"etag")?, raw_sha256:required_change_str(change,"raw_sha256")?,
        })),
        ("calendar", "calendar_set_text") => Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SetText {
            target:required_target(target, change)?, origin:required_change_str(change,"origin")?, collection:required_change_str(change,"collection")?, uid:required_change_str(change,"uid")?,
            recurrence_id:optional_change_str(change,"recurrence_id")?, field:required_change_str(change,"field")?, new_value:required_change_str(change,"new_value")?,
            old_value:optional_change_str(change,"old_value")?, etag:required_change_str(change,"etag")?, raw_sha256:required_change_str(change,"raw_sha256")?,
        })),
        ("calendar", "calendar_set_time") => Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SetTime {
            target:required_target(target,change)?, origin:required_change_str(change,"origin")?, collection:required_change_str(change,"collection")?, uid:required_change_str(change,"uid")?,
            start:required_change_str(change,"start")?, end:required_change_str(change,"end")?, date_only:required_change_bool(change,"date_only")?, old_start:required_change_str(change,"old_start")?, old_end:required_change_str(change,"old_end")?, etag:required_change_str(change,"etag")?, raw_sha256:required_change_str(change,"raw_sha256")?,
        })),
        ("calendar", "invitation_response") => Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::InvitationResponse {
            target:required_target(target,change)?, origin:required_change_str(change,"origin")?, collection:required_change_str(change,"collection")?, uid:required_change_str(change,"uid")?, recurrence_id:optional_change_str(change,"recurrence_id")?, attendee:required_change_str(change,"attendee")?, organizer:required_change_str(change,"organizer")?, old_response:required_change_str(change,"old_response")?, response:required_change_str(change,"response")?, etag:required_change_str(change,"etag")?, raw_sha256:required_change_str(change,"raw_sha256")?,
        })),
        ("calendar", "task_parent") => Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::TaskParent { target:required_target(target,change)?, origin:required_change_str(change,"origin")?, collection:required_change_str(change,"collection")?, uid:required_change_str(change,"uid")?, summary:required_change_str(change,"summary")?, old_parent:optional_change_str(change,"old_parent")?, new_parent:optional_change_str(change,"new_parent")?, etag:required_change_str(change,"etag")?, checkpoint:required_change_str(change,"checkpoint")?, raw_sha256:required_change_str(change,"raw_sha256")? })),
        ("calendar", "task_attributes") => Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::TaskAttributes { target:required_target(target,change)?, origin:required_change_str(change,"origin")?, collection:required_change_str(change,"collection")?, uid:required_change_str(change,"uid")?, name:required_change_str(change,"name")?, priority:match change.get("priority") { None | Some(serde_json::Value::Null) => None, Some(v) => Some(v.as_u64().and_then(|n| u8::try_from(n).ok()).ok_or("invalid_priority")?) }, due:optional_change_str(change,"due")?, due_date:required_change_bool(change,"due_date")?, start:optional_change_str(change,"start")?, old_start:optional_change_str(change,"old_start")?, new_start:optional_change_str(change,"new_start")?, old_priority:optional_change_str(change,"old_priority")?, old_due:optional_change_str(change,"old_due")?, etag:required_change_str(change,"etag")?, raw_sha256:required_change_str(change,"raw_sha256")? })),
        ("calendar", "calendar_set_zoned_time") => Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::ZonedTime { target:required_target(target,change)?, origin:required_change_str(change,"origin")?, collection:required_change_str(change,"collection")?, uid:required_change_str(change,"uid")?, start:required_change_str(change,"start")?, end:required_change_str(change,"end")?, tzid:required_change_str(change,"tzid")?, old_start:required_change_str(change,"old_start")?, old_start_zone:optional_change_str(change,"old_start_zone")?, old_end:required_change_str(change,"old_end")?, old_end_zone:optional_change_str(change,"old_end_zone")?, etag:required_change_str(change,"etag")?, raw_sha256:required_change_str(change,"raw_sha256")? })),
        ("calendar", "calendar_cancel") => Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Cancellation { target:required_target(target,change)?, collection:required_change_str(change,"collection")?, uid:required_change_str(change,"uid")?, recurrence_id:optional_change_str(change,"recurrence_id")?, sequence:change.get("sequence").and_then(serde_json::Value::as_u64).and_then(|n| u32::try_from(n).ok()).ok_or("invalid_sequence")?, effect:change.get("effect").filter(|v| v.is_object()).cloned().ok_or("invalid_cancellation_effect")?, etag:required_change_str(change,"etag")?, raw_sha256:required_change_str(change,"raw_sha256")? })),
        ("calendar", "calendar_exception") => Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Exception { target:required_target(target,change)?, origin:required_change_str(change,"origin")?, collection:required_change_str(change,"collection")?, uid:required_change_str(change,"uid")?, recurrence_id:required_change_str(change,"recurrence_id")?, summary:required_change_str(change,"summary")?, old_start:required_change_str(change,"old_start")?, old_end:required_change_str(change,"old_end")?, start:required_change_str(change,"start")?, end:required_change_str(change,"end")?, window_start:required_change_str(change,"window_start")?, window_end:required_change_str(change,"window_end")?, etag:required_change_str(change,"etag")?, raw_sha256:required_change_str(change,"raw_sha256")? })),
        ("calendar", "calendar_series_rule") => Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SeriesRule { target:required_target(target,change)?, collection:required_change_str(change,"collection")?, uid:required_change_str(change,"uid")?, rule:required_change_str(change,"rule")?, effect:change.get("effect").filter(|v| v.is_object()).cloned().ok_or("invalid_series_effect")?, etag:required_change_str(change,"etag")?, raw_sha256:required_change_str(change,"raw_sha256")? })),
        ("calendar", "calendar_alarm") => Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Alarm { target:required_target(target,change)?, collection:required_change_str(change,"collection")?, uid:required_change_str(change,"uid")?, recurrence_id:optional_change_str(change,"recurrence_id")?, index:optional_alarm_index(change)?, trigger:optional_change_str(change,"trigger")?, description:optional_change_str(change,"description")?, effect:change.get("effect").filter(|v| v.is_object()).cloned().ok_or("invalid_alarm_effect")?, etag:required_change_str(change,"etag")?, raw_sha256:required_change_str(change,"raw_sha256")? })),
        ("calendar", "task_occurrence_completion") => Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::TaskOccurrence { target:required_target(target,change)?, collection:required_change_str(change,"collection")?, uid:required_change_str(change,"uid")?, recurrence_id:required_change_str(change,"recurrence_id")?, completed_at:required_change_str(change,"completed_at")?, window_start:required_change_str(change,"window_start")?, window_end:required_change_str(change,"window_end")?, effect:change.get("effect").filter(|v| v.is_object()).cloned().ok_or("invalid_task_occurrence_effect")?, etag:required_change_str(change,"etag")?, raw_sha256:required_change_str(change,"raw_sha256")? })),
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
        ("contacts", "contact_set_name") => Ok(AgentLiveWriteIntent::Contacts(ContactLiveWrite::SetName {
            target:required_target(target, change)?, origin:required_change_str(change,"origin")?, collection:required_change_str(change,"collection")?,
            old_name:required_change_str(change,"old_name")?, new_name:required_change_str(change,"new_name")?,
            etag:required_change_str(change,"etag")?, raw_sha256:required_change_str(change,"raw_sha256")?,
        })),
        ("contacts", "contact_set_field") => Ok(AgentLiveWriteIntent::Contacts(ContactLiveWrite::SetField { target:required_target(target,change)?, origin:required_change_str(change,"origin")?, collection:required_change_str(change,"collection")?, key:required_change_str(change,"key")?, index:change.get("index").and_then(serde_json::Value::as_u64).and_then(|n| usize::try_from(n).ok()).ok_or("invalid_field_index")?, version:required_change_str(change,"version")?, property_name:required_change_str(change,"property_name")?, parameters:change.get("parameters").filter(|v| v.is_array()).cloned().ok_or("invalid_field_parameters")?, value_type:required_change_str(change,"value_type")?, old_value:required_change_str(change,"old_value")?, new_value:required_change_str(change,"new_value")?, etag:required_change_str(change,"etag")?, raw_sha256:required_change_str(change,"raw_sha256")? })),
        ("contacts", "contact_add_field") => Ok(AgentLiveWriteIntent::Contacts(ContactLiveWrite::AddField { target:required_target(target,change)?, origin:required_change_str(change,"origin")?, collection:required_change_str(change,"collection")?, key:required_change_str(change,"key")?, version:required_change_str(change,"version")?, value_type:required_change_str(change,"value_type")?, new_value:required_change_str(change,"new_value")?, etag:required_change_str(change,"etag")?, raw_sha256:required_change_str(change,"raw_sha256")? })),
        ("contacts", "contact_remove_field") => Ok(AgentLiveWriteIntent::Contacts(ContactLiveWrite::RemoveField { target:required_target(target,change)?, origin:required_change_str(change,"origin")?, collection:required_change_str(change,"collection")?, key:required_change_str(change,"key")?, index:change.get("index").and_then(serde_json::Value::as_u64).and_then(|n| usize::try_from(n).ok()).ok_or("invalid_field_index")?, version:required_change_str(change,"version")?, property_name:required_change_str(change,"property_name")?, parameters:change.get("parameters").filter(|v| v.is_array()).cloned().ok_or("invalid_field_parameters")?, value_type:required_change_str(change,"value_type")?, old_value:required_change_str(change,"old_value")?, etag:required_change_str(change,"etag")?, raw_sha256:required_change_str(change,"raw_sha256")? })),
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
        AgentLiveWriteIntent::DavAcl { .. }
            | AgentLiveWriteIntent::Mail(MailLiveWrite::ProviderAction { .. })
            | AgentLiveWriteIntent::Mail(MailLiveWrite::RelatedDraft { .. })
            | AgentLiveWriteIntent::DavCreate { .. }
            | AgentLiveWriteIntent::DavDelete { .. }
            |
        AgentLiveWriteIntent::Mail(MailLiveWrite::SetRead { .. })
            | AgentLiveWriteIntent::Mail(MailLiveWrite::ProviderCreateDraft { .. })
            | AgentLiveWriteIntent::Mail(MailLiveWrite::SetFlag { .. })
            | AgentLiveWriteIntent::Mail(MailLiveWrite::SetCategories { .. })
            | AgentLiveWriteIntent::Mail(MailLiveWrite::SendApproved { .. })
            | AgentLiveWriteIntent::Contacts(ContactLiveWrite::SetName { .. })
            | AgentLiveWriteIntent::Contacts(ContactLiveWrite::SetField { .. })
            | AgentLiveWriteIntent::Contacts(ContactLiveWrite::AddField { .. })
            | AgentLiveWriteIntent::Contacts(ContactLiveWrite::RemoveField { .. })
            | AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SetTaskCompletion { .. })
            | AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SetText { .. })
            | AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SetTime { .. })
            | AgentLiveWriteIntent::Calendar(CalendarLiveWrite::InvitationResponse { .. })
            | AgentLiveWriteIntent::Calendar(CalendarLiveWrite::TaskParent { .. })
            | AgentLiveWriteIntent::Calendar(CalendarLiveWrite::TaskAttributes { .. })
            | AgentLiveWriteIntent::Calendar(CalendarLiveWrite::ZonedTime { .. })
            | AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Cancellation { .. })
            | AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Exception { .. })
            | AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SeriesRule { .. })
            | AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Alarm { .. })
            | AgentLiveWriteIntent::Calendar(CalendarLiveWrite::TaskOccurrence { .. })
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

fn required_change_bool_with_legacy_alias(
    change: &serde_json::Value,
    key: &str,
    legacy_key: &str,
) -> Result<bool, String> {
    let object = change_object(change)?;
    match (object.get(key), object.get(legacy_key)) {
        (Some(_), Some(_)) => Err(format!(
            "invalid_live_write: {key} and {legacy_key} are mutually exclusive"
        )),
        (Some(value), None) => value
            .as_bool()
            .ok_or_else(|| format!("invalid_live_write: {key} must be a bool")),
        (None, Some(value)) => value
            .as_bool()
            .ok_or_else(|| format!("invalid_live_write: {legacy_key} must be a bool")),
        (None, None) => Err(format!("invalid_live_write: missing bool {key}")),
    }
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

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes).as_ref().iter().map(|byte| format!("{byte:02x}")).collect::<Vec<_>>().concat()
}

/// Bind a proposed contact edit to the exact server-prepared cached bytes and version.
pub(crate) fn bind_provider_contact_name(cfg: &Config, mut action: isyncyou_agent::ToolAction) -> Result<isyncyou_agent::ToolAction, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, service, target, change, .. } = &mut action else { return Ok(action); };
    if service != "contacts" || change.get("verb").and_then(serde_json::Value::as_str) != Some("contact_set_name") { return Ok(action); }
    let collection = required_change_str(change, "collection")?;
    let new_name = required_change_str(change, "new_name")?;
    let href = required_target(target, change)?;
    let prepared = isyncyou_engine::dav_live::prepare_contact_name(cfg, account, &collection, &href, &new_name)?;
    if prepared.edit.href != href || prepared.edit.expected_collection_token.is_some() { return Err("provider_contact_prepare_invalid".into()); }
    *target = Some(href);
    *change = serde_json::json!({"verb":"contact_set_name", "origin":prepared.origin, "collection":collection,
        "old_name":prepared.old_name, "new_name":new_name, "etag":prepared.edit.expected_etag,
        "raw_sha256":sha256_hex(&prepared.edit.raw)});
    Ok(action)
}

pub(crate) fn bind_provider_task_completion(cfg: &Config, mut action: isyncyou_agent::ToolAction) -> Result<isyncyou_agent::ToolAction, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, service, target, change, .. } = &mut action else { return Ok(action); };
    if service != "calendar" || change.get("verb").and_then(serde_json::Value::as_str) != Some("task_set_completion") { return Ok(action); }
    let collection = required_change_str(change, "collection")?;
    let uid = required_change_str(change, "uid")?;
    let recurrence_id = optional_change_str(change, "recurrence_id")?;
    let complete = required_change_bool(change, "complete")?;
    let href = required_target(target, change)?;
    let completed_at = if complete { Some(isyncyou_engine::dav_live::current_task_completion_time()?) } else { None };
    let prepared = isyncyou_engine::dav_live::prepare_task_completion_preview(cfg, account, &collection, &href, &uid, recurrence_id.as_deref(), completed_at.as_deref())?;
    if prepared.edit.href != href || prepared.edit.expected_collection_token.is_some() { return Err("provider_task_prepare_invalid".into()); }
    *target = Some(href);
    *change = serde_json::json!({"verb":"task_set_completion", "origin":prepared.origin, "collection":collection, "uid":uid,
        "recurrence_id":recurrence_id, "complete":complete, "completed_at":completed_at, "summary":prepared.summary,
        "old_status":prepared.old_status, "old_completed":prepared.old_completed, "old_percent":prepared.old_percent,
        "etag":prepared.edit.expected_etag, "raw_sha256":sha256_hex(&prepared.edit.raw)});
    Ok(action)
}

pub(crate) fn bind_provider_draft(cfg: &Config, mut action: isyncyou_agent::ToolAction) -> Result<isyncyou_agent::ToolAction, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, service, change, .. } = &mut action else { return Ok(action); };
    if service != "mail" || change.get("verb").and_then(serde_json::Value::as_str) != Some("provider_create_draft") { return Ok(action); }
    let origin = match cfg.provider_accounts.get(account) {
        Some(isyncyou_core::config::ProviderConfig::SelfHosted { origin, .. }) => origin.clone(),
        _ => return Err("provider_unavailable".into()),
    };
    let draft_mailbox_id = provider_drafts_mailbox(cfg, account)?;
    let upload_ids = optional_string_array(change,"upload_ids")?.unwrap_or_default();
    let uploads = if upload_ids.is_empty() { Vec::new() } else {
        let store = isyncyou_engine::provider_store::open_readonly(cfg,account)?;
        let client = isyncyou_engine::self_hosted_mail::client(cfg,account)?;
        let uploads = isyncyou_engine::self_hosted_mail::resolve_uploaded_attachments(cfg,account,&store,&upload_ids)?;
        let native_account = isyncyou_engine::self_hosted_mail::scope(cfg,account,&client)?.collection;
        if uploads.iter().any(|part| part.remote_account != native_account) { return Err("attachment_account_changed".into()); }
        uploads
    };
    change.as_object_mut().ok_or("invalid_live_write")?.insert("origin".into(), serde_json::json!(origin));
    change.as_object_mut().ok_or("invalid_live_write")?.insert("draft_mailbox_id".into(), serde_json::json!(draft_mailbox_id));
    change.as_object_mut().ok_or("invalid_live_write")?.insert("uploads".into(), serde_json::json!(uploads));
    validate_live_write_action(&action)?;
    Ok(action)
}

pub(crate) fn bind_provider_calendar_text(cfg: &Config, mut action: isyncyou_agent::ToolAction) -> Result<isyncyou_agent::ToolAction, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, service, target, change, .. } = &mut action else { return Ok(action); };
    if service != "calendar" || change.get("verb").and_then(serde_json::Value::as_str) != Some("calendar_set_text") { return Ok(action); }
    let collection = required_change_str(change,"collection")?;
    let uid = required_change_str(change,"uid")?;
    let recurrence_id = optional_change_str(change,"recurrence_id")?;
    let field = required_change_str(change,"field")?;
    let new_value = required_change_str(change,"new_value")?;
    let href = required_target(target,change)?;
    let prepared = isyncyou_engine::dav_live::prepare_text_preview(cfg, account, &collection, &href, &uid, recurrence_id.as_deref(), &field, &new_value)?;
    if prepared.edit.href != href || prepared.edit.expected_collection_token.is_some() { return Err("provider_calendar_prepare_invalid".into()); }
    *target = Some(href);
    *change = serde_json::json!({"verb":"calendar_set_text", "origin":prepared.origin, "collection":collection, "uid":uid, "recurrence_id":recurrence_id,
        "field":field, "old_value":prepared.old_value, "new_value":prepared.new_value, "etag":prepared.edit.expected_etag, "raw_sha256":sha256_hex(&prepared.edit.raw)});
    Ok(action)
}

pub(crate) fn bind_provider_calendar_time(cfg: &Config, mut action: isyncyou_agent::ToolAction) -> Result<isyncyou_agent::ToolAction, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, service, target, change, .. } = &mut action else { return Ok(action); };
    if service != "calendar" || change.get("verb").and_then(serde_json::Value::as_str) != Some("calendar_set_time") { return Ok(action); }
    let collection = required_change_str(change,"collection")?; let uid = required_change_str(change,"uid")?;
    let start = required_change_str(change,"start")?; let end = required_change_str(change,"end")?; let date_only = required_change_bool(change,"date_only")?;
    let href = required_target(target,change)?;
    let prepared = isyncyou_engine::dav_live::prepare_event_time_preview(cfg, account, &collection, &href, &uid, &start, &end, date_only)?;
    if prepared.edit.href != href || prepared.edit.expected_collection_token.is_some() { return Err("provider_calendar_prepare_invalid".into()); }
    *target = Some(href);
    *change = serde_json::json!({"verb":"calendar_set_time", "origin":prepared.origin, "collection":collection, "uid":uid, "start":start, "end":end, "date_only":date_only, "old_start":prepared.old_start, "old_end":prepared.old_end, "etag":prepared.edit.expected_etag, "raw_sha256":sha256_hex(&prepared.edit.raw)});
    Ok(action)
}

pub(crate) fn bind_provider_dav_acl(cfg: &Config, mut action: isyncyou_agent::ToolAction) -> Result<isyncyou_agent::ToolAction, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, service, target, change, .. } = &mut action else { return Ok(action); };
    if !matches!(service.as_str(), "calendar" | "contacts") || change.get("verb").and_then(serde_json::Value::as_str) != Some("dav_acl") { return Ok(action); }
    let origin = match cfg.provider_accounts.get(account) { Some(isyncyou_core::config::ProviderConfig::SelfHosted { origin, .. }) => origin.clone(), _ => return Err("provider_unavailable".into()) };
    let collection = required_change_str(change,"collection")?; let principal = required_change_str(change,"principal")?;
    let privilege = optional_change_str(change,"privilege")?;
    if !matches!(privilege.as_deref(), None | Some("read") | Some("write") | Some("read-write")) { return Err("invalid_acl_privilege".into()); }
    let acl = isyncyou_engine::dav_live::read_acl(cfg, account, if service == "calendar" { "/.well-known/caldav" } else { "/.well-known/carddav" }, &collection, service, None)?;
    if acl.len() > 1024 { return Err("acl_too_large".into()); }
    let expected: Vec<_> = acl.iter().map(|e| serde_json::json!({"principal":e.principal,"granted":e.granted,"privileges":e.privileges,"inherited":e.inherited,"protected":e.protected})).collect();
    *target = Some(collection.clone());
    *change = serde_json::json!({"verb":"dav_acl","origin":origin,"collection":collection,"principal":principal,"privilege":privilege,"expected":expected});
    Ok(action)
}

pub(crate) fn bind_provider_invitation_response(cfg: &Config, mut action: isyncyou_agent::ToolAction) -> Result<isyncyou_agent::ToolAction, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, service, target, change, .. } = &mut action else { return Ok(action); };
    if service != "calendar" || change.get("verb").and_then(serde_json::Value::as_str) != Some("invitation_response") { return Ok(action); }
    let collection = required_change_str(change,"collection")?; let uid = required_change_str(change,"uid")?; let recurrence_id = optional_change_str(change,"recurrence_id")?;
    let attendee = required_change_str(change,"attendee")?; let response = required_change_str(change,"response")?; let href = required_target(target,change)?;
    let prepared = isyncyou_engine::dav_live::prepare_invitation_response(cfg,account,"/.well-known/caldav",&collection,&href,&uid,recurrence_id.as_deref(),&attendee,&response,None)?;
    if prepared.edit.href != href || prepared.edit.expected_collection_token.is_some() { return Err("provider_invitation_prepare_invalid".into()); }
    *target = Some(href);
    *change = serde_json::json!({"verb":"invitation_response","origin":prepared.origin,"collection":collection,"uid":uid,"recurrence_id":recurrence_id,"attendee":prepared.attendee,"organizer":prepared.organizer,"old_response":prepared.old_response,"response":prepared.new_response,"etag":prepared.edit.expected_etag,"raw_sha256":sha256_hex(&prepared.edit.raw)});
    Ok(action)
}

pub(crate) fn bind_provider_dav_delete(cfg: &Config, mut action: isyncyou_agent::ToolAction) -> Result<isyncyou_agent::ToolAction, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, service, target, change, .. } = &mut action else { return Ok(action); };
    if !matches!(service.as_str(),"calendar" | "contacts") || change.get("verb").and_then(serde_json::Value::as_str) != Some("dav_delete") { return Ok(action); }
    let collection = required_change_str(change,"collection")?; let href = required_target(target,change)?;
    let prepared = isyncyou_engine::dav_live::prepare_delete_preview(cfg,account,&collection,service,&href)?;
    if prepared.href != href { return Err("provider_delete_prepare_invalid".into()); }
    *target = Some(href);
    let projection = serde_json::json!({"object":prepared.projection,"photos":prepared.photos.iter().map(|p| serde_json::json!({"value_type":p.value_type,"encoded_size":p.encoded_size,"image_size":p.image_size,"value_sha256":p.value_sha256})).collect::<Vec<_>>()});
    *change = serde_json::json!({"verb":"dav_delete","origin":prepared.origin,"collection":collection,"kind":prepared.kind,"uid":prepared.uid,"name":prepared.name,"etag":prepared.expected_etag,"source_sha256":prepared.source_sha256,"projection":projection});
    Ok(action)
}

pub(crate) fn bind_provider_dav_create(cfg: &Config, mut action: isyncyou_agent::ToolAction) -> Result<isyncyou_agent::ToolAction, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, service, target, change, .. } = &mut action else { return Ok(action); };
    if !matches!(service.as_str(),"calendar" | "contacts") || change.get("verb").and_then(serde_json::Value::as_str) != Some("dav_create") { return Ok(action); }
    let collection = required_change_str(change,"collection")?; let kind = required_change_str(change,"kind")?; let name = required_change_str(change,"name")?;
    let start = optional_change_str(change,"start")?; let end = optional_change_str(change,"end")?; let organizer = optional_change_str(change,"organizer")?; let attendees = optional_string_array(change,"attendees")?.unwrap_or_default();
    let prepared = if let Some(organizer) = &organizer {
        if kind != "event" { return Err("invalid_invitation_kind".into()); }
        isyncyou_engine::dav_live::prepare_invitation_create(cfg,account,"/.well-known/caldav",&collection,&name,start.as_deref().ok_or("missing_start")?,end.as_deref().ok_or("missing_end")?,organizer,&attendees,None)?.create
    } else { if !attendees.is_empty() { return Err("missing_organizer".into()); } isyncyou_engine::dav_live::prepare_create_simple(cfg,account,&collection,&kind,&name,start.as_deref(),end.as_deref())? };
    if prepared.service.as_str() != service.as_str() || prepared.collection != collection { return Err("provider_create_prepare_invalid".into()); }
    *target = Some(prepared.href);
    *change = serde_json::json!({"verb":"dav_create","origin":prepared.origin,"collection":collection,"kind":prepared.kind,"uid":prepared.uid,"timestamp":prepared.timestamp,"name":prepared.name,"start":prepared.start,"end":prepared.end,"organizer":organizer,"attendees":attendees,"raw_sha256":sha256_hex(&prepared.raw)});
    Ok(action)
}

pub(crate) fn bind_provider_contact_field(cfg: &Config, mut action: isyncyou_agent::ToolAction) -> Result<isyncyou_agent::ToolAction, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, service, target, change, .. } = &mut action else { return Ok(action); };
    if service != "contacts" || change.get("verb").and_then(serde_json::Value::as_str) != Some("contact_set_field") { return Ok(action); }
    let collection = required_change_str(change,"collection")?; let href = required_target(target,change)?; let key = required_change_str(change,"key")?;
    let index = change.get("index").and_then(serde_json::Value::as_u64).and_then(|n| usize::try_from(n).ok()).ok_or("invalid_field_index")?; let value = required_change_str(change,"new_value")?;
    let prepared = isyncyou_engine::dav_live::prepare_contact_text_preview(cfg,account,&collection,&href,&key,index,&value)?;
    if prepared.edit.href != href || prepared.edit.expected_collection_token.is_some() { return Err("provider_contact_prepare_invalid".into()); }
    *target = Some(href);
    *change = serde_json::json!({"verb":"contact_set_field","origin":prepared.origin,"collection":collection,"key":prepared.key,"index":prepared.index,"version":prepared.version,"property_name":prepared.property_name,"parameters":prepared.parameters,"value_type":prepared.value_type,"old_value":prepared.old_value,"new_value":prepared.new_value,"etag":prepared.edit.expected_etag,"raw_sha256":sha256_hex(&prepared.edit.raw)});
    Ok(action)
}

pub(crate) fn bind_provider_contact_add(cfg: &Config, mut action: isyncyou_agent::ToolAction) -> Result<isyncyou_agent::ToolAction, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, service, target, change, .. } = &mut action else { return Ok(action); };
    if service != "contacts" || change.get("verb").and_then(serde_json::Value::as_str) != Some("contact_add_field") { return Ok(action); }
    let collection = required_change_str(change,"collection")?; let href = required_target(target,change)?; let key = required_change_str(change,"key")?; let value = required_change_str(change,"new_value")?; let value_type = required_change_str(change,"value_type")?;
    let prepared = isyncyou_engine::dav_live::prepare_contact_add_preview(cfg,account,&collection,&href,&key,&value,&value_type)?;
    if prepared.edit.href != href || prepared.edit.expected_collection_token.is_some() { return Err("provider_contact_prepare_invalid".into()); }
    *target = Some(href);
    *change = serde_json::json!({"verb":"contact_add_field","origin":prepared.origin,"collection":collection,"key":prepared.key,"version":prepared.version,"value_type":prepared.value_type,"new_value":prepared.new_value,"etag":prepared.edit.expected_etag,"raw_sha256":sha256_hex(&prepared.edit.raw)});
    Ok(action)
}

pub(crate) fn bind_provider_contact_remove(cfg: &Config, mut action: isyncyou_agent::ToolAction) -> Result<isyncyou_agent::ToolAction, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, service, target, change, .. } = &mut action else { return Ok(action); };
    if service != "contacts" || change.get("verb").and_then(serde_json::Value::as_str) != Some("contact_remove_field") { return Ok(action); }
    let collection = required_change_str(change,"collection")?; let href = required_target(target,change)?; let key = required_change_str(change,"key")?;
    let index = change.get("index").and_then(serde_json::Value::as_u64).and_then(|n| usize::try_from(n).ok()).ok_or("invalid_field_index")?;
    let prepared = isyncyou_engine::dav_live::prepare_contact_remove_preview(cfg,account,&collection,&href,&key,index)?;
    if prepared.edit.href != href || prepared.edit.expected_collection_token.is_some() { return Err("provider_contact_prepare_invalid".into()); }
    *target = Some(href); *change = serde_json::json!({"verb":"contact_remove_field","origin":prepared.origin,"collection":collection,"key":prepared.key,"index":prepared.index,"version":prepared.version,"property_name":prepared.property_name,"parameters":prepared.parameters,"value_type":prepared.value_type,"old_value":prepared.old_value,"etag":prepared.edit.expected_etag,"raw_sha256":sha256_hex(&prepared.edit.raw)});
    Ok(action)
}

pub(crate) fn bind_provider_mail_action(cfg: &Config, mut action: isyncyou_agent::ToolAction) -> Result<isyncyou_agent::ToolAction, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, service, target, change, .. } = &mut action else { return Ok(action); };
    if service != "mail" || change.get("verb").and_then(serde_json::Value::as_str) != Some("provider_mail_action") { return Ok(action); }
    let email_id = required_target(target,change)?;
    if email_id.starts_with("shared-mail:") { return Err("shared_mail_write_unavailable".into()); }
    let requested = change.get("action").cloned().ok_or("invalid_mail_action")?;
    let requested = serde_json::from_value::<isyncyou_engine::self_hosted_mail::MailAction>(requested).map_err(|_| "invalid_mail_action")?;
    let client = isyncyou_engine::self_hosted_mail::client(cfg,account)?;
    let preview = isyncyou_engine::self_hosted_mail::preview_mail_action(cfg,account,&client,&email_id,requested)?;
    *target = Some(email_id);
    *change = serde_json::json!({"verb":"provider_mail_action","preview":preview});
    Ok(action)
}

pub(crate) fn bind_provider_related_draft(cfg: &Config, mut action: isyncyou_agent::ToolAction) -> Result<isyncyou_agent::ToolAction, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, service, target, change, .. } = &mut action else { return Ok(action); };
    if service != "mail" || change.get("verb").and_then(serde_json::Value::as_str) != Some("provider_related_draft") { return Ok(action); }
    let source_id = required_target(target,change)?; if source_id.starts_with("shared-mail:") { return Err("shared_mail_write_unavailable".into()); } let kind_text = required_change_str(change,"kind")?;
    let kind = match kind_text.as_str() { "reply" => isyncyou_engine::self_hosted_mail::RelatedDraftKind::Reply, "forward" => isyncyou_engine::self_hosted_mail::RelatedDraftKind::Forward, _ => return Err("invalid_draft_kind".into()) };
    let subject = required_change_str(change,"subject")?; let body = required_body_str(change,&["body"])?;
    let to = required_string_array(change,"to")?;
    let selected = optional_string_array(change,"selected_blob_ids")?.unwrap_or_default();
    let client = isyncyou_engine::self_hosted_mail::client(cfg,account)?;
    let preview = isyncyou_engine::self_hosted_mail::prepare_related_draft(cfg,account,&client,&source_id,kind,&selected)?;
    let draft = provider_drafts_mailbox(cfg,account)?;
    let html = body.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('\n', "<br>");
    let mut email = serde_json::json!({"mailboxIds":{draft:true},"keywords":{"$draft":true},"subject":subject,"to":to.iter().map(|email| serde_json::json!({"email":email})).collect::<Vec<_>>(),"cc":[],"bcc":[],"bodyValues":{"body":{"value":html}},"htmlBody":[{"partId":"body","type":"text/html"}],"attachments":preview.attachments.iter().map(|p| serde_json::json!({"blobId":p.blob_id,"name":p.filename,"type":p.content_type,"disposition":"attachment"})).collect::<Vec<_>>()});
    if kind_text == "reply" {
        email["inReplyTo"] = preview.source["messageId"].clone();
        let mut references = preview.source["references"].as_array().cloned().unwrap_or_default();
        references.extend(preview.source["messageId"].as_array().ok_or("source_message_id_unavailable")?.iter().cloned());
        email["references"] = serde_json::Value::Array(references);
    }
    *target = Some(source_id); *change = serde_json::json!({"verb":"provider_related_draft","preview":preview,"email":email});
    Ok(action)
}

pub(crate) fn bind_provider_task_parent(cfg:&Config, mut action:isyncyou_agent::ToolAction) -> Result<isyncyou_agent::ToolAction,String> {
    let isyncyou_agent::ToolAction::LiveWrite {account,service,target,change,..} = &mut action else { return Ok(action); };
    if service != "calendar" || change.get("verb").and_then(serde_json::Value::as_str) != Some("task_parent") { return Ok(action); }
    let collection=required_change_str(change,"collection")?; let uid=required_change_str(change,"uid")?; let href=required_target(target,change)?; let parent=optional_change_str(change,"new_parent")?;
    let prepared=isyncyou_engine::dav_live::prepare_task_parent_preview(cfg,account,&collection,&href,&uid,parent.as_deref())?;
    if prepared.edit.href != href { return Err("provider_task_prepare_invalid".into()); }
    let checkpoint=prepared.edit.expected_collection_token.as_ref().ok_or("provider_task_prepare_invalid")?;
    *target=Some(href); *change=serde_json::json!({"verb":"task_parent","origin":prepared.origin,"collection":collection,"uid":uid,"summary":prepared.summary,"old_parent":prepared.old_parent,"new_parent":prepared.new_parent,"etag":prepared.edit.expected_etag,"checkpoint":checkpoint,"raw_sha256":sha256_hex(&prepared.edit.raw)});
    Ok(action)
}

pub(crate) fn bind_provider_task_attributes(cfg: &Config, mut action: isyncyou_agent::ToolAction) -> Result<isyncyou_agent::ToolAction, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, service, target, change, .. } = &mut action else { return Ok(action); };
    if service != "calendar" || change.get("verb").and_then(serde_json::Value::as_str) != Some("task_attributes") { return Ok(action); }
    let collection = required_change_str(change,"collection")?; let uid = required_change_str(change,"uid")?; let href = required_target(target,change)?;
    let priority = match change.get("priority") { None | Some(serde_json::Value::Null) => None, Some(v) => Some(v.as_u64().and_then(|n| u8::try_from(n).ok()).ok_or("invalid_priority")?) };
    let due = optional_change_str(change,"due")?; let due_date = change.get("due_date").and_then(serde_json::Value::as_bool).unwrap_or(false); let start = optional_change_str(change,"start")?;
    let prepared = isyncyou_engine::dav_live::prepare_task_attributes_with_start_preview(cfg,account,&collection,&href,&uid,priority,due.as_deref().map(|d| (d,due_date)),start.as_deref())?;
    if prepared.edit.href != href || prepared.edit.expected_collection_token.is_some() { return Err("provider_task_prepare_invalid".into()); }
    *target = Some(href); *change = serde_json::json!({"verb":"task_attributes","origin":prepared.origin,"collection":collection,"uid":uid,"name":prepared.name,"priority":priority,"due":due,"due_date":due_date,"start":start,"old_start":prepared.old_start,"new_start":prepared.new_start,"old_priority":prepared.old_priority,"old_due":prepared.old_due,"etag":prepared.edit.expected_etag,"raw_sha256":sha256_hex(&prepared.edit.raw)});
    Ok(action)
}

pub(crate) fn bind_provider_zoned_time(cfg: &Config, mut action: isyncyou_agent::ToolAction) -> Result<isyncyou_agent::ToolAction, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, service, target, change, .. } = &mut action else { return Ok(action); };
    if service != "calendar" || change.get("verb").and_then(serde_json::Value::as_str) != Some("calendar_set_zoned_time") { return Ok(action); }
    let collection = required_change_str(change,"collection")?; let uid = required_change_str(change,"uid")?; let href = required_target(target,change)?;
    let start = required_change_str(change,"start")?; let end = required_change_str(change,"end")?; let tzid = required_change_str(change,"tzid")?;
    let prepared = isyncyou_engine::dav_live::prepare_zoned_time_preview(cfg,account,&collection,&href,&uid,&start,&end,&tzid)?;
    if prepared.edit.href != href || prepared.edit.expected_collection_token.is_some() { return Err("provider_calendar_prepare_invalid".into()); }
    *target = Some(href); *change = serde_json::json!({"verb":"calendar_set_zoned_time","origin":prepared.origin,"collection":collection,"uid":uid,"start":start,"end":end,"tzid":tzid,"old_start":prepared.old_start,"old_start_zone":prepared.old_start_zone,"old_end":prepared.old_end,"old_end_zone":prepared.old_end_zone,"etag":prepared.edit.expected_etag,"raw_sha256":sha256_hex(&prepared.edit.raw)});
    Ok(action)
}

pub(crate) fn bind_provider_cancellation(cfg: &Config, mut action: isyncyou_agent::ToolAction) -> Result<isyncyou_agent::ToolAction, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, service, target, change, .. } = &mut action else { return Ok(action); };
    if service != "calendar" || change.get("verb").and_then(serde_json::Value::as_str) != Some("calendar_cancel") { return Ok(action); }
    let collection = required_change_str(change,"collection")?; let uid = required_change_str(change,"uid")?; let href = required_target(target,change)?; let recurrence_id = optional_change_str(change,"recurrence_id")?;
    let sequence = change.get("sequence").and_then(serde_json::Value::as_u64).and_then(|n| u32::try_from(n).ok()).ok_or("invalid_sequence")?;
    let p = isyncyou_engine::dav_live::prepare_cancellation_preview(cfg,account,"/.well-known/caldav",&collection,&href,&uid,recurrence_id.as_deref(),sequence,None)?;
    if p.edit.href != href || p.edit.expected_collection_token.is_some() { return Err("provider_cancellation_prepare_invalid".into()); }
    let effect = serde_json::json!({"origin":p.origin,"summary":p.summary,"organizer":p.organizer,"attendees":p.attendees,"old_sequence":p.old_sequence,"new_sequence":p.new_sequence,"has_alarms":p.has_alarms});
    *target = Some(href); *change = serde_json::json!({"verb":"calendar_cancel","collection":collection,"uid":uid,"recurrence_id":recurrence_id,"sequence":sequence,"effect":effect,"etag":p.edit.expected_etag,"raw_sha256":sha256_hex(&p.edit.raw)});
    Ok(action)
}

pub(crate) fn bind_provider_exception(cfg: &Config, mut action: isyncyou_agent::ToolAction) -> Result<isyncyou_agent::ToolAction, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, service, target, change, .. } = &mut action else { return Ok(action); };
    if service != "calendar" || change.get("verb").and_then(serde_json::Value::as_str) != Some("calendar_exception") { return Ok(action); }
    let collection = required_change_str(change,"collection")?; let uid = required_change_str(change,"uid")?; let href = required_target(target,change)?;
    let recurrence_id = required_change_str(change,"recurrence_id")?; let start = required_change_str(change,"start")?; let end = required_change_str(change,"end")?;
    let window_start = required_change_str(change,"window_start")?; let window_end = required_change_str(change,"window_end")?;
    if !valid_expansion_month(&window_start,&window_end) { return Err("invalid_calendar_window".into()); }
    let p = isyncyou_engine::dav_live::prepare_exception_preview(cfg,account,"/.well-known/caldav",&collection,&href,&uid,&recurrence_id,&start,&end,&window_start,&window_end,None)?;
    if p.edit.href != href || p.edit.expected_collection_token.is_some() { return Err("provider_exception_prepare_invalid".into()); }
    *target = Some(href); *change = serde_json::json!({"verb":"calendar_exception","origin":p.origin,"collection":collection,"uid":uid,"recurrence_id":recurrence_id,"summary":p.summary,"old_start":p.old_start,"old_end":p.old_end,"start":start,"end":end,"window_start":window_start,"window_end":window_end,"etag":p.edit.expected_etag,"raw_sha256":sha256_hex(&p.edit.raw)});
    Ok(action)
}

pub(crate) fn bind_provider_task_occurrence(cfg: &Config, mut action: isyncyou_agent::ToolAction) -> Result<isyncyou_agent::ToolAction, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, service, target, change, .. } = &mut action else { return Ok(action); };
    if service != "calendar" || change.get("verb").and_then(serde_json::Value::as_str) != Some("task_occurrence_completion") { return Ok(action); }
    if !required_change_bool(change,"complete")? { return Err("task_occurrence_reopen_unsupported".into()); }
    let collection = required_change_str(change,"collection")?; let uid = required_change_str(change,"uid")?; let href = required_target(target,change)?;
    let recurrence_id = required_change_str(change,"recurrence_id")?; let window_start = required_change_str(change,"window_start")?; let window_end = required_change_str(change,"window_end")?;
    let completed_at = isyncyou_engine::dav_live::current_task_completion_time()?;
    let p = isyncyou_engine::dav_live::prepare_task_occurrence_completion_preview(cfg,account,"/.well-known/caldav",&collection,&href,&uid,&recurrence_id,Some(&completed_at),&window_start,&window_end,None)?;
    if p.edit.href != href || p.edit.expected_collection_token.is_some() { return Err("provider_task_prepare_invalid".into()); }
    let effect = serde_json::json!({"origin":p.origin,"summary":p.summary,"start":p.start,"due":p.due,"completed_at":p.completed_at});
    *target = Some(href); *change = serde_json::json!({"verb":"task_occurrence_completion","collection":collection,"uid":uid,"recurrence_id":recurrence_id,"completed_at":completed_at,"window_start":window_start,"window_end":window_end,"effect":effect,"etag":p.edit.expected_etag,"raw_sha256":sha256_hex(&p.edit.raw)});
    Ok(action)
}

fn optional_alarm_index(change: &serde_json::Value) -> Result<Option<usize>, String> {
    match change.get("alarm_index") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value.as_u64().and_then(|n| usize::try_from(n).ok()).map(Some).ok_or_else(|| "invalid_alarm_index".into()),
    }
}

pub(crate) fn bind_provider_alarm(cfg: &Config, mut action: isyncyou_agent::ToolAction) -> Result<isyncyou_agent::ToolAction, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, service, target, change, .. } = &mut action else { return Ok(action); };
    if service != "calendar" || change.get("verb").and_then(serde_json::Value::as_str) != Some("calendar_alarm") { return Ok(action); }
    let collection = required_change_str(change,"collection")?; let uid = required_change_str(change,"uid")?; let href = required_target(target,change)?;
    let recurrence_id = optional_change_str(change,"recurrence_id")?; let index = optional_alarm_index(change)?;
    let trigger = optional_change_str(change,"trigger")?; let description = optional_change_str(change,"description")?;
    let p = isyncyou_engine::dav_live::prepare_alarm_preview(cfg,account,&collection,&href,&uid,recurrence_id.as_deref(),index,trigger.as_deref(),description.as_deref())?;
    if p.edit.href != href || p.edit.expected_collection_token.is_some() { return Err("provider_alarm_prepare_invalid".into()); }
    let effect = serde_json::json!({"origin":p.origin,"summary":p.summary,"old_trigger":p.old_trigger,"old_description":p.old_description,"new_trigger":p.new_trigger,"new_description":p.new_description});
    *target = Some(href); *change = serde_json::json!({"verb":"calendar_alarm","collection":collection,"uid":uid,"recurrence_id":recurrence_id,"alarm_index":index,"trigger":trigger,"description":description,"effect":effect,"etag":p.edit.expected_etag,"raw_sha256":sha256_hex(&p.edit.raw)});
    Ok(action)
}

pub(crate) fn bind_provider_series_rule(cfg: &Config, mut action: isyncyou_agent::ToolAction) -> Result<isyncyou_agent::ToolAction, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, service, target, change, .. } = &mut action else { return Ok(action); };
    if service != "calendar" || change.get("verb").and_then(serde_json::Value::as_str) != Some("calendar_series_rule") { return Ok(action); }
    let collection = required_change_str(change,"collection")?; let uid = required_change_str(change,"uid")?; let href = required_target(target,change)?; let rule = required_change_str(change,"rule")?;
    let p = isyncyou_engine::dav_live::prepare_series_rule_preview(cfg,account,&collection,&href,&uid,&rule)?;
    if p.edit.href != href || p.edit.expected_collection_token.is_some() { return Err("provider_series_prepare_invalid".into()); }
    let effect = serde_json::json!({"origin":p.origin,"summary":p.summary,"start":p.start,"end":p.end,"old_rule":p.old_rule,"new_rule":p.new_rule,"old_occurrences":p.old_occurrences,"new_occurrences":p.new_occurrences});
    *target = Some(href); *change = serde_json::json!({"verb":"calendar_series_rule","collection":collection,"uid":uid,"rule":rule,"effect":effect,"etag":p.edit.expected_etag,"raw_sha256":sha256_hex(&p.edit.raw)});
    Ok(action)
}

/// Preview only the complete immutable capture intent prepared by the host.
pub(crate) fn single_capture_preview(action: &isyncyou_agent::ToolAction) -> Result<PendingOperationPreview, String> {
    let isyncyou_agent::ToolAction::LiveWrite { account, service, target, change } = action else {
        return Err("invalid_capture_intent".into());
    };
    if service != "calendar" || target.is_some() || change.get("verb").and_then(serde_json::Value::as_str) != Some("single_capture") {
        return Err("invalid_capture_intent".into());
    }
    let operation = required_change_str(change, "operation_id")?;
    let digest = required_change_str(change, "request_sha256")?;
    let value = change.get("intent").ok_or("invalid_capture_intent")?;
    let intent: isyncyou_engine::self_hosted_mail_restore::ApprovedSingleCapture =
        serde_json::from_value(value.clone()).map_err(|_| "invalid_capture_intent")?;
    if change.as_object().is_none_or(|v| v.len()!=4) ||
        serde_json::to_value(&intent).ok().as_ref() != Some(value) ||
        intent.account_id != *account || intent.operation_id != operation ||
        digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) ||
        isyncyou_engine::self_hosted_mail_restore::validate_single_capture_approval(&intent).is_err() {
        return Err("invalid_capture_intent".into());
    }
    Ok(PendingOperationPreview::destructive(format!("Request a fresh backup capture for account {}, {}: {}. Collection {}, object {}, version {}. The snapshot must start after confirmation; off-node protection is established only after a verified PBS binding.", account,intent.kind,intent.title,intent.collection,intent.href,intent.version)))
}

pub(crate) fn contact_photo_preview(action:&isyncyou_agent::ToolAction)->Result<PendingOperationPreview,String> {
    let isyncyou_agent::ToolAction::LiveWrite {account,service,target,change}=action else {return Err("invalid_photo_intent".into())};
    if service!="contacts" || change.get("verb").and_then(serde_json::Value::as_str)!=Some("contact_set_photo") || change.as_object().is_none_or(|o|o.len()!=14) {return Err("invalid_photo_intent".into());}
    let get=|key:&str| required_change_str(change,key);
    let stage=get("stage_id")?;let digest=get("request_sha256")?;let origin=get("origin")?;let collection=get("collection")?;let href=get("href")?;let media=get("media_type")?;let sha=get("sha256")?;let uid=get("uid")?;let name=get("name")?;let etag=get("etag")?;let raw=get("raw_sha256")?;
    let size=change.get("size").and_then(serde_json::Value::as_u64).filter(|n|*n>0 && *n<=2*1024*1024).ok_or("invalid_photo_intent")?;
    let hash=|v:&str|v.len()==64 && v.bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    let old=change.get("old_photo_uri_sha256").ok_or("invalid_photo_intent")?;
    if stage.len()>128 || !stage.bytes().all(|b|b.is_ascii_alphanumeric() || b==b'-' || b==b'_') || !hash(&digest) || !hash(&sha) || !hash(&raw) || !(old.is_null() || old.as_str().is_some_and(hash)) || !matches!(media.as_str(),"image/png"|"image/jpeg") || target.as_deref()!=Some(href.as_str()) || etag.starts_with("W/") || [&origin,&collection,&href,&uid,&name,&etag].iter().any(|s|s.len()>4096 || s.chars().any(char::is_control)) {return Err("invalid_photo_intent".into());}
    Ok(PendingOperationPreview::destructive(format!("Change contact photo for account {account}, contact {name} (UID {uid}), address book {collection}, resource {href}, version {etag}. New {media} image: {size} bytes, SHA-256 {sha}; previous photo URI SHA-256: {}. The exact updated contact is bound to SHA-256 {raw}.",old.as_str().unwrap_or("none"))))
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
            if let isyncyou_agent::ToolAction::LiveWrite { account,service,change,.. } = action {
                if service=="contacts" && change.get("verb").and_then(serde_json::Value::as_str)==Some("contact_set_photo") {return contact_photo_preview(action);}
                if service=="calendar" && change.get("verb").and_then(serde_json::Value::as_str)==Some("single_capture") {
                    return single_capture_preview(action);
                }
                if service=="calendar" && change.get("verb").and_then(serde_json::Value::as_str)==Some("recovery_jobs") {
                    let batch=required_change_str(change,"batch_id")?; let digest=required_change_str(change,"request_sha256")?;
                    let effect=change.get("effect").filter(|v| v.is_object()).ok_or("invalid_recovery_effect")?;
                    if batch.len()>128 || digest.len()!=64 || effect.to_string().len()>128*1024 { return Err("invalid_recovery_effect".into()); }
                    return Ok(PendingOperationPreview::destructive(format!("isyncyou-recovery-jobs-v1\n{batch}\nRequest historical recovery objects for account {account}: {effect}. A second confirmation is required before any import.")));
                }
                if service == "calendar" && change.get("verb").and_then(serde_json::Value::as_str)==Some("recovery_batch_import") {
                    let batch=required_change_str(change,"batch_id")?;
                    let manifest=required_change_str(change,"manifest_sha256")?;
                    let approval=required_change_str(change,"approval_sha256")?;
                    let effect=required_change_str(change,"effect_sha256")?;
                    if batch.len()>128 || manifest.len()!=64 || approval.len()!=64 || effect.len()!=64 { return Err("invalid_recovery_effect".into()); }
                    return Ok(PendingOperationPreview::destructive(format!("isyncyou-recovery-review-v1\nReview the complete recovered-object projections for account {account} before confirming. The bound approval is {approval}; uncertain writes are reconciled without resending.")));
                }
            }
            let intent = validate_live_write_action(action)?;
            if let AgentLiveWriteIntent::DavAcl { service, origin, collection, principal, privilege, expected } = &intent {
                return Ok(PendingOperationPreview::destructive(format!("Change {service} collection access in account {} at {origin}, collection {collection}. Principal: {principal}. New privilege: {}. Current complete access list: {expected}", action.account(), privilege.as_deref().unwrap_or("remove editable entry"))));
            }
            if let AgentLiveWriteIntent::DavDelete { service, origin, collection, target, kind, uid, name, projection, .. } = &intent {
                return Ok(PendingOperationPreview::destructive(format!("Delete {kind} ‘{name}’ (UID {uid}) from account {}, {service} collection {collection} at {origin}, resource {target}. Entire projected object: {projection}. Only a single unscheduled, non-repeating event without alarms or a contact is accepted.", action.account())));
            }
            if let AgentLiveWriteIntent::DavCreate { service, origin, collection, target, kind, uid, name, start, end, organizer, attendees, .. } = &intent {
                return Ok(PendingOperationPreview::destructive(format!("Create {kind} ‘{name}’ (UID {uid}) in account {}, {service} collection {collection} at {origin}, resource {target}. Start: {}; end: {}. Organizer: {}; attendees: {}. No recurrence, alarms or parent relationship. Server acceptance does not prove invitation delivery.", action.account(), start.as_deref().unwrap_or("unset"), end.as_deref().unwrap_or("unset"), organizer.as_deref().unwrap_or("none"), if attendees.is_empty() {"none".into()} else {attendees.join(", ")})));
            }
            if let AgentLiveWriteIntent::Contacts(ContactLiveWrite::SetName { target, origin, collection, old_name, new_name, .. }) = &intent {
                return Ok(PendingOperationPreview::destructive(format!("Change contact name from ‘{old_name}’ to ‘{new_name}’ in account {} at {origin}, address book {collection}, contact {target}.", action.account())));
            }
            if let AgentLiveWriteIntent::Contacts(ContactLiveWrite::SetField { target, origin, collection, key, index, version, property_name, parameters, value_type, old_value, new_value, .. }) = &intent {
                return Ok(PendingOperationPreview::destructive(format!("Change contact field {key} index {index} ({property_name}, vCard {version}, type {value_type}, parameters {parameters}) in account {} at {origin}, address book {collection}, contact {target}. Previous: {old_value}. New: {new_value}.", action.account())));
            }
            if let AgentLiveWriteIntent::Contacts(ContactLiveWrite::AddField { target, origin, collection, key, version, value_type, new_value, .. }) = &intent {
                return Ok(PendingOperationPreview::destructive(format!("Add {key} ({value_type}) value ‘{new_value}’ to vCard {version} in account {} at {origin}, address book {collection}, contact {target}.", action.account())));
            }
            if let AgentLiveWriteIntent::Contacts(ContactLiveWrite::RemoveField { target, origin, collection, key, index, version, property_name, parameters, value_type, old_value, .. }) = &intent {
                return Ok(PendingOperationPreview::destructive(format!("Remove contact field {key} index {index} ({property_name}, vCard {version}, type {value_type}, parameters {parameters}) from account {} at {origin}, address book {collection}, contact {target}. Entire removed value: {old_value}.", action.account())));
            }
            if let AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SetTaskCompletion { target, origin, collection, uid, recurrence_id, complete, summary, old_status, old_completed, old_percent, .. }) = &intent {
                return Ok(PendingOperationPreview::destructive(format!("{} task ‘{summary}’ (UID {uid}, occurrence {}) in account {} at {origin}, calendar {collection}, resource {target}. Previous status: {}; completed: {}; percent: {}.", if *complete { "Complete" } else { "Reopen" }, recurrence_id.as_deref().unwrap_or("series/master"), action.account(), old_status.as_deref().unwrap_or("unset"), old_completed.as_deref().unwrap_or("unset"), old_percent.as_deref().unwrap_or("unset"))));
            }
            if let AgentLiveWriteIntent::Mail(MailLiveWrite::ProviderCreateDraft { origin, draft_mailbox_id, subject, body, to, uploads, .. }) = &intent {
                return Ok(PendingOperationPreview::destructive(format!("Create a draft in account {} ({origin}), Drafts mailbox {draft_mailbox_id}. It will not be sent.\nTo: {}\nSubject: {subject}\nAttachments: {uploads}\n\n{body}", action.account(), to.join(", "))));
            }
            if let AgentLiveWriteIntent::Mail(MailLiveWrite::ProviderAction { preview }) = &intent {
                return Ok(PendingOperationPreview::destructive(format!("Change provider email in account {}. Exact native identity, current memberships and keywords, and requested effect: {preview}. Trash removes the message from all current mailboxes and moves it to Trash.", action.account())));
            }
            if let AgentLiveWriteIntent::Mail(MailLiveWrite::RelatedDraft { preview, email }) = &intent {
                return Ok(PendingOperationPreview::destructive(format!("Create a reply or forward draft in account {}. The message is not sent. Source and selected attachments: {preview}. Exact draft, recipients and content: {email}", action.account())));
            }
            if let AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SetText { target, origin, collection, uid, recurrence_id, field, old_value, new_value, .. }) = &intent {
                return Ok(PendingOperationPreview::destructive(format!("Change {field} for UID {uid}, occurrence {}, in account {} at {origin}, calendar {collection}, resource {target}.\nPrevious: {}\nNew: {new_value}", recurrence_id.as_deref().unwrap_or("series/master"), action.account(), old_value.as_deref().unwrap_or("unset"))));
            }
            if let AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SetTime { target, origin, collection, uid, old_start, old_end, start, end, date_only, .. }) = &intent {
                return Ok(PendingOperationPreview::destructive(format!("Change event UID {uid} in account {} at {origin}, calendar {collection}, resource {target}. Previous start: {old_start}; end: {old_end}. New start: {start}; end: {end}. {}", action.account(), if *date_only { "All-day; end date is exclusive." } else { "Times are UTC." })));
            }
            if let AgentLiveWriteIntent::Calendar(CalendarLiveWrite::InvitationResponse { target, origin, collection, uid, recurrence_id, attendee, organizer, old_response, response, .. }) = &intent {
                return Ok(PendingOperationPreview::destructive(format!("Respond to invitation UID {uid}, occurrence {}, in account {} at {origin}, calendar {collection}, resource {target}. Attendee: {attendee}; organizer: {organizer}. Previous response: {old_response}; new response: {response}. This may notify the organizer.", recurrence_id.as_deref().unwrap_or("single event"), action.account())));
            }
            if let AgentLiveWriteIntent::Calendar(CalendarLiveWrite::TaskParent {target,origin,collection,uid,summary,old_parent,new_parent,..}) = &intent {
                return Ok(PendingOperationPreview::destructive(format!("Change parent of task ‘{summary}’ (UID {uid}) in account {} at {origin}, collection {collection}, resource {target}. Previous parent UID: {}; new parent UID: {}. The task graph is checked under a collection lock before writing.",action.account(),old_parent.as_deref().unwrap_or("none"),new_parent.as_deref().unwrap_or("none"))));
            }
            if let AgentLiveWriteIntent::Calendar(CalendarLiveWrite::TaskAttributes { target, origin, collection, uid, name, priority, due, due_date, old_start, new_start, old_priority, old_due, .. }) = &intent {
                return Ok(PendingOperationPreview::destructive(format!("Change task ‘{name}’ (UID {uid}) in account {} at {origin}, collection {collection}, resource {target}. Previous priority: {}; due: {}; start: {}. New priority: {}; due: {} ({}); start: {}. Unset new fields remain unchanged.", action.account(), old_priority.as_deref().unwrap_or("unset"), old_due.as_deref().unwrap_or("unset"), old_start.as_deref().unwrap_or("unset"), priority.map(|v| v.to_string()).unwrap_or("unchanged".into()), due.as_deref().unwrap_or("unchanged"), if *due_date {"date"} else {"UTC"}, new_start.as_deref().unwrap_or("unchanged"))));
            }
            if let AgentLiveWriteIntent::Calendar(CalendarLiveWrite::ZonedTime { target, origin, collection, uid, start, end, tzid, old_start, old_start_zone, old_end, old_end_zone, .. }) = &intent {
                return Ok(PendingOperationPreview::destructive(format!("Change event UID {uid} in account {} at {origin}, calendar {collection}, resource {target}. Previous start: {old_start} (zone {}); end: {old_end} (zone {}). New start: {start}; end: {end}; zone {tzid}.", action.account(), old_start_zone.as_deref().unwrap_or("unspecified"), old_end_zone.as_deref().unwrap_or("unspecified"))));
            }
            if let AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Cancellation { target, collection, uid, recurrence_id, sequence, effect, .. }) = &intent {
                return Ok(PendingOperationPreview::destructive(format!("Cancel event UID {uid}, occurrence {}, in account {}, calendar {collection}, resource {target}, with sequence {sequence}. Exact organizer, attendees and scheduling effects: {effect}. Server acceptance does not prove delivery of cancellation notices.", recurrence_id.as_deref().unwrap_or("single event"), action.account())));
            }
            if let AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Exception { target, origin, collection, uid, recurrence_id, summary, old_start, old_end, start, end, window_start, window_end, .. }) = &intent {
                return Ok(PendingOperationPreview::destructive(format!("Change only occurrence {recurrence_id} of ‘{summary}’ (UID {uid}) in account {} at {origin}, calendar {collection}, resource {target}. Previous: {old_start} to {old_end}; new: {start} to {end} UTC. Server expansion window: {window_start} to {window_end}.", action.account())));
            }
            if let AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SeriesRule { target, collection, uid, rule, effect, .. }) = &intent {
                return Ok(PendingOperationPreview::destructive(format!("Change the entire event or task series UID {uid} in account {}, calendar {collection}, resource {target}, to rule {rule}. Complete old and new occurrence starts and duration: {effect}. These are locally computed and have not yet been verified against server expansion.", action.account())));
            }
            if let AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Alarm { target, collection, uid, recurrence_id, index, effect, .. }) = &intent {
                return Ok(PendingOperationPreview::destructive(format!("Change reminder {} for UID {uid}, occurrence {}, in account {}, calendar {collection}, resource {target}. Exact old and new reminder: {effect}.", index.map(|i| i.to_string()).unwrap_or_else(|| "new".into()), recurrence_id.as_deref().unwrap_or("series/master"), action.account())));
            }
            if let AgentLiveWriteIntent::Calendar(CalendarLiveWrite::TaskOccurrence { target, collection, uid, recurrence_id, effect, .. }) = &intent {
                return Ok(PendingOperationPreview::destructive(format!("Complete only task occurrence {recurrence_id} of UID {uid} in account {}, calendar {collection}, resource {target}. Server-expanded occurrence and fixed completion time: {effect}.", action.account())));
            }
            if let AgentLiveWriteIntent::Mail(MailLiveWrite::SendApproved { origin, mail_from, recipients, email_id, blob_id, .. }) = &intent {
                return Ok(PendingOperationPreview::destructive(format!("Submit email {email_id} (content {blob_id}) via {origin} from {mail_from} to {}. Delivery is not guaranteed by submission.", recipients.join(", "))));
            }
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

/// Enrich a provider submission with content read from the exact native blob.
/// Refuse previews that cannot show the complete effect to the approving user.
pub(crate) fn preview_for_pending_action_with_config(cfg: &Config, action: &isyncyou_agent::ToolAction) -> Result<PendingOperationPreview, String> {
    if let isyncyou_agent::ToolAction::LiveWrite { account,service,change,.. } = action {
        if service=="contacts" && change.get("verb").and_then(serde_json::Value::as_str)==Some("contact_set_photo") {
            if !cfg.accounts.iter().any(|a|a.id==*account) || !matches!(cfg.provider_accounts.get(account),Some(isyncyou_core::config::ProviderConfig::SelfHosted {origin,..}) if change.get("origin").and_then(serde_json::Value::as_str)==Some(origin.as_str())) {return Err("provider_origin_mismatch".into());}
            return contact_photo_preview(action);
        }
        if service=="calendar" && matches!(change.get("verb").and_then(serde_json::Value::as_str),Some("recovery_batch_import" | "recovery_jobs" | "single_capture")) {
            if !cfg.accounts.iter().any(|a| a.id==*account) || !matches!(cfg.provider_accounts.get(account),Some(isyncyou_core::config::ProviderConfig::SelfHosted {..})) { return Err("provider_operation_unavailable".into()); }
            if change.get("verb").and_then(serde_json::Value::as_str)==Some("single_capture") {
                let origin=change.pointer("/intent/origin").and_then(serde_json::Value::as_str).ok_or("invalid_capture_intent")?;
                if !matches!(cfg.provider_accounts.get(account),Some(isyncyou_core::config::ProviderConfig::SelfHosted {origin:actual,..}) if actual==origin) { return Err("provider_origin_mismatch".into()); }
            }
            return preview_for_pending_action(action);
        }
    }
    if cfg.provider_accounts.contains_key(action.account()) {
        let supported = match action {
            isyncyou_agent::ToolAction::LiveWrite { .. } => matches!(validate_live_write_action(action),
                Ok(AgentLiveWriteIntent::DavAcl { .. } | AgentLiveWriteIntent::DavDelete { .. } | AgentLiveWriteIntent::DavCreate { .. }) |
                Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::SetRead { .. } | MailLiveWrite::SendApproved { .. } | MailLiveWrite::ProviderCreateDraft { .. } | MailLiveWrite::ProviderAction { .. } | MailLiveWrite::RelatedDraft { .. })) |
                Ok(AgentLiveWriteIntent::Contacts(ContactLiveWrite::SetName { .. } | ContactLiveWrite::SetField { .. } | ContactLiveWrite::AddField { .. } | ContactLiveWrite::RemoveField { .. })) |
                Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SetTaskCompletion { .. } | CalendarLiveWrite::SetText { .. } | CalendarLiveWrite::SetTime { .. } | CalendarLiveWrite::InvitationResponse { .. } | CalendarLiveWrite::TaskAttributes { .. } | CalendarLiveWrite::TaskParent { .. } | CalendarLiveWrite::ZonedTime { .. } | CalendarLiveWrite::Cancellation { .. } | CalendarLiveWrite::Exception { .. } | CalendarLiveWrite::SeriesRule { .. } | CalendarLiveWrite::Alarm { .. } | CalendarLiveWrite::TaskOccurrence { .. }))),
            _ => false,
        };
        if !supported { return Err("provider_operation_unavailable".into()); }
    }
    let mut preview = preview_for_pending_action(action)?;
    if let isyncyou_agent::ToolAction::LiveWrite { account, change, .. } = action {
        if let (Some(isyncyou_core::config::ProviderConfig::SelfHosted { origin, .. }), Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::SetRead { target, is_read }))) = (cfg.provider_accounts.get(account), validate_live_write_action(action)) {
            if change.get("origin").and_then(serde_json::Value::as_str) != Some(origin.as_str()) { return Err("provider_origin_mismatch".into()); }
            let client = isyncyou_engine::self_hosted_mail::client(cfg, account)?;
            let body = isyncyou_engine::self_hosted_mail::read_body(cfg, account, &client, &target, 4096)?;
            let rows = body["list"].as_array().ok_or("provider_preview_unavailable")?;
            if rows.len() != 1 || rows[0]["id"].as_str() != Some(target.as_str()) { return Err("provider_preview_unavailable".into()); }
            let subject = rows[0]["subject"].as_str().ok_or("provider_preview_unavailable")?;
            if subject.len() > 1024 || subject.chars().any(char::is_control) { return Err("provider_preview_unavailable".into()); }
            preview.text = format!("Mark email ‘{subject}’ as {} in account {account} ({origin}). This identification was read from the provider.", if is_read { "read" } else { "unread" });
            return Ok(preview);
        }
    }
    let AgentLiveWriteIntent::Mail(MailLiveWrite::SendApproved { origin, email_id, blob_id, .. }) = (match action {
        isyncyou_agent::ToolAction::LiveWrite { .. } => validate_live_write_action(action)?,
        _ => return Ok(preview),
    }) else { return Ok(preview); };
    let account = action.account();
    match cfg.provider_accounts.get(account) {
        Some(isyncyou_core::config::ProviderConfig::SelfHosted { origin: configured, .. }) if configured == &origin => {},
        _ => return Err("provider_origin_mismatch".into()),
    }
    let client = isyncyou_engine::self_hosted_mail::client(cfg, account)?;
    let body = isyncyou_engine::self_hosted_mail::read_body(cfg, account, &client, &email_id, 128 * 1024)?;
    append_verified_submission_preview(&mut preview, account, &email_id, &blob_id, &body)?;
    Ok(preview)
}

pub(crate) fn append_verified_submission_preview(preview: &mut PendingOperationPreview, account: &str, email_id: &str, blob_id: &str, body: &serde_json::Value) -> Result<(), String> {
    let rows = body["list"].as_array().ok_or("provider_preview_unavailable")?;
    if rows.len() != 1 || rows[0]["id"].as_str() != Some(email_id) || rows[0]["blobId"].as_str() != Some(blob_id) {
        return Err("provider_preview_unavailable".into());
    }
    let row = &rows[0];
    if ["from", "sender", "replyTo", "to", "cc", "bcc", "header:Importance:asText", "header:Disposition-Notification-To:asAddresses"].iter().any(|key| row.get(key).is_none()) { return Err("provider_preview_unavailable".into()); }
    let subject = row["subject"].as_str().ok_or("provider_preview_unavailable")?;
    if subject.chars().any(char::is_control) { return Err("provider_preview_unavailable".into()); }
    let mut bodies = Vec::new();
    let mut body_part_ids = Vec::new();
    for (field, kind) in [("textBody", "text"), ("htmlBody", "html")] {
        let parts = row[field].as_array().ok_or("provider_preview_unavailable")?;
        for part in parts {
            let part_id = part["partId"].as_str().ok_or("provider_preview_unavailable")?;
            body_part_ids.push(part_id.to_string());
            let value = &row["bodyValues"][part_id];
            if value["isTruncated"].as_bool() != Some(false) || value["isEncodingProblem"].as_bool() != Some(false) { return Err("provider_preview_unavailable".into()); }
            let text = value["value"].as_str().ok_or("provider_preview_unavailable")?;
            let content = if kind == "html" { isyncyou_webui::sanitize_confirmation_mail_html(text) } else { text.to_string() };
            bodies.push(serde_json::json!({"kind":kind,"content":content}));
        }
    }
    if bodies.is_empty() { return Err("provider_preview_unavailable".into()); }
    let attachments = row["attachments"].as_array().ok_or("provider_preview_unavailable")?;
    let mut manifest = Vec::with_capacity(attachments.len());
    for item in attachments {
        let blob = item["blobId"].as_str().filter(|s| !s.is_empty()).ok_or("provider_preview_unavailable")?;
        let mime = item["type"].as_str().filter(|s| !s.is_empty()).ok_or("provider_preview_unavailable")?;
        let size = item["size"].as_u64().ok_or("provider_preview_unavailable")?;
        let name = item["name"].as_str().unwrap_or("(unnamed)");
        if [blob,mime,name].iter().any(|s| s.chars().any(char::is_control)) { return Err("provider_preview_unavailable".into()); }
        manifest.push(serde_json::json!({"name":name,"type":mime,"size":size,"blob_id":blob}));
    }
    fn collect_inline(part: &serde_json::Value, body_ids: &[String], manifest: &mut Vec<serde_json::Value>) -> Result<(), String> {
        if let Some(children) = part["subParts"].as_array() {
            for child in children { collect_inline(child, body_ids, manifest)?; }
        }
        let is_body = part["partId"].as_str().is_some_and(|id| body_ids.iter().any(|known| known == id));
        if !is_body && part["blobId"].as_str().is_some() {
            let blob = part["blobId"].as_str().filter(|s| !s.is_empty()).ok_or("provider_preview_unavailable")?;
            let mime = part["type"].as_str().filter(|s| !s.is_empty()).ok_or("provider_preview_unavailable")?;
            let size = part["size"].as_u64().ok_or("provider_preview_unavailable")?;
            let name = part["name"].as_str().unwrap_or("(inline)");
            if [blob,mime,name].iter().any(|s| s.chars().any(char::is_control)) { return Err("provider_preview_unavailable".into()); }
            if !manifest.iter().any(|entry| entry["blob_id"] == blob && entry["name"] == name) {
                manifest.push(serde_json::json!({"name":name,"type":mime,"size":size,"blob_id":blob}));
            }
        }
        Ok(())
    }
    if !row["bodyStructure"].is_object() { return Err("provider_preview_unavailable".into()); }
    collect_inline(&row["bodyStructure"], &body_part_ids, &mut manifest)?;
    if row["hasAttachment"].as_bool().ok_or("provider_preview_unavailable")? && manifest.is_empty() { return Err("provider_preview_unavailable".into()); }
    let value = serde_json::json!({"summary":preview.text,"account":account,"subject":subject,"provenance":"live","email_id":email_id,"blob_id":blob_id,"bodies":bodies,"attachments":manifest,
        "from":row.get("from"),"sender":row.get("sender"),"reply_to":row.get("replyTo"),"to":row.get("to"),"cc":row.get("cc"),"bcc":row.get("bcc"),
        "importance":row.get("header:Importance:asText"),"importance_observed":row.get("header:Importance:asText").is_some(),
        "read_receipt":row.get("header:Disposition-Notification-To:asAddresses"),"read_receipt_observed":row.get("header:Disposition-Notification-To:asAddresses").is_some()});
    let encoded = serde_json::to_string(&value).map_err(|_| "provider_preview_unavailable")?;
    preview.text = format!("isyncyou-mail-confirm-v1\n{encoded}");
    if preview.text.len() > 128 * 1024 { return Err("provider_preview_unavailable".into()); }
    Ok(())
}

pub(crate) fn pending_action_validation_diagnostic(error: &str) -> &'static str {
    if error.starts_with("unsupported_live_write_service:") {
        "unsupported_service"
    } else if error == "invalid_live_write: missing verb" {
        "missing_verb"
    } else if error == "invalid_live_write: change must be an object" {
        "invalid_change_object"
    } else if error == "invalid_live_write: missing target"
        || error == "invalid_live_write: target is empty"
    {
        "missing_target"
    } else if error == "invalid_live_write: target is too long"
        || error == "invalid_live_write: target contains control characters"
    {
        "invalid_target"
    } else if error == "invalid_live_write: missing bool is_read" {
        "missing_read_boolean"
    } else if error == "invalid_live_write: is_read and value are mutually exclusive" {
        "ambiguous_read_boolean"
    } else if error == "invalid_live_write: is_read must be a bool"
        || error == "invalid_live_write: value must be a bool"
    {
        "invalid_read_boolean"
    } else if error.starts_with("unsupported_live_write_operation:") {
        "unsupported_operation"
    } else {
        "invalid_action"
    }
}

pub(crate) fn log_pending_action_validation(error: &str) {
    let category = pending_action_validation_diagnostic(error);
    let message = format!("agent_pending_action_invalid={category}");
    #[cfg(target_os = "android")]
    {
        use std::ffi::CString;
        use std::os::raw::{c_char, c_int};

        unsafe extern "C" {
            fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
        }

        let Ok(tag) = CString::new("iSyncYou") else {
            return;
        };
        let Ok(message) = CString::new(message) else {
            return;
        };
        unsafe {
            let _ = __android_log_write(4, tag.as_ptr(), message.as_ptr());
        }
    }
    #[cfg(not(target_os = "android"))]
    eprintln!("isyncyoud: {message}");
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
        emit: &mut dyn isyncyou_agent::TurnEventSink,
    ) -> Result<String, isyncyou_agent::AgentError> {
        match action {
            isyncyou_agent::ToolAction::RestoreLocal { .. } => self.execute_read(action),
            _ => self.delegate.execute_read_streamed(action, emit),
        }
    }

    fn execute_read_with_context(
        &self,
        action: &isyncyou_agent::ToolAction,
        context: isyncyou_agent::ReadExecutionContext<'_, '_>,
    ) -> Result<isyncyou_agent::ReadExecutionOutputV2, isyncyou_agent::AgentError> {
        match action {
            isyncyou_agent::ToolAction::RestoreLocal { .. } => {
                let content =
                    self.execute_read_prepared(action, context.binding, context.local_effect)?;
                context.input_budget.charge(&content)?;
                Ok(isyncyou_agent::ReadExecutionOutputV2::RestoreLocal(
                    isyncyou_agent::ExistingSharedReadOutputV2 {
                        content,
                        untrusted: true,
                        assistant_sources: Vec::new(),
                    },
                ))
            }
            _ => self.delegate.execute_read_with_context(action, context),
        }
    }

    fn finish_with_exit(
        &self,
        exit: isyncyou_agent::TurnExitKind,
        proposed_text: Option<String>,
        assistant_sources: Vec<isyncyou_agent::SourceRef>,
        events: &mut dyn isyncyou_agent::TurnEventSink,
    ) -> Result<isyncyou_agent::TurnExitOutputV1, isyncyou_agent::AgentError> {
        self.delegate
            .finish_with_exit(exit, proposed_text, assistant_sources, events)
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
    config_path: Option<std::path::PathBuf>,
}

impl DesktopAgentOperations {
    pub(crate) fn new(cfg: Config, gate: Arc<Mutex<()>>) -> Self {
        Self { cfg, gate, config_path: None }
    }
    pub(crate) fn with_config_path(cfg: Config, gate: Arc<Mutex<()>>, config_path: std::path::PathBuf) -> Self {
        Self { cfg, gate, config_path: Some(config_path) }
    }
    fn current_config(&self) -> Result<Config, String> {
        match &self.config_path {
            None => Ok(self.cfg.clone()),
            Some(path) => { let cfg = Config::load(path)?; cfg.validate().map_err(|_| "account_config_unavailable".to_string())?; Ok(cfg) }
        }
    }
}

impl DesktopAgentOperations {
    fn execute_confirmed_inner(
        &self,
        action: &isyncyou_agent::ToolAction,
    ) -> Result<ConfirmedActionResult, String> {
        let cfg = self.current_config()?;
        if cfg.provider_accounts.contains_key(action.account()) {
            return Err("provider_operation_unavailable".into());
        }
        match action {
            isyncyou_agent::ToolAction::Backup { account, services } => {
                let run = run_backup_account(&cfg, account, &self.gate, services)?;
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
                let run = run_restore_cloud(&cfg, account, service, id, &self.gate)?;
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
                let run = run_live_write(&cfg, action, &self.gate)?;
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
                let run = run_share(&cfg, action, &self.gate)?;
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

impl AgentConfirmedActionExecutor for DesktopAgentOperations {
    fn resolved_account_key(
        &self,
        action: &isyncyou_agent::ToolAction,
    ) -> Result<String, ClosedExecutionCode> {
        let cfg = self.current_config().map_err(|_| ClosedExecutionCode::ExecutorRejected)?;
        exact_account_key(&cfg, action)
    }

    fn execute_confirmed(&self, action: &isyncyou_agent::ToolAction) -> ConfirmedExecutionOutcome {
        classify_confirmed_execution(action, || self.execute_confirmed_inner(action))
    }
    fn execute_confirmed_bound(&self, action: &isyncyou_agent::ToolAction, pending_id: &str) -> ConfirmedExecutionOutcome {
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::TaskOccurrence { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_task_occurrence(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Alarm { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_alarm(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SeriesRule { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_series_rule(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Exception { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_exception(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Cancellation { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_cancellation(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::ZonedTime { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_zoned_time(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Contacts(ContactLiveWrite::RemoveField { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_contact_remove(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::TaskParent { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_task_parent(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::TaskAttributes { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_task_attributes(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::RelatedDraft { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_related_draft(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Contacts(ContactLiveWrite::AddField { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_contact_add(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::ProviderAction { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_provider_mail_action(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Contacts(ContactLiveWrite::SetField { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_contact_field(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::DavCreate { .. })) {
            return match self.current_config() { Ok(cfg) => execute_bound_dav_create(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::DavDelete { .. })) {
            return match self.current_config() { Ok(cfg) => execute_bound_dav_delete(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::InvitationResponse { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_invitation_response(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::DavAcl { .. })) {
            return match self.current_config() { Ok(cfg) => execute_bound_dav_acl(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SetTime { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_calendar_time(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SetText { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_calendar_text(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::ProviderCreateDraft { .. }))) {
            return match self.current_config() {
                Ok(cfg) => execute_bound_provider_draft(&cfg, action, pending_id, &self.gate),
                Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected),
            };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SetTaskCompletion { .. }))) {
            return match self.current_config() {
                Ok(cfg) => execute_bound_task_completion(&cfg, action, pending_id, &self.gate),
                Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected),
            };
        }
        if is_provider_submission(action) {
            return match self.current_config() {
                Ok(cfg) => execute_bound_provider_submission(&cfg, action, pending_id, &self.gate),
                Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected),
            };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Contacts(ContactLiveWrite::SetName { .. }))) {
            return match self.current_config() {
                Ok(cfg) => execute_bound_provider_contact_name(&cfg, action, pending_id, &self.gate),
                Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected),
            };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::SetRead { .. }))) {
            if let Ok(cfg) = self.current_config() {
                if cfg.provider_accounts.contains_key(action.account()) {
                    return execute_bound_provider_read_state(&cfg, action, pending_id, &self.gate);
                }
            }
        }
        if self.current_config().map(|cfg| cfg.provider_accounts.contains_key(action.account())).unwrap_or(true) {
            return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected);
        }
        self.execute_confirmed(action)
    }
}

pub(crate) struct MobileDisabledAgentOperations {
    cfg: Config,
}

impl AgentConfirmedActionExecutor for MobileDisabledAgentOperations {
    fn resolved_account_key(
        &self,
        action: &isyncyou_agent::ToolAction,
    ) -> Result<String, ClosedExecutionCode> {
        exact_account_key(&self.cfg, action)
    }

    fn execute_confirmed(&self, _action: &isyncyou_agent::ToolAction) -> ConfirmedExecutionOutcome {
        ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected)
    }
}

pub(crate) struct MobileFullNodeAgentOperations {
    cfg: Config,
    config_path: Option<std::path::PathBuf>,
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
            config_path: None,
            gate,
            mobile_jobs,
        }
    }
    pub(crate) fn with_config_path(cfg: Config, gate: Arc<Mutex<()>>, mobile_jobs: Arc<MobileJobRuntime>, config_path: std::path::PathBuf) -> Self {
        Self { cfg, gate, mobile_jobs, config_path: Some(config_path) }
    }
    fn current_config(&self) -> Result<Config, String> {
        match &self.config_path {
            None => Ok(self.cfg.clone()),
            Some(path) => { let cfg = Config::load(path)?; cfg.validate().map_err(|_| "account_config_unavailable".to_string())?; Ok(cfg) }
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

impl MobileFullNodeAgentOperations {
    fn execute_confirmed_inner(
        &self,
        action: &isyncyou_agent::ToolAction,
    ) -> Result<ConfirmedActionResult, String> {
        let cfg = self.current_config()?;
        if cfg.provider_accounts.contains_key(action.account()) {
            return Err("provider_operation_unavailable".into());
        }
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
                let run = run_live_write(&cfg, action, &self.gate)?;
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
                let run = run_share(&cfg, action, &self.gate)?;
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

impl AgentConfirmedActionExecutor for MobileFullNodeAgentOperations {
    fn resolved_account_key(
        &self,
        action: &isyncyou_agent::ToolAction,
    ) -> Result<String, ClosedExecutionCode> {
        let cfg = self.current_config().map_err(|_| ClosedExecutionCode::ExecutorRejected)?;
        exact_account_key(&cfg, action)
    }

    fn execute_confirmed(&self, action: &isyncyou_agent::ToolAction) -> ConfirmedExecutionOutcome {
        if matches!(action, isyncyou_agent::ToolAction::LiveWrite { .. })
            && ensure_mobile_live_write_allowlisted(action).is_err()
        {
            return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected);
        }
        classify_confirmed_execution(action, || self.execute_confirmed_inner(action))
    }
    fn execute_confirmed_bound(&self, action: &isyncyou_agent::ToolAction, pending_id: &str) -> ConfirmedExecutionOutcome {
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::TaskOccurrence { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_task_occurrence(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Alarm { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_alarm(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SeriesRule { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_series_rule(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Exception { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_exception(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::Cancellation { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_cancellation(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::ZonedTime { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_zoned_time(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Contacts(ContactLiveWrite::RemoveField { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_contact_remove(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::TaskParent { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_task_parent(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::TaskAttributes { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_task_attributes(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::RelatedDraft { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_related_draft(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Contacts(ContactLiveWrite::AddField { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_contact_add(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::ProviderAction { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_provider_mail_action(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Contacts(ContactLiveWrite::SetField { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_contact_field(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::DavCreate { .. })) {
            return match self.current_config() { Ok(cfg) => execute_bound_dav_create(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::DavDelete { .. })) {
            return match self.current_config() { Ok(cfg) => execute_bound_dav_delete(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::InvitationResponse { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_invitation_response(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::DavAcl { .. })) {
            return match self.current_config() { Ok(cfg) => execute_bound_dav_acl(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SetTime { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_calendar_time(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SetText { .. }))) {
            return match self.current_config() { Ok(cfg) => execute_bound_calendar_text(&cfg, action, pending_id, &self.gate), Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected) };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::ProviderCreateDraft { .. }))) {
            return match self.current_config() {
                Ok(cfg) => execute_bound_provider_draft(&cfg, action, pending_id, &self.gate),
                Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected),
            };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::SetTaskCompletion { .. }))) {
            return match self.current_config() {
                Ok(cfg) => execute_bound_task_completion(&cfg, action, pending_id, &self.gate),
                Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected),
            };
        }
        if is_provider_submission(action) {
            return match self.current_config() {
                Ok(cfg) => execute_bound_provider_submission(&cfg, action, pending_id, &self.gate),
                Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected),
            };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Contacts(ContactLiveWrite::SetName { .. }))) {
            return match self.current_config() {
                Ok(cfg) => execute_bound_provider_contact_name(&cfg, action, pending_id, &self.gate),
                Err(_) => ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected),
            };
        }
        if matches!(validate_live_write_action(action), Ok(AgentLiveWriteIntent::Mail(MailLiveWrite::SetRead { .. }))) {
            if let Ok(cfg) = self.current_config() {
                if cfg.provider_accounts.contains_key(action.account()) {
                    return execute_bound_provider_read_state(&cfg, action, pending_id, &self.gate);
                }
            }
        }
        if self.current_config().map(|cfg| cfg.provider_accounts.contains_key(action.account())).unwrap_or(true) {
            return ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected);
        }
        self.execute_confirmed(action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn contact_photo_preview_binds_identity_version_and_hashes_without_bytes() {
        let action=isyncyou_agent::ToolAction::LiveWrite {account:"acct".into(),service:"contacts".into(),target:Some("/book/a.vcf".into()),change:json!({"verb":"contact_set_photo","stage_id":"photo-1","request_sha256":"a".repeat(64),"origin":"https://example.test/","collection":"/book/","href":"/book/a.vcf","media_type":"image/png","sha256":"b".repeat(64),"size":128,"uid":"uid-1","name":"Example","old_photo_uri_sha256":null,"etag":"\"v1\"","raw_sha256":"c".repeat(64)})};
        let preview=contact_photo_preview(&action).unwrap();assert!(preview.text.contains("Example"));assert!(preview.text.contains(&"b".repeat(64)));assert!(preview.text.contains("v1"));assert!(!preview.text.contains("photo_base64"));
        for field in ["stage_id","request_sha256","origin","collection","href","media_type","sha256","uid","name","etag","raw_sha256"] {let mut altered=action.clone();if let isyncyou_agent::ToolAction::LiveWrite {change,..}=&mut altered {change[field]=serde_json::Value::Null;}assert!(contact_photo_preview(&altered).is_err(),"accepted missing {field}");}
        let mut too_large=action.clone();if let isyncyou_agent::ToolAction::LiveWrite {change,..}=&mut too_large {change["size"]=json!(2*1024*1024+1);}assert!(contact_photo_preview(&too_large).is_err());
    }

    #[test]
    fn single_capture_preview_accepts_only_the_exact_prepared_effect() {
        let fields = json!(["acct", "https://example.test/", "/.well-known/caldav", "calendar",
            "/cal/", "/cal/object.ics", "native", "a".repeat(64), "b".repeat(64),
            "op-1", "event", "Appointment", "fresh", "confirmation"]);
        let approval = sha256_hex(fields.to_string().as_bytes());
        let intent = json!({"account_id":"acct","origin":"https://example.test/","entry":"/.well-known/caldav",
            "service":"calendar","collection":"/cal/","href":"/cal/object.ics","native_account":"native",
            "object":"a".repeat(64),"version":"b".repeat(64),"operation_id":"op-1","kind":"event",
            "title":"Appointment","mode":"fresh","confirmation":"confirmation","approval_sha256":approval});
        let action = isyncyou_agent::ToolAction::LiveWrite { account:"acct".into(), service:"calendar".into(), target:None,
            change:json!({"verb":"single_capture","operation_id":"op-1","request_sha256":"c".repeat(64),"intent":intent}) };
        let preview = preview_for_pending_action(&action).unwrap();
        assert_eq!(preview.text, single_capture_preview(&action).unwrap().text);
        assert!(preview.text.contains("Appointment"));
        let mut cfg=Config::default();
        cfg.accounts.push(isyncyou_core::config::AccountConfig { id:"acct".into(), username:"test".into(),
            sync_root:"/tmp/sync".into(), archive_root:"/tmp/archive".into(), cache_root:"/tmp/cache".into(), mount_point:None });
        cfg.provider_accounts.insert("acct".into(),isyncyou_core::config::ProviderConfig::SelfHosted {
            origin:"https://example.test/".into(), client_id:"test".into() });
        assert_eq!(preview_for_pending_action_with_config(&cfg,&action).unwrap().text,preview.text);
        cfg.provider_accounts.insert("acct".into(),isyncyou_core::config::ProviderConfig::SelfHosted {
            origin:"https://other.test/".into(), client_id:"test".into() });
        assert!(preview_for_pending_action_with_config(&cfg,&action).is_err());
        for field in ["account_id", "origin", "service", "title", "object", "version", "kind", "mode", "confirmation", "approval_sha256"] {
            let mut changed=action.clone();
            if let isyncyou_agent::ToolAction::LiveWrite { change, .. }=&mut changed { change["intent"][field]=json!("changed"); }
            assert!(preview_for_pending_action(&changed).is_err(),"accepted changed {field}");
        }
        for field in ["verb", "operation_id", "request_sha256", "intent"] {
            let mut changed=action.clone();
            if let isyncyou_agent::ToolAction::LiveWrite { change, .. }=&mut changed { change[field]=serde_json::Value::Null; }
            assert!(preview_for_pending_action(&changed).is_err(),"accepted missing {field}");
        }
        let mut changed=action;
        if let isyncyou_agent::ToolAction::LiveWrite { account, .. }=&mut changed { *account="other".into(); }
        assert!(preview_for_pending_action(&changed).is_err());
    }

    #[test]
    fn dav_approvals_show_old_access_and_exact_new_effect() {
        let action: isyncyou_agent::ToolAction = serde_json::from_value(json!({"op":"live-write", "account":"test", "service":"calendar", "target":"/cal/",
            "change":{"verb":"dav_acl", "origin":"https://example.test", "collection":"/cal/", "principal":"/principals/alice/", "privilege":"read-write",
                "expected":[{"principal":"/principals/alice/", "granted":true, "privileges":["read"], "inherited":false, "protected":false}]}})).unwrap();
        let preview = preview_for_pending_action(&action).unwrap();
        assert!(preview.text.contains("/principals/alice/"));
        assert!(preview.text.contains("read-write"));
        assert!(preview.text.contains("\"read\""));
        let mut invalid = action.clone();
        if let isyncyou_agent::ToolAction::LiveWrite { change, .. } = &mut invalid { change["privilege"] = json!("owner"); }
        assert!(validate_live_write_action(&invalid).is_err());
    }

    #[test]
    fn event_time_approval_shows_old_new_and_exclusive_end() {
        let action: isyncyou_agent::ToolAction = serde_json::from_value(json!({"op":"live-write", "account":"test", "service":"calendar", "target":"/cal/event.ics",
            "change":{"verb":"calendar_set_time", "origin":"https://example.test", "collection":"/cal/", "uid":"event-1", "start":"20261102", "end":"20261104", "date_only":true,
                "old_start":"20261101", "old_end":"20261102", "etag":"\"v1\"", "raw_sha256":"digest"}})).unwrap();
        let preview = preview_for_pending_action(&action).unwrap();
        for expected in ["20261101", "20261102", "20261104", "exclusive"] { assert!(preview.text.contains(expected)); }
    }

    #[test]
    fn invitation_approval_identifies_organizer_and_occurrence() {
        let action: isyncyou_agent::ToolAction = serde_json::from_value(json!({"op":"live-write", "account":"test", "service":"calendar", "target":"/cal/invite.ics",
            "change":{"verb":"invitation_response", "origin":"https://example.test", "collection":"/cal/", "uid":"event-1", "recurrence_id":"20261102T100000Z", "attendee":"mailto:person@example.test", "organizer":"mailto:owner@example.test", "old_response":"TENTATIVE", "response":"accept", "etag":"\"v1\"", "raw_sha256":"digest"}})).unwrap();
        let preview = preview_for_pending_action(&action).unwrap();
        for expected in ["person@example.test", "owner@example.test", "20261102T100000Z", "TENTATIVE", "accept"] { assert!(preview.text.contains(expected)); }
    }

    #[test]
    fn create_and_delete_approvals_identify_complete_effect() {
        let create: isyncyou_agent::ToolAction = serde_json::from_value(json!({"op":"live-write", "account":"test", "service":"calendar", "target":"/cal/new.ics",
            "change":{"verb":"dav_create", "origin":"https://example.test", "collection":"/cal/", "kind":"event", "uid":"new-uid", "timestamp":"20261101T120000Z", "name":"Planning", "start":"20261102T100000Z", "end":"20261102T110000Z", "raw_sha256":"digest"}})).unwrap();
        let preview = preview_for_pending_action(&create).unwrap();
        for expected in ["Planning", "new-uid", "20261102T100000Z", "20261102T110000Z", "attendees: none"] { assert!(preview.text.contains(expected)); }
        let delete: isyncyou_agent::ToolAction = serde_json::from_value(json!({"op":"live-write", "account":"test", "service":"contacts", "target":"/contacts/card.vcf",
            "change":{"verb":"dav_delete", "origin":"https://example.test", "collection":"/contacts/", "kind":"contact", "uid":"card-uid", "name":"Example", "etag":"\"v1\"", "source_sha256":"digest", "projection":{"email":"person@example.test"}}})).unwrap();
        let preview = preview_for_pending_action(&delete).unwrap();
        for expected in ["card-uid", "Example", "person@example.test"] { assert!(preview.text.contains(expected)); }
    }

    #[test]
    fn contact_delete_approval_shows_photo_metadata_without_photo_bytes() {
        let action: isyncyou_agent::ToolAction = serde_json::from_value(json!({"op":"live-write", "account":"test", "service":"contacts", "target":"/contacts/card.vcf",
            "change":{"verb":"dav_delete", "origin":"https://example.test", "collection":"/contacts/", "kind":"contact", "uid":"card-uid", "name":"Example", "etag":"\"v1\"", "source_sha256":"digest", "projection":{"object":["vcard",[]],"photos":[{"value_type":"uri","encoded_size":1200,"image_size":800,"value_sha256":"photo-hash"}]}}})).unwrap();
        let preview = preview_for_pending_action(&action).unwrap();
        assert!(preview.text.contains("photo-hash"));
        assert!(preview.text.contains("800"));
        assert!(!preview.text.contains("data:image/"));
    }

    #[test]
    fn mail_action_preview_shows_subject_membership_and_trash_effect() {
        let action: isyncyou_agent::ToolAction = serde_json::from_value(json!({"op":"live-write", "account":"test", "service":"mail", "target":"m1",
            "change":{"verb":"provider_mail_action", "preview":{"subject":"Example subject", "mailbox_ids":{"inbox":true}, "action":{"kind":"trash"}}}})).unwrap();
        let preview = preview_for_pending_action(&action).unwrap();
        for expected in ["Example subject", "inbox", "trash", "all current mailboxes"] { assert!(preview.text.contains(expected)); }
    }

    #[test]
    fn task_occurrence_and_alarm_approvals_identify_exact_effect() {
        let occurrence: isyncyou_agent::ToolAction = serde_json::from_value(json!({"op":"live-write", "account":"test", "service":"calendar", "target":"/cal/task.ics",
            "change":{"verb":"task_occurrence_completion", "collection":"/cal/", "uid":"task-1", "recurrence_id":"20261102T100000Z", "completed_at":"20261102T120000Z", "window_start":"20261101T000000Z", "window_end":"20261201T000000Z", "effect":{"origin":"https://example.test", "summary":"Review", "start":"20261102T100000Z", "due":"20261102T110000Z", "completed_at":"20261102T120000Z"}, "etag":"\"v1\"", "raw_sha256":"digest"}})).unwrap();
        let preview = preview_for_pending_action(&occurrence).unwrap();
        for expected in ["only task occurrence", "20261102T100000Z", "Review", "20261102T120000Z"] { assert!(preview.text.contains(expected)); }
        let alarm: isyncyou_agent::ToolAction = serde_json::from_value(json!({"op":"live-write", "account":"test", "service":"calendar", "target":"/cal/task.ics",
            "change":{"verb":"calendar_alarm", "collection":"/cal/", "uid":"task-1", "recurrence_id":null, "alarm_index":0, "trigger":"-PT15M", "description":"Prepare", "effect":{"origin":"https://example.test", "summary":"Review", "old_trigger":"-PT5M", "new_trigger":"-PT15M", "new_description":"Prepare"}, "etag":"\"v1\"", "raw_sha256":"digest"}})).unwrap();
        let preview = preview_for_pending_action(&alarm).unwrap();
        for expected in ["-PT5M", "-PT15M", "Prepare", "task-1"] { assert!(preview.text.contains(expected)); }
        let mut invalid = alarm;
        if let isyncyou_agent::ToolAction::LiveWrite { change, .. } = &mut invalid { change["effect"] = serde_json::Value::Null; }
        assert!(preview_for_pending_action(&invalid).is_err());
    }

    #[test]
    fn task_attributes_approval_shows_previous_and_requested_values() {
        let action: isyncyou_agent::ToolAction = serde_json::from_value(json!({"op":"live-write", "account":"test", "service":"calendar", "target":"/cal/task.ics",
            "change":{"verb":"task_attributes", "origin":"https://example.test", "collection":"/cal/", "uid":"task-1", "name":"Review", "priority":2, "due":"20261104", "due_date":true, "old_priority":"5", "old_due":"20261103", "etag":"\"v1\"", "raw_sha256":"digest"}})).unwrap();
        let preview = preview_for_pending_action(&action).unwrap();
        for expected in ["Review", "20261103", "20261104", "priority", "date"] { assert!(preview.text.contains(expected)); }
    }

    #[test]
    fn scheduling_approvals_expose_recipients_and_delivery_boundary() {
        let create: isyncyou_agent::ToolAction = serde_json::from_value(json!({"op":"live-write", "account":"test", "service":"calendar", "target":"/cal/new.ics",
            "change":{"verb":"dav_create", "origin":"https://example.test", "collection":"/cal/", "kind":"event", "uid":"new-uid", "timestamp":"20261101T120000Z", "name":"Meeting", "start":"20261102T100000Z", "end":"20261102T110000Z", "organizer":"mailto:owner@example.test", "attendees":["mailto:guest@example.test"], "raw_sha256":"digest"}})).unwrap();
        let preview = preview_for_pending_action(&create).unwrap();
        for expected in ["owner@example.test", "guest@example.test", "does not prove invitation delivery"] { assert!(preview.text.contains(expected)); }
        let cancel: isyncyou_agent::ToolAction = serde_json::from_value(json!({"op":"live-write", "account":"test", "service":"calendar", "target":"/cal/new.ics",
            "change":{"verb":"calendar_cancel", "collection":"/cal/", "uid":"new-uid", "recurrence_id":null, "sequence":2, "effect":{"organizer":"mailto:owner@example.test", "attendees":["mailto:guest@example.test"], "old_sequence":1, "new_sequence":2, "has_alarms":true}, "etag":"\"v1\"", "raw_sha256":"digest"}})).unwrap();
        let preview = preview_for_pending_action(&cancel).unwrap();
        for expected in ["guest@example.test", "sequence 2", "does not prove delivery"] { assert!(preview.text.contains(expected)); }
    }

    #[test]
    fn occurrence_expansion_window_is_exactly_one_calendar_month() {
        assert!(valid_expansion_month("20261201T000000Z", "20270101T000000Z"));
        assert!(valid_expansion_month("20260201T000000Z", "20260301T000000Z"));
        assert!(!valid_expansion_month("20260201T000000Z", "20260401T000000Z"));
        assert!(!valid_expansion_month("20261301T000000Z", "20270101T000000Z"));
        assert!(!valid_expansion_month("é0260201T000000Z", "20260301T000000Z"));
    }

    #[test]
    fn series_approval_displays_both_occurrence_sets_and_verification_limit() {
        let action: isyncyou_agent::ToolAction = serde_json::from_value(json!({"op":"live-write", "account":"test", "service":"calendar", "target":"/cal/event.ics",
            "change":{"verb":"calendar_series_rule", "collection":"/cal/", "uid":"series-1", "rule":"FREQ=DAILY;COUNT=2", "effect":{"old_occurrences":["20261101T100000Z"],"new_occurrences":["20261101T100000Z","20261102T100000Z"]}, "etag":"\"v1\"", "raw_sha256":"digest"}})).unwrap();
        let preview = preview_for_pending_action(&action).unwrap();
        for expected in ["entire event or task series", "20261101T100000Z", "20261102T100000Z", "not yet been verified"] { assert!(preview.text.contains(expected)); }
    }

    #[test]
    fn task_completion_preview_identifies_occurrence_and_prior_state() {
        let action: isyncyou_agent::ToolAction = serde_json::from_value(json!({
            "op":"live-write", "account":"test", "service":"calendar", "target":"https://example.test/cal/task.ics",
            "change":{"verb":"task_set_completion", "origin":"https://example.test", "collection":"cal", "uid":"task-1",
                "recurrence_id":"20261029T100000Z", "complete":true, "completed_at":"20261029T110000Z",
                "summary":"Review", "old_status":"IN-PROCESS", "old_completed":null, "old_percent":"30", "etag":"\"v1\"", "raw_sha256":"digest"}
        })).unwrap();
        let preview = preview_for_pending_action(&action).unwrap();
        assert!(preview.text.contains("Review"));
        assert!(preview.text.contains("20261029T100000Z"));
        assert!(preview.text.contains("IN-PROCESS"));
        assert!(preview.text.contains("30"));
    }

    #[test]
    fn task_parent_preview_binds_graph_checkpoint_and_both_parents() {
        let value = json!({"op":"live-write","account":"test","service":"calendar","target":"https://example.test/cal/child.ics",
            "change":{"verb":"task_parent","origin":"https://example.test","collection":"/cal/","uid":"child","summary":"Review",
                "old_parent":"old","new_parent":"new","etag":"\"v1\"","checkpoint":"token-1","raw_sha256":"digest"}});
        let action:isyncyou_agent::ToolAction = serde_json::from_value(value.clone()).unwrap();
        let preview = preview_for_pending_action(&action).unwrap();
        for expected in ["Review","child","old","new","collection lock"] { assert!(preview.text.contains(expected)); }
        assert!(matches!(validate_live_write_action(&action),Ok(AgentLiveWriteIntent::Calendar(CalendarLiveWrite::TaskParent { .. }))));
        let mut missing=value; missing["change"].as_object_mut().unwrap().remove("checkpoint");
        let invalid:isyncyou_agent::ToolAction = serde_json::from_value(missing).unwrap();
        assert!(validate_live_write_action(&invalid).is_err());
    }

    #[test]
    fn provider_draft_preview_shows_complete_effect_without_sending() {
        let action: isyncyou_agent::ToolAction = serde_json::from_value(json!({"op":"live-write", "account":"test", "service":"mail",
            "change":{"verb":"provider_create_draft", "origin":"https://example.test", "draft_mailbox_id":"drafts", "subject":"Hello", "body":"First line\n<script>", "to":["person@example.test"]}})).unwrap();
        let preview = preview_for_pending_action(&action).unwrap();
        assert!(preview.text.contains("not be sent"));
        assert!(preview.text.contains("person@example.test"));
        assert!(preview.text.contains("First line\n<script>"));
    }

    #[test]
    fn attachment_draft_preview_includes_bound_file_metadata() {
        let action: isyncyou_agent::ToolAction = serde_json::from_value(json!({"op":"live-write", "account":"test", "service":"mail",
            "change":{"verb":"provider_create_draft", "origin":"https://example.test", "draft_mailbox_id":"drafts", "subject":"Hello", "body":"Text", "to":["person@example.test"], "upload_ids":["upload-1"], "uploads":[{"filename":"report.pdf","size":42,"sha256":"content-digest","state":"confirmed","blob_id":"blob-1"}]}})).unwrap();
        let preview = preview_for_pending_action(&action).unwrap();
        for expected in ["report.pdf", "42", "content-digest", "not be sent"] { assert!(preview.text.contains(expected)); }
    }

    #[test]
    fn calendar_text_preview_shows_exact_old_and_new_values() {
        let action: isyncyou_agent::ToolAction = serde_json::from_value(json!({"op":"live-write", "account":"test", "service":"calendar", "target":"https://example.test/cal/event.ics",
            "change":{"verb":"calendar_set_text", "origin":"https://example.test", "collection":"cal", "uid":"event-1", "recurrence_id":null,
                "field":"SUMMARY", "old_value":"Old title", "new_value":"New title", "etag":"\"v1\"", "raw_sha256":"digest"}})).unwrap();
        let preview = preview_for_pending_action(&action).unwrap();
        assert!(preview.text.contains("Old title"));
        assert!(preview.text.contains("New title"));
        assert!(preview.text.contains("event-1"));
    }
    use std::sync::{mpsc, Mutex as StdMutex};
    use std::time::Duration;

    #[test]
    fn submission_preview_rejects_unverified_or_incomplete_content() {
        let original = json!({"list":[{"id":"e1", "blobId":"b1", "subject":"Subject", "from":[], "sender":[], "replyTo":[], "to":[], "cc":[], "bcc":[], "header:Importance:asText":null, "header:Disposition-Notification-To:asAddresses":null, "hasAttachment":false, "attachments":[], "bodyStructure":{},
            "textBody":[{"partId":"p1"}], "htmlBody":[], "bodyValues":{"p1":{"value":"Hello", "isTruncated":false, "isEncodingProblem":false}}}]});
        let mut preview = PendingOperationPreview::destructive("Submit to to@example.invalid");
        append_verified_submission_preview(&mut preview, "account", "e1", "b1", &original).unwrap();
        assert!(preview.text.contains("Subject"));
        assert!(preview.text.contains("Hello"));
        let mut with_attachment = original.clone();
        with_attachment["list"][0]["hasAttachment"] = json!(true);
        with_attachment["list"][0]["attachments"] = json!([{"blobId":"att1","type":"application/pdf","size":42,"name":"report.pdf"}]);
        with_attachment["list"][0]["htmlBody"] = json!([{"partId":"p2"}]);
        with_attachment["list"][0]["bodyValues"]["p2"] = json!({"value":"<p>Hi</p><script>alert(1)</script>","isTruncated":false, "isEncodingProblem":false});
        let mut preview = PendingOperationPreview::destructive("Submit");
        append_verified_submission_preview(&mut preview, "account", "e1", "b1", &with_attachment).unwrap();
        assert!(preview.text.contains("report.pdf"));
        assert!(!preview.text.contains("<script>"));
        for field in ["wrong_blob", "truncated", "encoding", "missing_header", "attachment", "extra_part"] {
            let mut response = original.clone();
            match field {
                "wrong_blob" => response["list"][0]["blobId"] = json!("other"),
                "truncated" => response["list"][0]["bodyValues"]["p1"]["isTruncated"] = json!(true),
                "encoding" => response["list"][0]["bodyValues"]["p1"]["isEncodingProblem"] = json!(true),
                "missing_header" => { response["list"][0].as_object_mut().unwrap().remove("header:Importance:asText"); },
                "attachment" => response["list"][0]["hasAttachment"] = json!(true),
                _ => response["list"][0]["htmlBody"] = json!([{"partId":"p2"}]),
            }
            assert!(append_verified_submission_preview(&mut PendingOperationPreview::destructive("Submit"), "account", "e1", "b1", &response).is_err(), "{field}");
        }
    }

    #[test]
    fn approved_submission_binds_all_envelope_and_native_fields_to_effect() {
        let input = json!({"op":"live-write", "account":"a", "service":"mail", "change": {
            "verb":"send_approved", "origin":"https://example.invalid", "email_id":"email-1", "blob_id":"blob-1",
            "identity_id":"identity-1", "mail_from":"from@example.invalid", "recipients":["to@example.invalid"],
            "draft_mailbox_id":"drafts", "sent_mailbox_id":"sent"
        }});
        let action = isyncyou_agent::parse_action(&input).unwrap();
        let intent = validate_live_write_action(&action).unwrap();
        let AgentLiveWriteIntent::Mail(MailLiveWrite::SendApproved { recipients, email_id, blob_id, .. }) = intent else { panic!("wrong intent") };
        let effect_hash = isyncyou_agent::confirm::action_hash(&action, 0).unwrap();
        assert_eq!(recipients, vec!["to@example.invalid"]);
        assert_eq!(email_id, "email-1");
        assert_eq!(blob_id, "blob-1");
        let preview = preview_for_pending_action(&action).unwrap();
        assert!(preview.text.contains("to@example.invalid"));
        assert!(preview.text.contains("from@example.invalid"));
        for field in ["origin", "email_id", "blob_id", "identity_id", "mail_from", "draft_mailbox_id", "sent_mailbox_id"] {
            let mut changed = input.clone();
            changed["change"][field] = json!("changed");
            let changed = isyncyou_agent::parse_action(&changed).unwrap();
            assert_ne!(effect_hash, isyncyou_agent::confirm::action_hash(&changed, 0).unwrap(), "{field}");
        }
        let mut many = input.clone();
        many["change"]["recipients"] = json!((0..21).map(|index| format!("person{index}@example.invalid")).collect::<Vec<_>>());
        assert!(validate_live_write_action(&isyncyou_agent::parse_action(&many).unwrap()).is_ok());
        let mut changed = input.clone();
        changed["change"]["recipients"] = json!(["other@example.invalid"]);
        assert_ne!(effect_hash, isyncyou_agent::confirm::action_hash(&isyncyou_agent::parse_action(&changed).unwrap(), 0).unwrap());
    }

    #[test]
    fn contact_name_confirmation_binds_version_and_exact_effect() {
        let input = json!({"op":"live-write", "account":"a", "service":"contacts", "target":"/addressbook/contact.vcf", "change":{
            "verb":"contact_set_name", "origin":"https://example.invalid", "collection":"/addressbook/", "old_name":"Old", "new_name":"New", "etag":"\"v1\"", "raw_sha256":"abcdef"}});
        let action = isyncyou_agent::parse_action(&input).unwrap();
        assert!(matches!(validate_live_write_action(&action), Ok(AgentLiveWriteIntent::Contacts(ContactLiveWrite::SetName { .. }))));
        let hash = isyncyou_agent::confirm::action_hash(&action, 0).unwrap();
        for field in ["origin", "collection", "old_name", "new_name", "etag", "raw_sha256"] {
            let mut changed = input.clone(); changed["change"][field] = json!("changed");
            assert_ne!(hash, isyncyou_agent::confirm::action_hash(&isyncyou_agent::parse_action(&changed).unwrap(), 0).unwrap(), "{field}");
        }
        let mut changed = input; changed["target"] = json!("/addressbook/other.vcf");
        assert_ne!(hash, isyncyou_agent::confirm::action_hash(&isyncyou_agent::parse_action(&changed).unwrap(), 0).unwrap());
    }

    #[test]
    fn provider_read_state_effect_binds_origin_target_and_value() {
        let input = json!({"op":"live-write", "account":"a", "service":"mail", "target":"email-1", "change":{"verb":"set_read", "origin":"https://example.invalid", "is_read":true}});
        let action = isyncyou_agent::parse_action(&input).unwrap();
        let hash = isyncyou_agent::confirm::action_hash(&action, 0).unwrap();
        for field in ["origin", "is_read"] {
            let mut changed = input.clone();
            changed["change"][field] = if field == "is_read" { json!(false) } else { json!("https://other.invalid") };
            assert_ne!(hash, isyncyou_agent::confirm::action_hash(&isyncyou_agent::parse_action(&changed).unwrap(), 0).unwrap());
        }
        let mut changed = input;
        changed["target"] = json!("email-2");
        assert_ne!(hash, isyncyou_agent::confirm::action_hash(&isyncyou_agent::parse_action(&changed).unwrap(), 0).unwrap());
    }

    #[test]
    fn submission_rejection_and_unknown_are_distinct_and_pending_ids_are_stable() {
        use isyncyou_engine::self_hosted_mail::SendOutcome;
        assert_eq!(submission_outcome(SendOutcome::Rejected), ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ProviderRejected));
        assert_eq!(submission_outcome(SendOutcome::Unknown), ConfirmedExecutionOutcome::OutcomeUnknown);
        assert_eq!(bound_submission_operation_id("pending-one"), bound_submission_operation_id("pending-one"));
        assert_ne!(bound_submission_operation_id("pending-one"), bound_submission_operation_id("pending-two"));
        assert!(bound_submission_operation_id("bad\nidentity").is_none());
    }

    #[test]
    fn agent_confirm_production_send_accepted_then_disconnected_is_outcome_unknown() {
        use std::io::Read;
        use std::net::{Shutdown, TcpListener};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut connection = loop {
                match listener.accept() {
                    Ok((connection, _)) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "send request never arrived"
                        );
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept failed: {error}"),
                }
            };
            connection
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 8192);
                connection.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            assert!(request.starts_with(b"POST /me/messages/synthetic-draft/send HTTP/1.1\r\n"));
            // The server has accepted the complete empty-body send request, but
            // closes before returning its acknowledgement to the production client.
            connection.shutdown(Shutdown::Both).unwrap();
            1
        });
        crate::TEST_CONFIRMED_MAIL_CLIENT.with(|slot| {
            *slot.borrow_mut() =
                Some(isyncyou_graph::GraphClient::new("synthetic-token").with_base_url(endpoint));
        });
        let executor = DesktopAgentOperations::new(Config::default(), Arc::new(Mutex::new(())));
        let action = isyncyou_agent::parse_action(&json!({
            "op": "live-write", "account": "synthetic-account", "service": "mail",
            "target": "synthetic-draft", "change": { "verb": "send_draft" }
        }))
        .unwrap();
        assert!(matches!(
            executor.execute_confirmed(&action),
            ConfirmedExecutionOutcome::OutcomeUnknown
        ));
        assert_eq!(server.join().unwrap(), 1);
        crate::TEST_CONFIRMED_MAIL_CLIENT.with(|slot| assert!(slot.borrow().is_none()));
    }

    #[test]
    fn agent_confirm_invalid_action_is_rejected_before_dispatch() {
        let action = isyncyou_agent::parse_action(&json!({
            "op": "live-write", "account": "synthetic-account", "service": "mail",
            "target": "synthetic-message", "change": { "verb": "set_read" }
        }))
        .unwrap();
        assert!(matches!(
            classify_confirmed_execution(&action, || panic!("must not dispatch")),
            ConfirmedExecutionOutcome::Failed(ClosedExecutionCode::ExecutorRejected)
        ));
    }

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
    fn agent_live_write_mail_set_read_accepts_unambiguous_legacy_value() {
        let action = isyncyou_agent::parse_action(&json!({
            "op": "live-write",
            "account": "me",
            "service": "mail",
            "target": "message-1",
            "change": { "verb": "set_read", "value": true }
        }))
        .unwrap();

        let intent = validate_live_write_action(&action).unwrap();
        assert_eq!(
            intent,
            AgentLiveWriteIntent::Mail(MailLiveWrite::SetRead {
                target: "message-1".into(),
                is_read: true,
            })
        );
    }

    #[test]
    fn agent_live_write_mail_set_read_rejects_ambiguous_boolean_fields() {
        let action = isyncyou_agent::parse_action(&json!({
            "op": "live-write",
            "account": "me",
            "service": "mail",
            "target": "message-1",
            "change": {
                "verb": "set_read",
                "is_read": true,
                "value": true
            }
        }))
        .unwrap();

        assert!(validate_live_write_action(&action)
            .unwrap_err()
            .contains("mutually exclusive"));
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
    fn pending_action_validation_diagnostic_is_closed_and_value_free() {
        assert_eq!(
            pending_action_validation_diagnostic("invalid_live_write: missing target"),
            "missing_target"
        );
        assert_eq!(
            pending_action_validation_diagnostic("invalid_live_write: missing bool is_read"),
            "missing_read_boolean"
        );
        assert_eq!(
            pending_action_validation_diagnostic(
                "unsupported_live_write_service: private-account-item"
            ),
            "unsupported_service"
        );
        assert_eq!(
            pending_action_validation_diagnostic("private-account-item"),
            "invalid_action"
        );
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
