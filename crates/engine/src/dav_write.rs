//! Durable, conditional DAV resource writes.
//!
//! These functions require an effect-bound approval from the caller. An
//! operation is recorded before dispatch and is never replayed after a crash or
//! a transport failure. A conflict leaves both the cache and intended bytes in
//! protected storage for explicit resolution.

use crate::provider_credential::ProviderCredentialStore;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use isyncyou_connectors::dav::{DavClient, DavError};
use isyncyou_core::{config::ProviderConfig, Config};
use isyncyou_store::{ProviderScope, Store};
use sha2::{Digest, Sha256};

/// The caller must bind approval to every field, including the exact bytes and
/// version, account, provider, owner/session and expiry.
pub struct ApprovedDavWrite {
    pub operation_id: String,
    pub account_id: String,
    pub origin: String,
    pub collection: String,
    pub service: String,
    pub href: String,
    /// `None` means create-if-absent. Existing objects require a strong ETag.
    pub expected_etag: Option<String>,
    /// Optional collection sync-token precondition for cross-resource effects.
    pub expected_collection_token: Option<String>,
    /// `None` means delete; otherwise the complete intended ICS/VCF payload.
    pub raw: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    Confirmed(String),
    Conflict,
    Rejected,
    Unknown,
}

/// The stage of this invocation before it enters a DAV write boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DavPreDispatchStage {
    Validation,
    Store,
    Credentials,
    Client,
    Discovery,
    Target,
    Privilege,
}

/// Dispatch certainty for this invocation only. A caller must independently
/// exclude another invocation of the same operation before claiming that no
/// concurrent writer dispatched it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DavWriteExecutionError {
    PreDispatch(DavPreDispatchStage),
    DispatchUncertain,
}

/// A confirmed change to one principal's visible collection ACL. The caller
/// binds approval to the complete displayed prior ACL and proposed privilege.
pub struct ApprovedDavAcl {
    pub operation_id: String,
    pub account_id: String,
    pub origin: String,
    pub collection: String,
    pub service: String,
    pub expected: Vec<isyncyou_connectors::dav::AclEntry>,
    pub principal: String,
    /// None revokes; read, write, or read-write replace its privileges.
    pub privilege: Option<String>,
}

/// Persist before attempting a lock or ACL write. Never repeat an uncertain
/// dispatch: callers must reconcile the native ACL and durable outcome.
pub fn execute_acl_with_store(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavAcl,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, String> {
    if !matches!(op.service.as_str(), "calendar" | "contacts")
        || op.operation_id.is_empty()
        || op.operation_id.len() > 256
        || op.expected.len() > 1024
        || !matches!(
            op.privilege.as_deref(),
            None | Some("read") | Some("write") | Some("read-write")
        )
    {
        return Err("Invalid DAV ACL intent".into());
    }
    let origin = match cfg.provider_accounts.get(&op.account_id) {
        Some(ProviderConfig::SelfHosted { origin, .. }) if origin == &op.origin => origin,
        _ => return Err("Account or provider changed after approval".into()),
    };
    let store = crate::provider_store::open(cfg, &op.account_id)?;
    let token = match credentials {
        Some(c) => {
            crate::provider_auth::self_hosted_access_token_with_store(cfg, &op.account_id, c)?
        }
        None => crate::provider_auth::self_hosted_access_token(cfg, &op.account_id)?,
    };
    let client = DavClient::new(origin, token).map_err(|e| e.to_string())?;
    if !client
        .discover(entry)
        .map_err(|e| e.to_string())?
        .iter()
        .any(|c| {
            c.url == op.collection
                && if op.service == "calendar" {
                    c.calendar
                } else {
                    c.addressbook
                }
        })
    {
        return Err("Collection is not currently accessible".into());
    }
    let scope = ProviderScope {
        account_id: op.account_id.clone(),
        origin: op.origin.clone(),
        service: op.service.clone(),
        collection: op.collection.clone(),
    };
    let expected: Vec<_> = op.expected.iter().map(|a| serde_json::json!({"principal":a.principal,"granted":a.granted,"privileges":a.privileges,"inherited":a.inherited,"protected":a.protected})).collect();
    let intent = serde_json::to_string(&serde_json::json!({"kind":"dav-acl","account":op.account_id,"origin":op.origin,"service":op.service,"collection":op.collection,"principal":op.principal,"privilege":op.privilege,"expected":expected})).map_err(|_| "Invalid ACL intent")?;
    if intent.len() > 1024 * 1024 {
        return Err("ACL intent too large".into());
    }
    let digest = format!("{:x}", Sha256::digest(intent.as_bytes()));
    if let Some(outcome) = recorded(&store, &scope, &op.operation_id, &digest)? {
        return Ok(outcome);
    }
    if !store
        .claim_provider_target_mutation(
            &scope,
            &op.operation_id,
            "dav-acl",
            &op.collection,
            &intent,
            &digest,
        )
        .map_err(|e| e.to_string())?
    {
        return recorded(&store, &scope, &op.operation_id, &digest)?
            .ok_or_else(|| "Collection has an unresolved mutation".into());
    }
    let result =
        client.update_acl_locked(&scope, &op.expected, &op.principal, op.privilege.as_deref());
    match result {
        Ok(()) => {
            if matches!(
                store.finish_provider_mutation(&scope, &op.operation_id, Some(&op.collection)),
                Ok(true)
            ) {
                Ok(WriteOutcome::Confirmed(op.collection.clone()))
            } else {
                Ok(WriteOutcome::Unknown)
            }
        }
        Err(DavError::Conflict) => {
            if matches!(
                store.conflict_provider_mutation(&scope, &op.operation_id),
                Ok(true)
            ) {
                Ok(WriteOutcome::Conflict)
            } else {
                Ok(WriteOutcome::Unknown)
            }
        }
        Err(DavError::Http(code)) if (400..500).contains(&code) && code != 408 && code != 429 => {
            if matches!(
                store.finish_provider_mutation(&scope, &op.operation_id, None),
                Ok(true)
            ) {
                Ok(WriteOutcome::Rejected)
            } else {
                Ok(WriteOutcome::Unknown)
            }
        }
        _ => Ok(WriteOutcome::Unknown),
    }
}

/// Read the durable outcome without dispatching again or requiring current DAV
/// access. `Unknown` includes interrupted dispatch and failed local finalization.
pub fn observe(
    cfg: &Config,
    account: &str,
    origin: &str,
    service: &str,
    collection: &str,
    operation_id: &str,
) -> Result<Option<WriteOutcome>, String> {
    match cfg.provider_accounts.get(account) {
        Some(ProviderConfig::SelfHosted {
            origin: configured, ..
        }) if configured == origin => {}
        _ => return Err("Account or provider changed".into()),
    }
    if !matches!(service, "calendar" | "contacts") || operation_id.is_empty() {
        return Err("Invalid DAV operation".into());
    }
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.into(),
        service: service.into(),
        collection: collection.into(),
    };
    let store = crate::provider_store::open(cfg, account)?;
    let Some((_, state, result)) = store
        .provider_mutation(&scope, operation_id)
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    Ok(Some(match state.as_str() {
        "confirmed" => {
            WriteOutcome::Confirmed(result.ok_or("Missing confirmed resource identity")?)
        }
        "conflict" => WriteOutcome::Conflict,
        "rejected" => WriteOutcome::Rejected,
        _ => WriteOutcome::Unknown,
    }))
}

