//! JMAP mail synchronization into the provider-scoped store.

use isyncyou_connectors::jmap::{EmailUpdateResult, JmapClient, JmapError, SubmissionResult};
use isyncyou_core::{config::ProviderConfig, Config};
use isyncyou_store::{ProviderChange, ProviderObject, ProviderScope, ProviderSubmission, Store};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::Value;
use sha2::{Digest, Sha256};

struct StageGuard<'a> {
    store: &'a Store,
    scope: &'a ProviderScope,
    generation: String,
    finished: bool,
}
impl<'a> StageGuard<'a> {
    fn new(store: &'a Store, scope: &'a ProviderScope) -> Result<Self, String> {
        let mut bytes = [0u8; 32];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| "Staging random source unavailable")?;
        let generation = format!("{:x}", Sha256::digest(bytes));
        store
            .begin_provider_staging(scope, &generation)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            store,
            scope,
            generation,
            finished: false,
        })
    }
    fn page(&self, objects: &[ProviderObject]) -> Result<(), String> {
        self.store
            .stage_provider_objects(self.scope, &self.generation, objects)
            .map_err(|e| e.to_string())
    }
    fn finish(mut self, cursor: &str) -> Result<(), String> {
        self.store
            .finish_provider_staging(self.scope, &self.generation, cursor)
            .map_err(|e| e.to_string())?;
        self.finished = true;
        Ok(())
    }
}
impl Drop for StageGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self
                .store
                .abort_provider_staging(self.scope, &self.generation);
        }
    }
}

fn object(value: &Value) -> Result<ProviderObject, String> {
    let id = value["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("JMAP email has no id")?;
    Ok(ProviderObject {
        remote_id: id.into(),
        etag: None,
        raw: serde_json::to_vec(value).map_err(|_| "Invalid JMAP email")?,
    })
}
fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    value[name]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("JMAP response missing {name}"))
}
fn list(value: &Value, name: &str) -> Result<Vec<String>, String> {
    value[name]
        .as_array()
        .ok_or_else(|| format!("JMAP response missing {name}"))?
        .iter()
        .map(|v| {
            v.as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| "Invalid JMAP id".into())
        })
        .collect()
}

/// Open a client using the installation's authenticated provider grant.
pub struct SelfHostedClient {
    pub(crate) client: JmapClient,
    account: String,
    origin: String,
}

pub fn client(cfg: &Config, account: &str) -> Result<SelfHostedClient, String> {
    let origin = match cfg.provider_accounts.get(account) {
        Some(ProviderConfig::SelfHosted { origin, .. }) => origin,
        None => return Err("Not a self-hosted account".into()),
    };
    let token = crate::provider_auth::self_hosted_access_token(cfg, account)?;
    let client = JmapClient::discover(origin, token).map_err(|_| "JMAP discovery failed")?;
    Ok(SelfHostedClient {
        client,
        account: account.into(),
        origin: origin.clone(),
    })
}

pub fn client_with_store(
    cfg: &Config,
    account: &str,
    credentials: &dyn crate::provider_credential::ProviderCredentialStore,
) -> Result<SelfHostedClient, String> {
    let origin = match cfg.provider_accounts.get(account) {
        Some(ProviderConfig::SelfHosted { origin, .. }) => origin,
        None => return Err("Not a self-hosted account".into()),
    };
    let token =
        crate::provider_auth::self_hosted_access_token_with_store(cfg, account, credentials)?;
    let client = JmapClient::discover(origin, token).map_err(|_| "JMAP discovery failed")?;
    Ok(SelfHostedClient {
        client,
        account: account.into(),
        origin: origin.clone(),
    })
}

pub fn scope(
    cfg: &Config,
    account: &str,
    client: &SelfHostedClient,
) -> Result<ProviderScope, String> {
    let origin = match cfg.provider_accounts.get(account) {
        Some(ProviderConfig::SelfHosted { origin, .. }) => origin.clone(),
        None => return Err("Not a self-hosted account".into()),
    };
    if client.account != account || client.origin != origin {
        return Err("JMAP client account or origin changed".into());
    }
    Ok(ProviderScope {
        account_id: account.into(),
        origin,
        service: "mail".into(),
        collection: client.client.account_id().into(),
    })
}