trait WriteTransport {
    fn put(
        &self,
        scope: &ProviderScope,
        href: &str,
        raw: Vec<u8>,
        etag: Option<&str>,
        collection_token: Option<&str>,
    ) -> Result<Option<String>, DavError>;
    fn delete(&self, scope: &ProviderScope, href: &str, etag: &str) -> Result<(), DavError>;
}

impl WriteTransport for DavClient {
    fn put(
        &self,
        scope: &ProviderScope,
        href: &str,
        raw: Vec<u8>,
        etag: Option<&str>,
        collection_token: Option<&str>,
    ) -> Result<Option<String>, DavError> {
        DavClient::put_with_collection_precondition(self, scope, href, raw, etag, collection_token)
    }
    fn delete(&self, scope: &ProviderScope, href: &str, etag: &str) -> Result<(), DavError> {
        DavClient::delete(self, scope, href, etag)
    }
}

struct LockedTransport<'a> {
    client: &'a DavClient,
    token: &'a str,
}
impl WriteTransport for LockedTransport<'_> {
    fn put(
        &self,
        scope: &ProviderScope,
        href: &str,
        raw: Vec<u8>,
        etag: Option<&str>,
        _: Option<&str>,
    ) -> Result<Option<String>, DavError> {
        self.client.put_under_lock(
            scope,
            href,
            raw,
            etag.ok_or(DavError::InvalidResponse)?,
            self.token,
        )
    }
    fn delete(&self, scope: &ProviderScope, href: &str, etag: &str) -> Result<(), DavError> {
        self.client.delete_under_lock(scope, href, etag, self.token)
    }
}

fn recorded(
    store: &Store,
    scope: &ProviderScope,
    operation: &str,
    digest: &str,
) -> Result<Option<WriteOutcome>, String> {
    let Some((previous, state, result)) = store
        .provider_mutation(scope, operation)
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    if previous != digest {
        return Err("Operation does not match its recorded effect".into());
    }
    Ok(Some(match state.as_str() {
        "confirmed" => {
            WriteOutcome::Confirmed(result.ok_or("Missing confirmed resource identity")?)
        }
        "conflict" => WriteOutcome::Conflict,
        "rejected" => WriteOutcome::Rejected,
        _ => WriteOutcome::Unknown,
    }))
}

fn dispatch<T: WriteTransport>(
    store: &Store,
    transport: &T,
    scope: &ProviderScope,
    op: &ApprovedDavWrite,
    target: &str,
) -> Result<WriteOutcome, String> {
    if op.operation_id.is_empty()
        || op.operation_id.len() > 256
        || target.is_empty()
        || op.raw.as_ref().is_some_and(|v| v.len() > 16 * 1024 * 1024)
    {
        return Err("Invalid DAV write intent".into());
    }
    if let Some(tag) = &op.expected_etag {
        if tag.is_empty()
            || tag.starts_with("W/")
            || !tag.starts_with('"')
            || !tag.ends_with('"')
            || tag.contains('\r')
            || tag.contains('\n')
            || tag.len() < 2
            || tag[1..tag.len() - 1]
                .bytes()
                .any(|b| !(0x21..=0x7e).contains(&b) || b == b'"')
        {
            return Err("A strong resource ETag is required".into());
        }
    } else if op.raw.is_none() {
        return Err("Delete requires a strong resource ETag".into());
    }
    if let Some(token) = &op.expected_collection_token {
        if token.len() > 4096
            || reqwest::Url::parse(token).is_err()
            || token
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '<' | '>' | '(' | ')'))
        {
            return Err("Invalid collection precondition".into());
        }
    }
    let kind = if op.raw.is_some() {
        "dav-put"
    } else {
        "dav-delete"
    };
    let intent = serde_json::to_string(&serde_json::json!({"kind":kind,"account":scope.account_id,"origin":scope.origin,"service":scope.service,"collection":scope.collection,"href":target,"etag":op.expected_etag,"collection_token":op.expected_collection_token,"raw_base64":op.raw.as_ref().map(|v| STANDARD.encode(v))})).map_err(|_| "Invalid DAV intent")?;
    let digest = format!("{:x}", Sha256::digest(intent.as_bytes()));
    if let Some(outcome) = recorded(store, scope, &op.operation_id, &digest)? {
        return Ok(outcome);
    }
    if !store
        .claim_provider_target_mutation(scope, &op.operation_id, kind, target, &intent, &digest)
        .map_err(|e| e.to_string())?
    {
        return recorded(store, scope, &op.operation_id, &digest)?
            .ok_or_else(|| "Target has an unresolved mutation".into());
    }
    let result = match &op.raw {
        Some(raw) => transport
            .put(
                scope,
                target,
                raw.clone(),
                op.expected_etag.as_deref(),
                op.expected_collection_token.as_deref(),
            )
            .map(|_| ()),
        None => transport.delete(
            scope,
            target,
            op.expected_etag.as_deref().ok_or("Missing ETag")?,
        ),
    };
    match result {
        Ok(()) => {
            if matches!(
                store.finish_provider_mutation(scope, &op.operation_id, Some(target)),
                Ok(true)
            ) {
                Ok(WriteOutcome::Confirmed(target.into()))
            } else {
                Ok(WriteOutcome::Unknown)
            }
        }
        Err(DavError::Conflict) => {
            if matches!(
                store.conflict_provider_mutation(scope, &op.operation_id),
                Ok(true)
            ) {
                Ok(WriteOutcome::Conflict)
            } else {
                Ok(WriteOutcome::Unknown)
            }
        }
        Err(DavError::Http(code)) if (400..500).contains(&code) && code != 408 && code != 429 => {
            if matches!(
                store.finish_provider_mutation(scope, &op.operation_id, None),
                Ok(true)
            ) {
                Ok(WriteOutcome::Rejected)
            } else {
                Ok(WriteOutcome::Unknown)
            }
        }
        Err(_) => Ok(WriteOutcome::Unknown),
    }
}