/// Fetch a bounded native text/HTML body after checking account and origin.
pub fn read_body(
    cfg: &Config,
    account: &str,
    provider: &SelfHostedClient,
    email_id: &str,
    max_bytes: usize,
) -> Result<Value, String> {
    scope(cfg, account, provider)?;
    if email_id.is_empty() {
        return Err("Missing email identity".into());
    }
    provider
        .client
        .get_email_bodies(&[email_id.to_string()], max_bytes)
        .map_err(|_| "JMAP body unavailable".into())
}

/// Fetch native sending identities after account/origin validation.
pub fn sending_identities(
    cfg: &Config,
    account: &str,
    provider: &SelfHostedClient,
) -> Result<Vec<Value>, String> {
    scope(cfg, account, provider)?;
    let response = provider
        .client
        .get("Identity", None, None)
        .map_err(|_| "Sending identities unavailable")?;
    response["list"]
        .as_array()
        .cloned()
        .ok_or_else(|| "Invalid sending identities".into())
}

/// Initial enumeration is reconciled only when all pages share one query state.
/// Subsequent changes are applied with their cursor in the same transaction.
pub fn sync(
    cfg: &Config,
    account: &str,
    store: &Store,
    client: &SelfHostedClient,
) -> Result<usize, String> {
    let folders = sync_mailboxes_inner(cfg, account, store, client, false)?;
    Ok(folders + sync_inner(cfg, account, store, client, false)?)
}

/// Mailbox identity and state are distinct from Email identity and state.
pub fn mailbox_scope(
    cfg: &Config,
    account: &str,
    provider: &SelfHostedClient,
) -> Result<ProviderScope, String> {
    let mut scope = scope(cfg, account, provider)?;
    scope.service = "mailboxes".into();
    Ok(scope)
}

fn sync_mailboxes_inner(
    cfg: &Config,
    account: &str,
    store: &Store,
    provider: &SelfHostedClient,
    reconcile: bool,
) -> Result<usize, String> {
    let scope = mailbox_scope(cfg, account, provider)?;
    let client = &provider.client;
    let page_size = client.max_objects_in_get().min(100);
    if page_size == 0 {
        return Err("JMAP server has no usable batch size".into());
    }
    let mut count = 0;
    let saved = if reconcile {
        None
    } else {
        store.provider_cursor(&scope).map_err(|e| e.to_string())?
    };
    let mut cursor = if let Some(cursor) = saved {
        cursor
    } else {
        let empty: Vec<String> = Vec::new();
        let baseline = client
            .get("Mailbox", Some(&empty), None)
            .map_err(|_| "JMAP mailbox state unavailable")?;
        let baseline = field(&baseline, "state")?.to_owned();
        let staging = StageGuard::new(store, &scope)?;
        let mut position = 0usize;
        let mut query_state = None;
        loop {
            let page = client
                .query_mailboxes(position, page_size)
                .map_err(|_| "JMAP mailbox enumeration failed")?;
            let current = field(&page, "queryState")?;
            if query_state.as_deref().is_some_and(|state| state != current) {
                return Err("JMAP mailbox query changed; retry required".into());
            }
            query_state = Some(current.to_owned());
            if page["position"].as_u64() != Some(position as u64) {
                return Err("Invalid JMAP mailbox position".into());
            }
            let ids = list(&page, "ids")?;
            let total = page["total"]
                .as_u64()
                .ok_or("JMAP mailbox total unavailable")? as usize;
            if position + ids.len() > total || (ids.is_empty() && position != total) {
                return Err("Incomplete JMAP mailbox enumeration".into());
            }
            if !ids.is_empty() {
                let result = client
                    .get("Mailbox", Some(&ids), None)
                    .map_err(|_| "JMAP mailbox fetch failed")?;
                if !list(&result, "notFound")?.is_empty() {
                    return Err("JMAP mailbox changed; retry required".into());
                }
                let expected: std::collections::HashSet<&str> =
                    ids.iter().map(String::as_str).collect();
                if expected.len() != ids.len() {
                    return Err("Duplicate JMAP mailbox".into());
                }
                let mut actual = std::collections::HashSet::new();
                let mut objects = Vec::new();
                for value in result["list"]
                    .as_array()
                    .ok_or("JMAP mailbox list unavailable")?
                {
                    let item = object(value)?;
                    if !expected.contains(item.remote_id.as_str())
                        || !actual.insert(item.remote_id.clone())
                    {
                        return Err("Invalid JMAP mailbox identity".into());
                    }
                    objects.push(item);
                }
                if actual.len() != expected.len() {
                    return Err("Incomplete JMAP mailbox fetch".into());
                }
                staging.page(&objects)?;
                count += objects.len();
            }
            position += ids.len();
            if position == total {
                break;
            }
        }
        staging.finish(&baseline)?;
        baseline
    };
    loop {
        let page = match client.changes("Mailbox", &cursor, page_size) {
            Ok(page) => page,
            Err(JmapError::Method(kind)) if kind == "cannotCalculateChanges" && !reconcile => {
                return sync_mailboxes_inner(cfg, account, store, provider, true)
            }
            Err(_) => return Err("JMAP mailbox delta unavailable".into()),
        };
        if field(&page, "oldState")? != cursor {
            return Err("JMAP mailbox state mismatch".into());
        }
        let next = field(&page, "newState")?.to_owned();
        let mut ids = list(&page, "created")?;
        ids.extend(list(&page, "updated")?);
        let mut changes = Vec::new();
        if !ids.is_empty() {
            let fetched = client
                .get("Mailbox", Some(&ids), None)
                .map_err(|_| "JMAP changed mailbox fetch failed")?;
            if !list(&fetched, "notFound")?.is_empty() {
                return Err("JMAP changed mailbox inaccessible".into());
            }
            let expected: std::collections::HashSet<&str> =
                ids.iter().map(String::as_str).collect();
            let mut actual = std::collections::HashSet::new();
            for value in fetched["list"]
                .as_array()
                .ok_or("JMAP mailbox list unavailable")?
            {
                let item = object(value)?;
                if !expected.contains(item.remote_id.as_str())
                    || !actual.insert(item.remote_id.clone())
                {
                    return Err("Invalid changed JMAP mailbox identity".into());
                }
                changes.push(ProviderChange::Upsert(item));
            }
            if actual.len() != expected.len() {
                return Err("Incomplete changed JMAP mailbox fetch".into());
            }
        }
        changes.extend(
            list(&page, "destroyed")?
                .into_iter()
                .map(ProviderChange::Destroyed),
        );
        count += changes.len();
        store
            .apply_provider_page(&scope, &changes, &next)
            .map_err(|e| e.to_string())?;
        cursor = next;
        match page["hasMoreChanges"].as_bool() {
            Some(false) => break,
            Some(true) => {}
            None => return Err("Invalid JMAP mailbox pagination".into()),
        }
    }
    Ok(count)
}