/// Dispatch an already-approved effect after checking current configuration and
/// rediscovering the collection with the current account grant.
pub fn execute(cfg: &Config, entry: &str, op: &ApprovedDavWrite) -> Result<WriteOutcome, String> {
    execute_inner(cfg, entry, op, None)
}

/// Android or another native platform supplies its secure credential owner.
pub fn execute_with_store(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    credentials: &dyn ProviderCredentialStore,
) -> Result<WriteOutcome, String> {
    execute_inner(cfg, entry, op, Some(credentials))
}

/// Revalidate the authenticated attendee and exact preview before dispatching
/// an invitation response. Never use the generic raw PUT for this flow.
#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn execute_invitation_response(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    uid: &str,
    recurrence_id: Option<&str>,
    attendee: &str,
    response: &str,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, String> {
    if op.service != "calendar" || op.expected_collection_token.is_some() {
        return Err("Invalid invitation operation".into());
    }
    let prepared = crate::dav_live::prepare_invitation_response(
        cfg,
        &op.account_id,
        entry,
        &op.collection,
        &op.href,
        uid,
        recurrence_id,
        attendee,
        response,
        credentials,
    )?;
    if prepared.origin != op.origin
        || prepared.edit.href != op.href
        || Some(prepared.edit.expected_etag.as_str()) != op.expected_etag.as_deref()
        || Some(prepared.edit.raw.as_slice()) != op.raw.as_deref()
    {
        return Err("Invitation changed after approval".into());
    }
    execute_inner(cfg, entry, op, credentials)
}