fn sync_inner(
    cfg: &Config,
    account: &str,
    store: &Store,
    provider: &SelfHostedClient,
    reconcile: bool,
) -> Result<usize, String> {
    let scope = scope(cfg, account, provider)?;
    let client = &provider.client;
    let page_size = client.max_objects_in_get().min(100);
    if page_size == 0 {
        return Err("JMAP server has no usable batch size".into());
    }
    let mut count = 0;
    let cursor = if reconcile {
        None
    } else {
        store.provider_cursor(&scope).map_err(|e| e.to_string())?
    };
    let mut cursor = if let Some(cursor) = cursor {
        cursor
    } else {
        let empty: Vec<String> = Vec::new();
        let baseline = client
            .get("Email", Some(&empty), None)
            .map_err(|_| "JMAP email state unavailable")?;
        let baseline = field(&baseline, "state")?.to_string();
        let staging = StageGuard::new(store, &scope)?;
        let mut position = 0usize;
        let mut query_state = None;
        loop {
            let page = client
                .query_emails(position, page_size, None)
                .map_err(|_| "JMAP email enumeration failed")?;
            let current = field(&page, "queryState")?;
            if query_state.as_deref().is_some_and(|s| s != current) {
                return Err("JMAP query changed during enumeration; retry required".into());
            }
            query_state = Some(current.to_owned());
            if page["position"].as_u64() != Some(position as u64) {
                return Err("Invalid JMAP query position".into());
            }
            let ids = list(&page, "ids")?;
            let total = page["total"]
                .as_u64()
                .ok_or("JMAP query total unavailable")? as usize;
            if position + ids.len() > total || (ids.is_empty() && position != total) {
                return Err("Incomplete JMAP enumeration".into());
            }
            if !ids.is_empty() {
                let fetched = client
                    .get("Email", Some(&ids), None)
                    .map_err(|_| "JMAP email fetch failed")?;
                if !list(&fetched, "notFound")?.is_empty() {
                    return Err("JMAP email changed during enumeration; retry required".into());
                }
                let values = fetched["list"]
                    .as_array()
                    .ok_or("JMAP email list unavailable")?;
                if values.len() != ids.len() {
                    return Err("Incomplete JMAP email fetch".into());
                }
                let expected: std::collections::HashSet<&str> =
                    ids.iter().map(String::as_str).collect();
                if expected.len() != ids.len() {
                    return Err("Duplicate JMAP email in enumeration".into());
                }
                let mut actual = std::collections::HashSet::new();
                let mut objects = Vec::new();
                for value in values {
                    let item = object(value)?;
                    if !expected.contains(item.remote_id.as_str())
                        || !actual.insert(item.remote_id.clone())
                    {
                        return Err("Invalid JMAP email fetch identities".into());
                    }
                    objects.push(item);
                }
                staging.page(&objects)?;
                count += objects.len();
            }
            position += ids.len();
            if position == total {
                break;
            }
        }
        staging.finish(&baseline)?;
        baseline
    };
    loop {
        let page = match client.changes("Email", &cursor, page_size) {
            Ok(page) => page,
            Err(JmapError::Method(kind)) if kind == "cannotCalculateChanges" && !reconcile => {
                return sync_inner(cfg, account, store, provider, true)
            }
            Err(_) => return Err("JMAP delta unavailable".into()),
        };
        if field(&page, "oldState")? != cursor {
            return Err("JMAP delta state mismatch".into());
        }
        let next = field(&page, "newState")?.to_owned();
        let mut ids = list(&page, "created")?;
        ids.extend(list(&page, "updated")?);
        let mut changes = Vec::new();
        if !ids.is_empty() {
            let fetched = client
                .get("Email", Some(&ids), None)
                .map_err(|_| "JMAP changed email fetch failed")?;
            if !list(&fetched, "notFound")?.is_empty() {
                return Err("JMAP changed email inaccessible; retry required".into());
            }
            let expected: std::collections::HashSet<&str> =
                ids.iter().map(String::as_str).collect();
            let mut actual = std::collections::HashSet::new();
            for value in fetched["list"]
                .as_array()
                .ok_or("JMAP email list unavailable")?
            {
                let item = object(value)?;
                if !expected.contains(item.remote_id.as_str())
                    || !actual.insert(item.remote_id.clone())
                {
                    return Err("Invalid JMAP changed email identities".into());
                }
                changes.push(ProviderChange::Upsert(item));
            }
            if actual.len() != expected.len() {
                return Err("Incomplete JMAP changed email fetch".into());
            }
        }
        changes.extend(
            list(&page, "destroyed")?
                .into_iter()
                .map(ProviderChange::Destroyed),
        );
        count += changes.len();
        store
            .apply_provider_page(&scope, &changes, &next)
            .map_err(|e| e.to_string())?;
        cursor = next;
        match page["hasMoreChanges"].as_bool() {
            Some(false) => break,
            Some(true) => {}
            None => return Err("Invalid JMAP pagination".into()),
        }
    }
    Ok(count)
}

/// A previously confirmed exact native email and SMTP envelope. The host must
/// bind its owner/session/expiry approval to all fields before invoking send.
pub struct ApprovedSubmission {
    pub operation_id: String,
    pub email_id: String,
    pub blob_id: String,
    pub identity_id: String,
    pub mail_from: String,
    pub recipients: Vec<String>,
    pub draft_mailbox_id: String,
    pub sent_mailbox_id: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Confirmed(String),
    Submitted { id: String, sent_update: String },
    Rejected,
    Unknown,
}