/// Recheck the safe deletion preview against the exact approved source.
pub fn execute_delete_preview(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    uid: &str,
    source_sha256: &str,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, String> {
    if op.raw.is_some() || op.expected_collection_token.is_some() {
        return Err("Invalid delete operation".into());
    }
    let preview = crate::dav_live::prepare_delete_preview(
        cfg,
        &op.account_id,
        &op.collection,
        &op.service,
        &op.href,
    )?;
    if preview.origin != op.origin
        || preview.href != op.href
        || preview.uid != uid
        || preview.source_sha256 != source_sha256
        || Some(preview.expected_etag.as_str()) != op.expected_etag.as_deref()
    {
        return Err("Delete target changed after approval".into());
    }
    execute_inner(cfg, entry, op, credentials)
}

/// Dispatch exactly the typed, unscheduled create effect after confirmation.
pub fn execute_create_simple(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    preview: &crate::dav_live::PreparedCreatePreview,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, String> {
    if op.expected_etag.is_some()
        || op.expected_collection_token.is_some()
        || op.origin != preview.origin
        || op.collection != preview.collection
        || op.service != preview.service
        || op.href != preview.href
    {
        return Err("Create effect differs from approval".into());
    }
    let service = match preview.kind.as_str() {
        "event" | "task" => "calendar",
        "contact" => "contacts",
        _ => return Err("Invalid new resource kind".into()),
    };
    if service != op.service {
        return Err("Create service differs from approval".into());
    }
    let base = reqwest::Url::parse(&op.collection).map_err(|_| "Invalid collection URL")?;
    let expected_href = base
        .join(&format!(
            "{}.{}",
            preview.uid,
            if preview.kind == "contact" {
                "vcf"
            } else {
                "ics"
            }
        ))
        .map_err(|_| "Invalid resource URL")?
        .to_string();
    let raw = isyncyou_connectors::dav::edit::create_simple(
        &preview.kind,
        &preview.uid,
        &preview.timestamp,
        &preview.name,
        preview.start.as_deref(),
        preview.end.as_deref(),
    )?;
    if expected_href != op.href || raw != preview.raw || op.raw.as_deref() != Some(raw.as_slice()) {
        return Err("Create effect differs from approval".into());
    }
    execute_inner(cfg, entry, op, credentials)
}

pub fn execute_create_task_series(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    preview: &crate::dav_live::PreparedCreatePreview,
    rule: &str,
    occurrences: &[String],
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, String> {
    if op.expected_etag.is_some()
        || op.expected_collection_token.is_some()
        || preview.kind != "task"
        || op.origin != preview.origin
        || op.collection != preview.collection
        || op.service != "calendar"
        || preview.service != "calendar"
        || op.href != preview.href
    {
        return Err("Create effect differs from approval".into());
    }
    let base = reqwest::Url::parse(&op.collection).map_err(|_| "Invalid collection URL")?;
    let expected_href = base
        .join(&format!("{}.ics", preview.uid))
        .map_err(|_| "Invalid resource URL")?
        .to_string();
    let generated = isyncyou_connectors::dav::edit::create_task_series(
        &preview.uid,
        &preview.timestamp,
        &preview.name,
        preview.start.as_deref().ok_or("Task start missing")?,
        preview.end.as_deref().ok_or("Task due missing")?,
        rule,
    )?;
    if expected_href != op.href
        || generated.raw != preview.raw
        || op.raw.as_deref() != Some(generated.raw.as_slice())
        || generated.new_occurrences.as_slice() != occurrences
    {
        return Err("Create effect differs from approval".into());
    }
    execute_inner(cfg, entry, op, credentials)
}

pub fn execute_invitation_create(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    preview: &crate::dav_live::PreparedInvitationCreate,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, String> {
    let create = &preview.create;
    if op.expected_etag.is_some()
        || op.expected_collection_token.is_some()
        || op.origin != create.origin
        || op.collection != create.collection
        || op.service != "calendar"
        || create.service != "calendar"
        || create.kind != "event"
        || op.href != create.href
    {
        return Err("Invitation effect differs from approval".into());
    }
    if !matches!(cfg.provider_accounts.get(&op.account_id), Some(ProviderConfig::SelfHosted {origin,..}) if origin == &op.origin)
    {
        return Err("Account or provider changed after approval".into());
    }
    let start = create.start.as_deref().ok_or("Invitation start missing")?;
    let end = create.end.as_deref().ok_or("Invitation end missing")?;
    let expected = isyncyou_connectors::dav::edit::create_invitation(
        &create.uid,
        &create.timestamp,
        &create.name,
        start,
        end,
        &preview.organizer,
        &preview.attendees,
    )?;
    let base = reqwest::Url::parse(&op.collection).map_err(|_| "Invalid collection URL")?;
    let expected_href = base
        .join(&format!("{}.ics", create.uid))
        .map_err(|_| "Invalid resource URL")?
        .to_string();
    if expected_href != op.href
        || create.raw != expected
        || op.raw.as_deref() != Some(expected.as_slice())
    {
        return Err("Invitation effect differs from approval".into());
    }
    let token = match credentials {
        Some(c) => {
            crate::provider_auth::self_hosted_access_token_with_store(cfg, &op.account_id, c)?
        }
        None => crate::provider_auth::self_hosted_access_token(cfg, &op.account_id)?,
    };
    let client = DavClient::new(&op.origin, token).map_err(|e| e.to_string())?;
    if !client
        .calendar_user_addresses(entry)
        .map_err(|e| e.to_string())?
        .iter()
        .any(|address| address == &preview.organizer)
    {
        return Err("Organizer is no longer an authenticated calendar identity".into());
    }
    execute_inner(cfg, entry, op, credentials)
}

/// Rebuild a contact-field effect from the approved version and input.
pub fn execute_contact_field(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    key: &str,
    index: usize,
    value: &str,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, String> {
    if op.service != "contacts" || op.expected_collection_token.is_some() {
        return Err("Invalid contact operation".into());
    }
    let preview = crate::dav_live::prepare_contact_text_preview(
        cfg,
        &op.account_id,
        &op.collection,
        &op.href,
        key,
        index,
        value,
    )?;
    if preview.origin != op.origin
        || preview.edit.href != op.href
        || Some(preview.edit.expected_etag.as_str()) != op.expected_etag.as_deref()
        || Some(preview.edit.raw.as_slice()) != op.raw.as_deref()
    {
        return Err("Contact changed after approval".into());
    }
    execute_inner(cfg, entry, op, credentials)
}

pub fn execute_contact_add(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    key: &str,
    value: &str,
    value_type: &str,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, String> {
    if op.service != "contacts" || op.expected_collection_token.is_some() {
        return Err("Invalid contact operation".into());
    }
    let preview = crate::dav_live::prepare_contact_add_preview(
        cfg,
        &op.account_id,
        &op.collection,
        &op.href,
        key,
        value,
        value_type,
    )?;
    if preview.origin != op.origin
        || preview.edit.href != op.href
        || Some(preview.edit.expected_etag.as_str()) != op.expected_etag.as_deref()
        || Some(preview.edit.raw.as_slice()) != op.raw.as_deref()
    {
        return Err("Contact changed after approval".into());
    }
    execute_inner(cfg, entry, op, credentials)
}

pub fn execute_contact_remove(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    key: &str,
    index: usize,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, String> {
    if op.service != "contacts" || op.expected_collection_token.is_some() {
        return Err("Invalid contact operation".into());
    }
    let preview = crate::dav_live::prepare_contact_remove_preview(
        cfg,
        &op.account_id,
        &op.collection,
        &op.href,
        key,
        index,
    )?;
    if preview.origin != op.origin
        || preview.edit.href != op.href
        || Some(preview.edit.expected_etag.as_str()) != op.expected_etag.as_deref()
        || Some(preview.edit.raw.as_slice()) != op.raw.as_deref()
    {
        return Err("Contact changed after approval".into());
    }
    execute_inner(cfg, entry, op, credentials)
}

pub fn execute_contact_photo(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    media_type: &str,
    photo: &[u8],
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, String> {
    if op.service != "contacts" || op.expected_collection_token.is_some() {
        return Err("Invalid contact photo operation".into());
    }
    let preview = crate::dav_live::prepare_contact_photo(
        cfg,
        &op.account_id,
        &op.collection,
        &op.href,
        media_type,
        photo,
    )?;
    if preview.origin != op.origin
        || preview.edit.href != op.href
        || Some(preview.edit.expected_etag.as_str()) != op.expected_etag.as_deref()
        || Some(preview.edit.raw.as_slice()) != op.raw.as_deref()
    {
        return Err("Contact changed after approval".into());
    }
    execute_inner(cfg, entry, op, credentials)
}

pub fn execute_task_attributes(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    uid: &str,
    priority: Option<u8>,
    due: Option<(&str, bool)>,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, String> {
    execute_task_attributes_with_start(cfg, entry, op, uid, priority, due, None, credentials)
}

#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn execute_task_attributes_with_start(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    uid: &str,
    priority: Option<u8>,
    due: Option<(&str, bool)>,
    start: Option<&str>,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, String> {
    if op.service != "calendar" || op.expected_collection_token.is_some() {
        return Err("Invalid task operation".into());
    }
    let preview = crate::dav_live::prepare_task_attributes_with_start_preview(
        cfg,
        &op.account_id,
        &op.collection,
        &op.href,
        uid,
        priority,
        due,
        start,
    )?;
    if preview.origin != op.origin
        || preview.edit.href != op.href
        || Some(preview.edit.expected_etag.as_str()) != op.expected_etag.as_deref()
        || Some(preview.edit.raw.as_slice()) != op.raw.as_deref()
    {
        return Err("Task changed after approval".into());
    }
    execute_inner(cfg, entry, op, credentials)
}

#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn execute_task_attributes_with_start_classified(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    uid: &str,
    priority: Option<u8>,
    due: Option<(&str, bool)>,
    start: Option<&str>,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, DavWriteExecutionError> {
    // Until the original scope can be inspected, a previous invocation may
    // already have dispatched this operation ID under that scope.
    if op.service != "calendar" || op.expected_collection_token.is_some() {
        return Err(DavWriteExecutionError::DispatchUncertain);
    }
    if !matches!(cfg.provider_accounts.get(&op.account_id), Some(ProviderConfig::SelfHosted {origin,..}) if origin == &op.origin)
    {
        return Err(DavWriteExecutionError::DispatchUncertain);
    }
    let store = crate::provider_store::open(cfg, &op.account_id)
        .map_err(|_| DavWriteExecutionError::DispatchUncertain)?;
    let scope = ProviderScope {
        account_id: op.account_id.clone(),
        origin: op.origin.clone(),
        service: op.service.clone(),
        collection: op.collection.clone(),
    };
    if store
        .provider_mutation(&scope, &op.operation_id)
        .map_err(|_| DavWriteExecutionError::DispatchUncertain)?
        .is_some()
    {
        return Err(DavWriteExecutionError::DispatchUncertain);
    }
    let preview = crate::dav_live::prepare_task_attributes_with_start_preview(
        cfg,
        &op.account_id,
        &op.collection,
        &op.href,
        uid,
        priority,
        due,
        start,
    )
    .map_err(|_| DavWriteExecutionError::PreDispatch(DavPreDispatchStage::Validation))?;
    if preview.origin != op.origin
        || preview.edit.href != op.href
        || Some(preview.edit.expected_etag.as_str()) != op.expected_etag.as_deref()
        || Some(preview.edit.raw.as_slice()) != op.raw.as_deref()
    {
        return Err(DavWriteExecutionError::PreDispatch(
            DavPreDispatchStage::Validation,
        ));
    }
    execute_inner_classified(cfg, entry, op, credentials)
}

#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn execute_alarm(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    uid: &str,
    recurrence_id: Option<&str>,
    index: Option<usize>,
    trigger: Option<&str>,
    description: Option<&str>,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, String> {
    if op.service != "calendar" || op.expected_collection_token.is_some() {
        return Err("Invalid alarm operation".into());
    }
    let p = crate::dav_live::prepare_alarm_preview(
        cfg,
        &op.account_id,
        &op.collection,
        &op.href,
        uid,
        recurrence_id,
        index,
        trigger,
        description,
    )?;
    if p.origin != op.origin
        || p.edit.href != op.href
        || Some(p.edit.expected_etag.as_str()) != op.expected_etag.as_deref()
        || Some(p.edit.raw.as_slice()) != op.raw.as_deref()
    {
        return Err("Alarm changed after approval".into());
    }
    execute_inner(cfg, entry, op, credentials)
}

#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn execute_task_occurrence_completion(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    uid: &str,
    recurrence_id: &str,
    completed_at: Option<&str>,
    window_start: &str,
    window_end: &str,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, String> {
    if op.service != "calendar" || op.expected_collection_token.is_some() {
        return Err("Invalid task occurrence operation".into());
    }
    let p = crate::dav_live::prepare_task_occurrence_completion_preview(
        cfg,
        &op.account_id,
        entry,
        &op.collection,
        &op.href,
        uid,
        recurrence_id,
        completed_at,
        window_start,
        window_end,
        credentials,
    )?;
    if p.origin != op.origin
        || p.edit.href != op.href
        || Some(p.edit.expected_etag.as_str()) != op.expected_etag.as_deref()
        || Some(p.edit.raw.as_slice()) != op.raw.as_deref()
    {
        return Err("Task occurrence changed after approval".into());
    }
    execute_inner(cfg, entry, op, credentials)
}

#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn execute_zoned_time(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    uid: &str,
    start: &str,
    end: &str,
    tzid: &str,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, String> {
    if op.service != "calendar" || op.expected_collection_token.is_some() {
        return Err("Invalid event operation".into());
    }
    let preview = crate::dav_live::prepare_zoned_time_preview(
        cfg,
        &op.account_id,
        &op.collection,
        &op.href,
        uid,
        start,
        end,
        tzid,
    )?;
    if preview.origin != op.origin
        || preview.edit.href != op.href
        || Some(preview.edit.expected_etag.as_str()) != op.expected_etag.as_deref()
        || Some(preview.edit.raw.as_slice()) != op.raw.as_deref()
    {
        return Err("Event changed after approval".into());
    }
    execute_inner(cfg, entry, op, credentials)
}

pub fn execute_cancellation(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    uid: &str,
    recurrence_id: Option<&str>,
    sequence: u32,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, String> {
    if op.service != "calendar" || op.expected_collection_token.is_some() {
        return Err("Invalid cancellation operation".into());
    }
    let preview = crate::dav_live::prepare_cancellation_preview(
        cfg,
        &op.account_id,
        entry,
        &op.collection,
        &op.href,
        uid,
        recurrence_id,
        sequence,
        credentials,
    )?;
    if preview.origin != op.origin
        || preview.edit.href != op.href
        || Some(preview.edit.expected_etag.as_str()) != op.expected_etag.as_deref()
        || Some(preview.edit.raw.as_slice()) != op.raw.as_deref()
    {
        return Err("Cancellation changed after approval".into());
    }
    execute_inner(cfg, entry, op, credentials)
}

#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn execute_exception(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    uid: &str,
    recurrence_id: &str,
    start: &str,
    end: &str,
    window_start: &str,
    window_end: &str,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, String> {
    if op.service != "calendar" || op.expected_collection_token.is_some() {
        return Err("Invalid exception operation".into());
    }
    let preview = crate::dav_live::prepare_exception_preview(
        cfg,
        &op.account_id,
        entry,
        &op.collection,
        &op.href,
        uid,
        recurrence_id,
        start,
        end,
        window_start,
        window_end,
        credentials,
    )?;
    if preview.origin != op.origin
        || preview.edit.href != op.href
        || Some(preview.edit.expected_etag.as_str()) != op.expected_etag.as_deref()
        || Some(preview.edit.raw.as_slice()) != op.raw.as_deref()
    {
        return Err("Occurrence changed after approval".into());
    }
    execute_inner(cfg, entry, op, credentials)
}

pub fn execute_series_rule(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    uid: &str,
    rule: &str,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, String> {
    if op.service != "calendar" || op.expected_collection_token.is_some() {
        return Err("Invalid series operation".into());
    }
    let preview = crate::dav_live::prepare_series_rule_preview(
        cfg,
        &op.account_id,
        &op.collection,
        &op.href,
        uid,
        rule,
    )?;
    if preview.origin != op.origin
        || preview.edit.href != op.href
        || Some(preview.edit.expected_etag.as_str()) != op.expected_etag.as_deref()
        || Some(preview.edit.raw.as_slice()) != op.raw.as_deref()
    {
        return Err("Series changed after approval".into());
    }
    execute_inner(cfg, entry, op, credentials)
}

pub fn execute_task_parent(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    uid: &str,
    parent: Option<&str>,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, String> {
    if op.service != "calendar" || op.expected_collection_token.is_none() {
        return Err("Invalid task parent operation".into());
    }
    let preview = crate::dav_live::prepare_task_parent_preview(
        cfg,
        &op.account_id,
        &op.collection,
        &op.href,
        uid,
        parent,
    )?;
    if preview.origin != op.origin
        || preview.edit.href != op.href
        || Some(preview.edit.expected_etag.as_str()) != op.expected_etag.as_deref()
        || preview.edit.expected_collection_token != op.expected_collection_token
        || Some(preview.edit.raw.as_slice()) != op.raw.as_deref()
    {
        return Err("Task graph changed after approval".into());
    }
    execute_inner(cfg, entry, op, credentials)
}

pub fn execute_task_delete(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    uid: &str,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, String> {
    if op.service != "calendar" || op.raw.is_some() {
        return Err("Invalid task deletion".into());
    }
    let preview = crate::dav_live::prepare_task_delete_preview(
        cfg,
        &op.account_id,
        &op.collection,
        &op.href,
        uid,
    )?;
    if preview.origin != op.origin
        || preview.href != op.href
        || Some(preview.expected_etag.as_str()) != op.expected_etag.as_deref()
        || Some(preview.expected_collection_token.as_str())
            != op.expected_collection_token.as_deref()
    {
        return Err("Task graph changed after approval".into());
    }
    execute_inner(cfg, entry, op, credentials)
}

fn execute_inner(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, String> {
    execute_inner_impl(
        cfg,
        entry,
        op,
        credentials,
        &std::cell::Cell::new(false),
        &std::cell::Cell::new(DavPreDispatchStage::Validation),
        false,
    )
}

fn execute_inner_classified(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, DavWriteExecutionError> {
    let entered = std::cell::Cell::new(false);
    let stage = std::cell::Cell::new(DavPreDispatchStage::Validation);
    let result = execute_inner_impl(cfg, entry, op, credentials, &entered, &stage, true);
    classify_execution(result, entered.get(), stage.get())
}

fn classify_execution(
    result: Result<WriteOutcome, String>,
    entered: bool,
    stage: DavPreDispatchStage,
) -> Result<WriteOutcome, DavWriteExecutionError> {
    result.map_err(|_| {
        if entered {
            DavWriteExecutionError::DispatchUncertain
        } else {
            DavWriteExecutionError::PreDispatch(stage)
        }
    })
}

fn discover_write_collection(
    discover: impl FnOnce() -> Result<Vec<isyncyou_connectors::dav::Collection>, DavError>,
    collection: &str,
    service: &str,
) -> Result<isyncyou_connectors::dav::Collection, String> {
    discover()
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|c| {
            c.url == collection
                && if service == "calendar" {
                    c.calendar
                } else {
                    c.addressbook
                }
        })
        .ok_or_else(|| "Collection is not currently accessible".into())
}

fn execute_inner_impl(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    credentials: Option<&dyn ProviderCredentialStore>,
    entered: &std::cell::Cell<bool>,
    stage: &std::cell::Cell<DavPreDispatchStage>,
    verify_prior: bool,
) -> Result<WriteOutcome, String> {
    #[cfg(debug_assertions)]
    let perf_start = std::time::Instant::now();
    if !matches!(op.service.as_str(), "calendar" | "contacts") {
        return Err("Unsupported DAV service".into());
    }
    let origin = match cfg.provider_accounts.get(&op.account_id) {
        Some(ProviderConfig::SelfHosted { origin, .. }) if origin == &op.origin => origin,
        _ => return Err("Account or provider changed after approval".into()),
    };
    stage.set(DavPreDispatchStage::Store);
    if verify_prior {
        entered.set(true);
    }
    let store = crate::provider_store::open(cfg, &op.account_id)?;
    // A previous invocation with this operation ID may already have dispatched.
    // Failure to inspect it must never be reported as proof of no dispatch.
    if verify_prior {
        let prior_scope = ProviderScope {
            account_id: op.account_id.clone(),
            origin: op.origin.clone(),
            service: op.service.clone(),
            collection: op.collection.clone(),
        };
        if store
            .provider_mutation(&prior_scope, &op.operation_id)
            .map_err(|e| e.to_string())?
            .is_none()
        {
            entered.set(false);
        }
    }
    #[cfg(debug_assertions)]
    eprintln!(
        "isyncyou: dav_confirm store_open_ms={}",
        perf_start.elapsed().as_millis()
    );
    #[cfg(debug_assertions)]
    let auth_start = std::time::Instant::now();
    stage.set(DavPreDispatchStage::Credentials);
    let token = match credentials {
        Some(credentials) => crate::provider_auth::self_hosted_access_token_with_store(
            cfg,
            &op.account_id,
            credentials,
        )?,
        None => crate::provider_auth::self_hosted_access_token(cfg, &op.account_id)?,
    };
    stage.set(DavPreDispatchStage::Client);
    let client = DavClient::new(origin, token).map_err(|e| e.to_string())?;
    #[cfg(debug_assertions)]
    eprintln!(
        "isyncyou: dav_confirm token_client_ms={}",
        auth_start.elapsed().as_millis()
    );
    #[cfg(debug_assertions)]
    let discovery_start = std::time::Instant::now();
    stage.set(DavPreDispatchStage::Discovery);
    let collection =
        discover_write_collection(|| client.discover(entry), &op.collection, &op.service)?;
    #[cfg(debug_assertions)]
    eprintln!(
        "isyncyou: dav_confirm discovery_ms={}",
        discovery_start.elapsed().as_millis()
    );
    let scope = ProviderScope {
        account_id: op.account_id.clone(),
        origin: origin.clone(),
        service: op.service.clone(),
        collection: collection.url,
    };
    stage.set(DavPreDispatchStage::Target);
    let target = client
        .validate_target(&scope, &op.href)
        .map_err(|e| e.to_string())?;
    let mut parent_change = None;
    let mut task_delete = None;
    if scope.service == "calendar" {
        if op.raw.is_none() {
            let previous = store
                .provider_object(&scope, &target)
                .map_err(|e| e.to_string())?
                .ok_or("Cached calendar resource missing")?;
            if let isyncyou_connectors::dav::edit::ResourceIndex::Task(task) =
                isyncyou_connectors::dav::edit::resource_index(&previous.raw)?
            {
                let (_, _, etag, token) = crate::dav_live::prepare_task_delete_in_store(
                    &store, &scope, &target, &task.uid,
                )?;
                if Some(etag.as_str()) != op.expected_etag.as_deref()
                    || Some(token.as_str()) != op.expected_collection_token.as_deref()
                {
                    return Err("Task graph changed after approval".into());
                }
                task_delete = Some(task.uid);
            }
        }
        if let Some(raw) = &op.raw {
            use isyncyou_connectors::dav::edit::{resource_index, ResourceIndex};
            let proposed = resource_index(raw)?;
            if let ResourceIndex::Event { uid } = &proposed {
                if op.expected_etag.is_some() {
                    let previous = store
                        .provider_object(&scope, &target)
                        .map_err(|e| e.to_string())?
                        .ok_or("Cached event resource missing")?;
                    if !matches!(resource_index(&previous.raw)?, ResourceIndex::Event {uid: old} if old == *uid)
                    {
                        return Err("Event UID or resource type changed".into());
                    }
                }
            }
            if let ResourceIndex::Task(task) = proposed {
                match &op.expected_etag {
                    None if task.parent_uid.is_some() => {
                        return Err(
                            "Creating a task with parentage requires a validated graph operation"
                                .into(),
                        )
                    }
                    Some(_) => {
                        let previous = store
                            .provider_object(&scope, &target)
                            .map_err(|e| e.to_string())?
                            .ok_or("Cached task resource missing")?;
                        let ResourceIndex::Task(old) = resource_index(&previous.raw)? else {
                            return Err("Resource type changed".into());
                        };
                        if old.uid != task.uid {
                            return Err("Task UID cannot change during an edit".into());
                        }
                        if old.parent_uid != task.parent_uid {
                            let prepared = crate::dav_live::prepare_task_parent_in_store(
                                &store,
                                &scope,
                                &target,
                                &task.uid,
                                task.parent_uid.as_deref(),
                            )?;
                            if prepared.raw != *raw
                                || op.expected_collection_token
                                    != prepared.expected_collection_token
                                || op.expected_etag.as_deref()
                                    != Some(prepared.expected_etag.as_str())
                            {
                                return Err(
                                    "Task parent effect no longer matches its validated graph"
                                        .into(),
                                );
                            }
                            parent_change = Some((task.uid, task.parent_uid));
                        }
                    }
                    None => {}
                }
            }
        }
    }
    let (resource, privilege) = match (&op.raw, &op.expected_etag) {
        (Some(_), None) => (None, "bind"),
        (Some(_), Some(_)) => (Some(target.as_str()), "write-content"),
        (None, _) => (None, "unbind"),
    };
    #[cfg(debug_assertions)]
    let privilege_start = std::time::Instant::now();
    stage.set(DavPreDispatchStage::Privilege);
    if !client
        .permits(&scope, resource, privilege)
        .map_err(|e| e.to_string())?
    {
        return Err("DAV privilege no longer permits this effect".into());
    }
    #[cfg(debug_assertions)]
    eprintln!(
        "isyncyou: dav_confirm privilege_ms={}",
        privilege_start.elapsed().as_millis()
    );
    if let Some((uid, parent)) = parent_change {
        entered.set(true);
        return client
            .with_locked_collection(&scope, |token| {
                client.sync_collection(&store, &scope)?;
                let current = crate::dav_live::prepare_task_parent_in_store(
                    &store,
                    &scope,
                    &target,
                    &uid,
                    parent.as_deref(),
                )
                .map_err(|_| DavError::Conflict)?;
                if current.raw != *op.raw.as_ref().ok_or(DavError::InvalidResponse)?
                    || current.expected_collection_token != op.expected_collection_token
                    || Some(current.expected_etag.as_str()) != op.expected_etag.as_deref()
                {
                    return Err(DavError::Conflict);
                }
                dispatch(
                    &store,
                    &LockedTransport {
                        client: &client,
                        token,
                    },
                    &scope,
                    op,
                    &target,
                )
                .map_err(DavError::Store)
            })
            .or_else(|error| match error {
                DavError::Conflict => Ok(WriteOutcome::Conflict),
                other => Err(other.to_string()),
            });
    }
    if let Some(uid) = task_delete {
        entered.set(true);
        return client
            .with_locked_collection(&scope, |token| {
                client.sync_collection(&store, &scope)?;
                let (_, _, etag, checkpoint) =
                    crate::dav_live::prepare_task_delete_in_store(&store, &scope, &target, &uid)
                        .map_err(|_| DavError::Conflict)?;
                if Some(etag.as_str()) != op.expected_etag.as_deref()
                    || Some(checkpoint.as_str()) != op.expected_collection_token.as_deref()
                {
                    return Err(DavError::Conflict);
                }
                dispatch(
                    &store,
                    &LockedTransport {
                        client: &client,
                        token,
                    },
                    &scope,
                    op,
                    &target,
                )
                .map_err(DavError::Store)
            })
            .or_else(|error| match error {
                DavError::Conflict => Ok(WriteOutcome::Conflict),
                other => Err(other.to_string()),
            });
    }
    if op.expected_collection_token.is_some() {
        return Err(
            "Collection checkpoint is supported only for locked task parent changes".into(),
        );
    }
    #[cfg(debug_assertions)]
    let dispatch_start = std::time::Instant::now();
    entered.set(true);
    let result = dispatch(&store, &client, &scope, op, &target);
    #[cfg(debug_assertions)]
    eprintln!(
        "isyncyou: dav_confirm dispatch_ms={} total_ms={}",
        dispatch_start.elapsed().as_millis(),
        perf_start.elapsed().as_millis()
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Fake {
        calls: Cell<usize>,
        conflict: bool,
    }
    impl WriteTransport for Fake {
        fn put(
            &self,
            _: &ProviderScope,
            _: &str,
            _: Vec<u8>,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<Option<String>, DavError> {
            self.calls.set(self.calls.get() + 1);
            if self.conflict {
                Err(DavError::Conflict)
            } else {
                Err(DavError::Transport("uncertain".into()))
            }
        }
        fn delete(&self, _: &ProviderScope, _: &str, _: &str) -> Result<(), DavError> {
            panic!("unexpected delete")
        }
    }
    fn fixture() -> (ProviderScope, ApprovedDavWrite) {
        let scope = ProviderScope {
            account_id: "account".into(),
            origin: "https://example.test".into(),
            service: "calendar".into(),
            collection: "https://example.test/cal/".into(),
        };
        let op = ApprovedDavWrite {
            operation_id: "op-1".into(),
            account_id: scope.account_id.clone(),
            origin: scope.origin.clone(),
            collection: scope.collection.clone(),
            service: scope.service.clone(),
            href: "a.ics".into(),
            expected_etag: Some("\"v1\"".into()),
            expected_collection_token: None,
            raw: Some(b"original bytes".to_vec()),
        };
        (scope, op)
    }

    #[test]
    fn discovery_failure_is_predispatch_but_dispatch_failure_is_unknown() {
        let private_error = "provider detail must not escape".to_string();
        assert_eq!(
            classify_execution(
                Err(private_error.clone()),
                false,
                DavPreDispatchStage::Discovery
            ),
            Err(DavWriteExecutionError::PreDispatch(
                DavPreDispatchStage::Discovery
            ))
        );
        assert_eq!(
            classify_execution(Err(private_error), true, DavPreDispatchStage::Discovery),
            Err(DavWriteExecutionError::DispatchUncertain)
        );
        assert_eq!(
            classify_execution(
                Ok(WriteOutcome::Unknown),
                true,
                DavPreDispatchStage::Privilege
            ),
            Ok(WriteOutcome::Unknown)
        );
        let (_, op) = fixture();
        assert_eq!(
            execute_task_attributes_with_start_classified(
                &Config::default(),
                "/.well-known/caldav",
                &op,
                "uid",
                None,
                None,
                None,
                None
            ),
            Err(DavWriteExecutionError::DispatchUncertain)
        );
    }

    #[test]
    fn discovery_http_failure_stops_before_any_write_dispatch() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let n = stream.read(&mut buffer).unwrap();
                assert_ne!(n, 0);
                request.extend_from_slice(&buffer[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            assert!(request.starts_with(b"PROPFIND "));
            stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let client =
            isyncyou_connectors::dav::LoopbackDavReader::new(&origin, "fixture".into()).unwrap();
        let writes = Cell::new(0usize);
        let result = discover_write_collection(
            || client.discover("/.well-known/caldav"),
            &format!("{origin}cal/"),
            "calendar",
        )
        .map(|_| {
            writes.set(writes.get() + 1);
            WriteOutcome::Unknown
        });
        assert_eq!(
            classify_execution(result, false, DavPreDispatchStage::Discovery),
            Err(DavWriteExecutionError::PreDispatch(
                DavPreDispatchStage::Discovery
            ))
        );
        assert_eq!(writes.get(), 0);
        server.join().unwrap();
    }

    #[test]
    fn unknown_is_persisted_without_resend_and_intent_cannot_change() {
        let store = Store::open_in_memory().unwrap();
        let (scope, mut op) = fixture();
        let fake = Fake {
            calls: Cell::new(0),
            conflict: false,
        };
        let target = "https://example.test/cal/a.ics";
        assert_eq!(
            dispatch(&store, &fake, &scope, &op, target).unwrap(),
            WriteOutcome::Unknown
        );
        assert_eq!(
            dispatch(&store, &fake, &scope, &op, target).unwrap(),
            WriteOutcome::Unknown
        );
        assert_eq!(fake.calls.get(), 1);
        let mut second = fixture().1;
        second.operation_id = "op-2".into();
        assert!(dispatch(&store, &fake, &scope, &second, target).is_err());
        assert_eq!(fake.calls.get(), 1);
        let (_, intent, _, state, _) = store
            .provider_mutation_intent(&scope, &op.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(state, "unknown");
        assert!(intent.contains(&STANDARD.encode(b"original bytes")));
        op.raw = Some(b"different".to_vec());
        assert!(dispatch(&store, &fake, &scope, &op, target).is_err());
        assert_eq!(fake.calls.get(), 1);
    }

    #[test]
    fn conflict_keeps_intended_bytes_and_never_retries() {
        let store = Store::open_in_memory().unwrap();
        let (scope, op) = fixture();
        let fake = Fake {
            calls: Cell::new(0),
            conflict: true,
        };
        let target = "https://example.test/cal/a.ics";
        assert_eq!(
            dispatch(&store, &fake, &scope, &op, target).unwrap(),
            WriteOutcome::Conflict
        );
        assert_eq!(
            dispatch(&store, &fake, &scope, &op, target).unwrap(),
            WriteOutcome::Conflict
        );
        assert_eq!(fake.calls.get(), 1);
        assert_eq!(
            store
                .provider_mutation_intent(&scope, &op.operation_id)
                .unwrap()
                .unwrap()
                .3,
            "conflict"
        );
    }
}