/// Create one draft with a durable intent; uncertain creation is never replayed.
pub fn create_draft(
    cfg: &Config,
    account: &str,
    store: &Store,
    provider: &SelfHostedClient,
    operation_id: &str,
    email: &Value,
) -> Result<SendOutcome, String> {
    let scope = scope(cfg, account, provider)?;
    if operation_id.is_empty()
        || !email.is_object()
        || email.pointer("/keywords/$draft").and_then(Value::as_bool) != Some(true)
    {
        return Err("Invalid draft intent".into());
    }
    let allowed = [
        "mailboxIds",
        "keywords",
        "subject",
        "to",
        "bodyValues",
        "htmlBody",
    ];
    if email
        .as_object()
        .is_none_or(|fields| fields.keys().any(|key| !allowed.contains(&key.as_str())))
    {
        return Err("Unsupported draft fields".into());
    }
    let subject = email["subject"].as_str().ok_or("Draft subject required")?;
    let recipients = email["to"].as_array().ok_or("Draft recipients required")?;
    if subject.contains('\r')
        || subject.contains('\n')
        || recipients.len() > 1000
        || recipients.iter().any(|recipient| {
            recipient.as_object().is_none_or(|fields| fields.len() != 1)
                || recipient["email"].as_str().is_none_or(|address| {
                    address.is_empty()
                        || address.len() > 320
                        || address.contains('\r')
                        || address.contains('\n')
                })
        })
    {
        return Err("Invalid draft recipients or subject".into());
    }
    let body_values = email["bodyValues"]
        .as_object()
        .ok_or("Draft body required")?;
    if body_values.len() != 1
        || body_values
            .get("body")
            .and_then(Value::as_object)
            .is_none_or(|body| {
                body.len() != 1 || body.get("value").and_then(Value::as_str).is_none()
            })
    {
        return Err("Invalid draft body".into());
    }
    let html = email["htmlBody"].as_array().ok_or("Draft body required")?;
    if html.len() != 1
        || html[0].as_object().is_none_or(|fields| fields.len() != 2)
        || html[0]["partId"].as_str() != Some("body")
        || html[0]["type"].as_str() != Some("text/html")
    {
        return Err("Invalid draft HTML part".into());
    }
    let intent = serde_json::to_string(email).map_err(|_| "Invalid draft intent")?;
    if intent.len() > 16 * 1024 * 1024 {
        return Err("Draft intent exceeds size limit".into());
    }
    let digest = format!("{:x}", Sha256::digest(intent.as_bytes()));
    if let Some((previous, state, result)) = store
        .provider_mutation(&scope, operation_id)
        .map_err(|e| e.to_string())?
    {
        if previous != digest {
            return Err("Draft intent differs from recorded operation".into());
        }
        return Ok(match state.as_str() {
            "confirmed" => SendOutcome::Confirmed(result.ok_or("Missing native draft identity")?),
            "rejected" => SendOutcome::Rejected,
            _ => SendOutcome::Unknown,
        });
    }
    let mailbox_ids = email["mailboxIds"]
        .as_object()
        .ok_or("Draft mailbox required")?;
    if mailbox_ids.len() != 1 {
        return Err("Draft must use one Drafts mailbox".into());
    }
    let (mailbox_id, membership) = mailbox_ids.iter().next().ok_or("Draft mailbox required")?;
    if mailbox_id.is_empty() || membership.as_bool() != Some(true) {
        return Err("Invalid draft mailbox".into());
    }
    let keywords = email["keywords"]
        .as_object()
        .ok_or("Draft keyword required")?;
    if keywords.len() != 1 || keywords.get("$draft").and_then(Value::as_bool) != Some(true) {
        return Err("Invalid draft keywords".into());
    }
    let ids = vec![mailbox_id.clone()];
    let mailboxes = provider
        .client
        .get("Mailbox", Some(&ids), None)
        .map_err(|_| "Draft mailbox unavailable")?;
    let mailboxes = mailboxes["list"]
        .as_array()
        .ok_or("Draft mailbox unavailable")?;
    if mailboxes.len() != 1
        || mailboxes[0]["id"].as_str() != Some(mailbox_id.as_str())
        || mailboxes[0]["role"].as_str() != Some("drafts")
        || mailboxes[0]
            .pointer("/myRights/mayAddItems")
            .and_then(Value::as_bool)
            != Some(true)
    {
        return Err("Draft mailbox is not writable".into());
    }
    if !store
        .claim_provider_mutation(&scope, operation_id, "draft", &intent, &digest)
        .map_err(|e| e.to_string())?
    {
        return Ok(SendOutcome::Unknown);
    }
    let creation_id = format!("op_{:x}", Sha256::digest(operation_id.as_bytes()));
    let result = match provider.client.create_draft(&creation_id, email.clone()) {
        Ok(value) => value,
        Err(_) => return Ok(SendOutcome::Unknown),
    };
    let created = result
        .get("created")
        .and_then(|v| v.get(&creation_id))
        .and_then(|v| v.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    let rejected = result
        .get("notCreated")
        .and_then(|v| v.get(&creation_id))
        .and_then(|v| v.get("type"))
        .and_then(Value::as_str)
        .filter(|kind| !kind.is_empty());
    match (created, rejected) {
        (Some(id), None)
            if store
                .finish_provider_mutation(&scope, operation_id, Some(id))
                .map_err(|e| e.to_string())? =>
        {
            Ok(SendOutcome::Confirmed(id.to_owned()))
        }
        (None, Some(_))
            if store
                .finish_provider_mutation(&scope, operation_id, None)
                .map_err(|e| e.to_string())? =>
        {
            Ok(SendOutcome::Rejected)
        }
        _ => Ok(SendOutcome::Unknown),
    }
}

/// Submit once. An existing or uncertain operation is observed, never resent.
pub fn send_approved(
    cfg: &Config,
    account: &str,
    store: &Store,
    provider: &SelfHostedClient,
    approved: &ApprovedSubmission,
) -> Result<SendOutcome, String> {
    let invalid_address = |value: &str| {
        value.is_empty() || value.len() > 320 || value.contains('\r') || value.contains('\n')
    };
    if approved.operation_id.is_empty()
        || approved.email_id.is_empty()
        || approved.blob_id.is_empty()
        || approved.identity_id.is_empty()
        || approved.draft_mailbox_id.is_empty()
        || approved.sent_mailbox_id.is_empty()
        || approved.draft_mailbox_id == approved.sent_mailbox_id
        || invalid_address(&approved.mail_from)
        || approved.recipients.is_empty()
        || approved.recipients.len() > 1000
        || approved
            .recipients
            .iter()
            .any(|value| invalid_address(value))
    {
        return Err("Incomplete approved submission".into());
    }
    let scope = scope(cfg, account, provider)?;
    let client = &provider.client;
    let intent = serde_json::to_string(&serde_json::json!({"account":account,"origin":scope.origin,"remote_account":scope.collection,"email_id":approved.email_id,"blob_id":approved.blob_id,"identity_id":approved.identity_id,"mail_from":approved.mail_from,"recipients":approved.recipients,"draft_mailbox_id":approved.draft_mailbox_id,"sent_mailbox_id":approved.sent_mailbox_id})).map_err(|_| "Invalid submission")?;
    let digest = format!("{:x}", Sha256::digest(intent.as_bytes()));
    if let Some(existing) = store
        .provider_submission(&scope, &approved.operation_id)
        .map_err(|e| e.to_string())?
    {
        if existing.snapshot_digest != digest {
            return Err("Approval does not match recorded submission".into());
        }
        return Ok(match existing.state.as_str() {
            "confirmed" => SendOutcome::Submitted {
                id: existing
                    .submission_id
                    .ok_or("Missing native submission identity")?,
                sent_update: existing.sent_update.unwrap_or_else(|| "unknown".into()),
            },
            "rejected" => SendOutcome::Rejected,
            _ => SendOutcome::Unknown,
        });
    }
    let ids = vec![approved.email_id.clone()];
    let email = client
        .get("Email", Some(&ids), None)
        .map_err(|_| "Approved email unavailable")?;
    let rows = email["list"].as_array().ok_or("Invalid approved email")?;
    if rows.len() != 1
        || rows[0]["id"].as_str() != Some(approved.email_id.as_str())
        || rows[0]["blobId"].as_str() != Some(approved.blob_id.as_str())
    {
        return Err("Approved email changed or is unavailable".into());
    }
    let identity_ids = vec![approved.identity_id.clone()];
    let identities = client
        .get("Identity", Some(&identity_ids), None)
        .map_err(|_| "Sending identity unavailable")?;
    let identities = identities["list"]
        .as_array()
        .ok_or("Sending identity unavailable")?;
    if identities.len() != 1
        || identities[0]["id"].as_str() != Some(approved.identity_id.as_str())
        || identities[0]["email"].as_str() != Some(approved.mail_from.as_str())
    {
        return Err("Sending identity does not match approved sender".into());
    }
    let mailbox_ids = vec![
        approved.draft_mailbox_id.clone(),
        approved.sent_mailbox_id.clone(),
    ];
    let mailboxes = client
        .get("Mailbox", Some(&mailbox_ids), None)
        .map_err(|_| "Submission mailboxes unavailable")?;
    let mailboxes = mailboxes["list"]
        .as_array()
        .ok_or("Submission mailboxes unavailable")?;
    let drafts = mailboxes.iter().find(|mailbox| {
        mailbox["id"].as_str() == Some(approved.draft_mailbox_id.as_str())
            && mailbox["role"].as_str() == Some("drafts")
    });
    let sent = mailboxes.iter().find(|mailbox| {
        mailbox["id"].as_str() == Some(approved.sent_mailbox_id.as_str())
            && mailbox["role"].as_str() == Some("sent")
    });
    if drafts
        .and_then(|value| value.pointer("/myRights/mayRemoveItems"))
        .and_then(Value::as_bool)
        != Some(true)
        || sent
            .and_then(|value| value.pointer("/myRights/mayAddItems"))
            .and_then(Value::as_bool)
            != Some(true)
        || rows[0]["mailboxIds"]
            .get(approved.draft_mailbox_id.as_str())
            .and_then(Value::as_bool)
            != Some(true)
    {
        return Err("Approved draft or Sent mailbox changed or is not writable".into());
    }
    let empty: Vec<String> = Vec::new();
    let submission = client
        .get_submissions(Some(&empty))
        .map_err(|_| "Submission state unavailable")?;
    let submission_state = field(&submission, "state")?.to_string();
    let creation_id = format!("op_{:x}", Sha256::digest(approved.operation_id.as_bytes()));
    let op = ProviderSubmission {
        scope: scope.clone(),
        operation_id: approved.operation_id.clone(),
        draft_id: approved.email_id.clone(),
        identity_id: approved.identity_id.clone(),
        email_state: field(&email, "state")?.into(),
        snapshot_digest: digest,
        creation_id: creation_id.clone(),
        intent_json: intent,
        state: "unknown".into(),
        submission_id: None,
        sent_update: None,
    };
    if !store
        .claim_provider_submission(&op)
        .map_err(|e| e.to_string())?
    {
        return Ok(SendOutcome::Unknown);
    }
    match client.submit(
        &creation_id,
        &approved.email_id,
        &approved.identity_id,
        &submission_state,
        &approved.mail_from,
        &approved.recipients,
        &approved.draft_mailbox_id,
        &approved.sent_mailbox_id,
    ) {
        Ok(SubmissionResult::Created { id, email_update }) => {
            let sent_update = match email_update {
                EmailUpdateResult::Applied => "applied",
                EmailUpdateResult::Rejected => "rejected",
                EmailUpdateResult::Unknown => "unknown",
            };
            if !store
                .confirm_provider_submission(&scope, &approved.operation_id, &id, sent_update)
                .map_err(|e| e.to_string())?
            {
                return Ok(SendOutcome::Unknown);
            }
            Ok(SendOutcome::Submitted {
                id,
                sent_update: sent_update.into(),
            })
        }
        Ok(SubmissionResult::Rejected { .. }) => {
            if !store
                .reject_provider_submission(&scope, &approved.operation_id)
                .map_err(|e| e.to_string())?
            {
                return Ok(SendOutcome::Unknown);
            }
            Ok(SendOutcome::Rejected)
        }
        Err(_) => Ok(SendOutcome::Unknown),
    }
}
