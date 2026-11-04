//! JMAP mail synchronization into the provider-scoped store.

use isyncyou_connectors::jmap::{EmailUpdateResult, JmapClient, JmapError, SubmissionResult};
use isyncyou_core::{config::ProviderConfig, Config};
use isyncyou_store::{ProviderChange, ProviderObject, ProviderScope, ProviderSubmission, Store};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::Value;
use sha2::{Digest, Sha256};

struct StageGuard<'a> {
    store: &'a Store,
    client: &'a JmapClient,
    scope: &'a ProviderScope,
    generation: String,
    finished: bool,
}
impl<'a> StageGuard<'a> {
    fn new(store: &'a Store, scope: &'a ProviderScope, client: &'a JmapClient) -> Result<Self, String> {
        let mut bytes = [0u8; 32];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| "Staging random source unavailable")?;
        let generation = format!("{:x}", Sha256::digest(bytes));
        store
            .begin_provider_staging(scope, &generation)
            .map_err(|e| { client.record_store_failure(); e.to_string() })?;
        Ok(Self {
            store,
            client,
            scope,
            generation,
            finished: false,
        })
    }
    fn page(&self, objects: &[ProviderObject]) -> Result<(), String> {
        self.store
            .stage_provider_objects(self.scope, &self.generation, objects)
            .map_err(|e| { self.client.record_store_failure(); e.to_string() })
    }
    fn finish(mut self, cursor: &str) -> Result<(), String> {
        self.store
            .finish_provider_staging(self.scope, &self.generation, cursor)
            .map_err(|e| { self.client.record_store_failure(); e.to_string() })?;
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

/// Last live JMAP discovery outcome; independent of locally stored credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MailAccessObservation { Available, AuthenticationRejected, PermissionDenied, TransportUnavailable, InvalidResponse, Unknown }

fn access_observations() -> &'static std::sync::Mutex<std::collections::HashMap<(String,String),MailAccessObservation>> {
    static OBS:std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<(String,String),MailAccessObservation>>> = std::sync::OnceLock::new();
    OBS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Observation is local to this process and never proves current access without a new request.
pub fn mail_access_observation(cfg:&Config, account:&str) -> MailAccessObservation {
    let Some(ProviderConfig::SelfHosted {origin,..})=cfg.provider_accounts.get(account) else { return MailAccessObservation::Unknown; };
    access_observations().lock().unwrap_or_else(|e| e.into_inner()).get(&(account.into(),origin.clone())).copied().unwrap_or(MailAccessObservation::Unknown)
}

fn observe_discovery(account:&str, origin:&str, result:&Result<JmapClient,JmapError>) {
    let status=match result { Ok(_) => MailAccessObservation::Available, Err(JmapError::Http(401)) => MailAccessObservation::AuthenticationRejected, Err(JmapError::Http(403)) => MailAccessObservation::PermissionDenied, Err(JmapError::Transport) => MailAccessObservation::TransportUnavailable, Err(JmapError::InvalidUrl | JmapError::InvalidResponse | JmapError::Unsupported) => MailAccessObservation::InvalidResponse, Err(_) => MailAccessObservation::Unknown };
    let mut map=access_observations().lock().unwrap_or_else(|e| e.into_inner());
    if map.len()>=256 && !map.contains_key(&(account.into(),origin.into())) { map.clear(); }
    map.insert((account.into(),origin.into()),status);
}
fn observe_unknown(account:&str, origin:&str) {
    let mut map=access_observations().lock().unwrap_or_else(|e| e.into_inner());
    if map.len()>=256 && !map.contains_key(&(account.into(),origin.into())) { map.clear(); }
    map.insert((account.into(),origin.into()),MailAccessObservation::Unknown);
}
fn discovery_failure(error:JmapError) -> String {
    match error {
        JmapError::InvalidUrl => "JMAP discovery invalid_url".into(),
        JmapError::InvalidResponse => "JMAP discovery invalid_response".into(),
        JmapError::Unsupported => "JMAP discovery unsupported".into(),
        JmapError::Http(code) => format!("JMAP discovery http_{code}"),
        JmapError::Transport => "JMAP discovery transport".into(),
        JmapError::OutcomeUnknown => "JMAP discovery outcome_unknown".into(),
        JmapError::Method(_) => "JMAP discovery method_error".into(),
    }
}

pub fn client(cfg: &Config, account: &str) -> Result<SelfHostedClient, String> {
    let origin = match cfg.provider_accounts.get(account) {
        Some(ProviderConfig::SelfHosted { origin, .. }) => origin,
        None => return Err("Not a self-hosted account".into()),
    };
    let token = crate::provider_auth::self_hosted_access_token(cfg, account).map_err(|e| { observe_unknown(account,origin); e })?;
    let discovery = JmapClient::discover(origin, token);
    observe_discovery(account,origin,&discovery);
    let client = discovery.map_err(discovery_failure)?;
    Ok(SelfHostedClient {
        client,
        account: account.into(),
        origin: origin.clone(),
    })
}

/// Open one account-bound JMAP event stream. Each connection calls `on_change`
/// before reading hints so callers reconcile persisted cursors after reconnect.
pub fn watch_mail_events(cfg: &Config, account: &str, stop: &std::sync::atomic::AtomicBool, mut on_change: impl FnMut(isyncyou_connectors::jmap::EventDomains) -> Result<(), ()>) -> Result<(), String> {
    let _lease = crate::provider_auth::try_event_stream_lease(cfg,account)?;
    let origin = match cfg.provider_accounts.get(account) { Some(ProviderConfig::SelfHosted {origin,..}) => origin, None => return Err("Not a self-hosted account".into()) };
    let token = crate::provider_auth::self_hosted_access_token(cfg, account)?;
    let discovery = JmapClient::discover(origin, token.clone());
    observe_discovery(account,origin,&discovery);
    let native = discovery.map_err(|_| "JMAP discovery failed")?;
    let provider = SelfHostedClient {client:native,account:account.into(),origin:origin.clone()};
    scope(cfg, account, &provider)?;
    let mut last_auth_check: Option<std::time::Instant> = None;
    provider.client.watch_mail_events(stop, |domains| {
        if stop.load(std::sync::atomic::Ordering::Relaxed) { return Err(()); }
        if last_auth_check.is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(1)) {
            let current = crate::provider_auth::self_hosted_access_token(cfg,account).map_err(|_| ())?;
            if current != token { return Err(()); }
            last_auth_check = Some(std::time::Instant::now());
        }
        on_change(domains)
    }).map_err(|_| "JMAP event stream unavailable".into())
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
    let token = crate::provider_auth::self_hosted_access_token_with_store(cfg, account, credentials).map_err(|e| { observe_unknown(account,origin); e })?;
    let discovery = JmapClient::discover(origin, token);
    observe_discovery(account,origin,&discovery);
    let client = discovery.map_err(discovery_failure)?;
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
        service: if client.client.account_id() == client.client.primary_account_id() { "mail" } else { "mail-shared" }.into(),
        collection: client.client.account_id().into(),
    })
}

/// Shared mail accounts advertised by a freshly discovered authenticated session.
pub fn shared_mail_accessible_accounts(cfg:&Config, account:&str, provider:&SelfHostedClient) -> Result<Vec<String>,String> {
    scope(cfg,account,provider)?;
    if provider.client.account_id()!=provider.client.primary_account_id() { return Err("Primary mail client required".into()); }
    Ok(provider.client.mail_accounts().iter().filter_map(|(id,_)| (id!=provider.client.primary_account_id()).then_some(id.clone())).collect())
}

/// Live readable mailbox IDs for one freshly discovered shared account.
pub fn shared_mail_readable_mailboxes(cfg:&Config, account:&str, provider:&SelfHostedClient, native:&str) -> Result<Vec<String>,String> {
    if !shared_mail_accessible_accounts(cfg,account,provider)?.iter().any(|id| id==native) { return Err("Shared mail scope unavailable".into()); }
    let client=provider.client.for_mail_account(native).map_err(|_| "Shared mail scope unavailable")?;
    let page_size=client.max_objects_in_get().min(100);
    if page_size==0 { return Err("Mailbox rights unavailable".into()); }
    let mut position=0usize; let mut state:Option<String>=None; let mut readable=Vec::new(); let mut seen=std::collections::HashSet::new();
    loop {
        let page=client.query_mailboxes(position,page_size).map_err(|_| "Mailbox rights unavailable")?;
        let current=field(&page,"queryState")?;
        if state.as_deref().is_some_and(|s| s!=current) { return Err("Mailbox rights changed".into()); }
        state=Some(current.into());
        if page["position"].as_u64()!=Some(position as u64) { return Err("Invalid mailbox page".into()); }
        let ids=list(&page,"ids")?;
        let total_u64=page["total"].as_u64().ok_or("Invalid mailbox page")?;
        if total_u64>1000 { return Err("Invalid mailbox page".into()); }
        let total=total_u64 as usize;
        if position+ids.len()>total || (ids.is_empty() && position!=total) || ids.iter().any(|id| !seen.insert(id.clone())) { return Err("Invalid mailbox page".into()); }
        if !ids.is_empty() {
            let result=client.get("Mailbox",Some(&ids),None).map_err(|_| "Mailbox rights unavailable")?;
            if !list(&result,"notFound")?.is_empty() { return Err("Mailbox rights changed".into()); }
            let rows=result["list"].as_array().ok_or("Mailbox rights unavailable")?;
            let mut found=std::collections::HashSet::new();
            for row in rows {
                let id=field(row,"id")?;
                if !ids.iter().any(|value| value==id) || !found.insert(id.to_owned()) { return Err("Invalid mailbox identity".into()); }
                if row["myRights"]["mayReadItems"].as_bool()==Some(true) { readable.push(id.to_owned()); }
            }
            if found.len()!=ids.len() { return Err("Mailbox rights changed".into()); }
        }
        position+=ids.len(); if position==total { break; }
    }
    Ok(readable)
}

/// Read one export-compatible version while the caller holds the provider account lock.
/// The token must belong to that lock; this function never acquires it again.
pub(crate) fn selected_capture_with_token(cfg: &Config, account: &str, email_id: &str, token: &str) -> Result<(ProviderScope, ProviderObject, String), String> {
    if email_id.is_empty() || email_id.len() > 4096 || email_id.chars().any(char::is_control) || token.is_empty() { return Err("Invalid capture identity".into()); }
    let origin = match cfg.provider_accounts.get(account) { Some(ProviderConfig::SelfHosted { origin, .. }) => origin.clone(), None => return Err("Not a self-hosted account".into()) };
    let native = JmapClient::discover(&origin, token.to_owned()).map_err(|_| "JMAP discovery failed")?;
    let provider = SelfHostedClient { client:native, account:account.into(), origin };
    let scope = scope(cfg, account, &provider)?;
    let ids = vec![email_id.to_owned()];
    let read = || -> Result<(String, Value), String> {
        let result = provider.client.get("Email", Some(&ids), None).map_err(|_| "Mail capture unavailable")?;
        if !list(&result, "notFound")?.is_empty() { return Err("Mail capture unavailable".into()); }
        let values = result.get("list").and_then(Value::as_array).ok_or("Invalid mail capture")?;
        if values.len() != 1 || field(&values[0], "id")? != email_id { return Err("Invalid mail capture identity".into()); }
        Ok((field(&result, "state")?.to_owned(), values[0].clone()))
    };
    let (state, item) = read()?;
    let blob = field(&item, "blobId")?;
    let mime = provider.client.download(blob, "message.eml", "message/rfc822").map_err(|_| "Mail capture MIME unavailable")?;
    let digest = format!("{:x}", Sha256::digest(&mime));
    let (after_state, after_item) = read()?;
    if state != after_state || item != after_item { return Err("Mail changed during capture".into()); }
    let object = isyncyou_store::mail_capture_object(&item, &digest, mime.len() as u64).map_err(str::to_owned)?;
    let version = isyncyou_store::capture_version(object.etag.as_deref(), &object.raw);
    Ok((scope, object, version))
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

fn readable_scoped_client(cfg: &Config, account: &str, provider: &SelfHostedClient, native: &str, email_id: &str) -> Result<SelfHostedClient, String> {
    scope(cfg, account, provider)?;
    let selected = provider.client.for_mail_account(native).map_err(|_| "Mail scope unavailable")?;
    let scoped = SelfHostedClient { client:selected, account:account.into(), origin:provider.origin.clone() };
    if native != provider.client.primary_account_id() {
        let ids = vec![email_id.to_owned()];
        let email = scoped.client.get("Email", Some(&ids), None).map_err(|_| "Mail unavailable")?;
        if !list(&email,"notFound")?.is_empty() { return Err("Mail unavailable".into()); }
        let entries = email["list"].as_array().ok_or("Invalid mail response")?;
        if entries.len()!=1 || field(&entries[0],"id")? != email_id { return Err("Mail unavailable".into()); }
        let mailboxes = entries[0]["mailboxIds"].as_object().ok_or("Mail membership unavailable")?;
        let ids:Vec<String> = mailboxes.iter().filter_map(|(id,v)| (v.as_bool()==Some(true)).then_some(id.clone())).collect();
        if ids.is_empty() || ids.len()>256 { return Err("Mail membership unavailable".into()); }
        let response = scoped.client.get("Mailbox",Some(&ids),None).map_err(|_| "Mailbox rights unavailable")?;
        let entries = response["list"].as_array().ok_or("Mailbox rights unavailable")?;
        let readable = entries.iter().any(|m| m["id"].as_str().is_some_and(|id| ids.iter().any(|x| x==id)) && m["myRights"]["mayReadItems"].as_bool()==Some(true));
        if !readable { return Err("Mailbox access unavailable".into()); }
    }
    Ok(scoped)
}

/// Read from an explicitly selected account advertised by the authenticated session.
pub fn read_body_in_scope(cfg:&Config, account:&str, provider:&SelfHostedClient, native:&str, email_id:&str, max_bytes:usize) -> Result<Value,String> {
    let selected = readable_scoped_client(cfg,account,provider,native,email_id)?;
    let mut result=read_body(cfg,account,&selected,email_id,max_bytes)?;
    if native != provider.client.primary_account_id() {
        let allowed=shared_mail_readable_mailboxes(cfg,account,provider,native)?;
        retain_readable_memberships(&mut result,email_id,&allowed)?;
    }
    Ok(result)
}

fn retain_readable_memberships(result:&mut Value, email_id:&str, allowed:&[String]) -> Result<(),String> {
    let rows=result["list"].as_array_mut().ok_or("Invalid mail response")?;
    if rows.len()!=1 || field(&rows[0],"id")? != email_id { return Err("Mail unavailable".into()); }
    let memberships=rows[0]["mailboxIds"].as_object_mut().ok_or("Mail membership unavailable")?;
    memberships.retain(|id,value| value.as_bool()==Some(true) && allowed.iter().any(|v| v==id));
    if memberships.is_empty() { return Err("Mailbox access unavailable".into()); }
    Ok(())
}

pub fn list_attachments_in_scope(cfg:&Config, account:&str, provider:&SelfHostedClient, native:&str, email_id:&str) -> Result<Vec<AttachmentInfo>,String> {
    let selected = readable_scoped_client(cfg,account,provider,native,email_id)?;
    list_attachments(cfg,account,&selected,email_id)
}

pub fn download_attachment_in_scope(cfg:&Config, account:&str, provider:&SelfHostedClient, native:&str, email_id:&str, blob_id:&str) -> Result<DownloadedAttachment,String> {
    let selected = readable_scoped_client(cfg,account,provider,native,email_id)?;
    download_attachment(cfg,account,&selected,email_id,blob_id)
}

pub struct DownloadedAttachment {
    pub filename: String,
    pub content_type: String,
    pub blob_id: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachmentInfo {
    pub blob_id: String,
    pub filename: String,
    pub content_type: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelatedDraftKind { Reply, Forward }

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RelatedDraftPreview {
    pub account: String,
    pub origin: String,
    pub remote_account: String,
    pub source_email_id: String,
    pub source_blob_id: String,
    pub kind: RelatedDraftKind,
    pub source: Value,
    pub attachments: Vec<AttachmentInfo>,
}

/// Bind a reply/forward preview to an immutable native email and selected MIME
/// blobs. The caller must show the source and selected attachments to the user.
pub fn prepare_related_draft(cfg: &Config, account: &str, provider: &SelfHostedClient, source_email_id: &str, kind: RelatedDraftKind, selected_blob_ids: &[String]) -> Result<RelatedDraftPreview, String> {
    let scope = scope(cfg, account, provider)?;
    if source_email_id.is_empty() || selected_blob_ids.len() > 32 { return Err("Invalid source email or attachments".into()); }
    let response = provider.client.get_email_bodies(&[source_email_id.into()], 1024 * 1024).map_err(|_| "Source email unavailable")?;
    if !list(&response, "notFound")?.is_empty() { return Err("Source email unavailable".into()); }
    let rows = response["list"].as_array().ok_or("Invalid source email")?;
    if rows.len() != 1 || rows[0]["id"].as_str() != Some(source_email_id) { return Err("Invalid source email".into()); }
    let source = &rows[0];
    validate_complete_body(source).map_err(|_| "Source email body incomplete")?;
    let blob_id = field(source, "blobId")?;
    if kind == RelatedDraftKind::Reply && source["messageId"].as_array().is_none_or(|ids| ids.len() != 1 || ids[0].as_str().is_none_or(str::is_empty)) { return Err("Reply requires one source Message-ID".into()); }
    let available = list_attachments(cfg, account, provider, source_email_id)?;
    let mut unique = std::collections::HashSet::new();
    let mut attachments = Vec::new();
    for id in selected_blob_ids {
        if !unique.insert(id) { return Err("Duplicate attachment".into()); }
        let mut found = available.iter().filter(|item| &item.blob_id == id);
        let part = found.next().ok_or("Source attachment unavailable")?;
        if found.next().is_some() { return Err("Source attachment ambiguous".into()); }
        attachments.push(part.clone());
    }
    Ok(RelatedDraftPreview { account:account.into(), origin:scope.origin, remote_account:scope.collection,
        source_email_id:source_email_id.into(), source_blob_id:blob_id.into(), kind, source:source.clone(), attachments })
}

/// Create a reply/forward only after revalidating the exact approved source.
pub fn create_related_draft(cfg: &Config, account: &str, store: &Store, provider: &SelfHostedClient, operation_id: &str, approved: &RelatedDraftPreview, email: &Value) -> Result<SendOutcome, String> {
    let selected: Vec<String> = approved.attachments.iter().map(|a| a.blob_id.clone()).collect();
    let current = prepare_related_draft(cfg, account, provider, &approved.source_email_id, approved.kind.clone(), &selected)?;
    if &current != approved { return Err("Source email changed; new approval required".into()); }
    if approved.kind == RelatedDraftKind::Reply {
        if email.get("inReplyTo") != approved.source.get("messageId") { return Err("Reply references do not match source".into()); }
        if let Some(references) = email.get("references") {
            let mut expected = approved.source["references"].as_array().cloned().unwrap_or_default();
            expected.extend(approved.source["messageId"].as_array().ok_or("Source Message-ID unavailable")?.iter().cloned());
            if references != &Value::Array(expected) { return Err("Reply references do not match source".into()); }
        }
    } else if email.get("inReplyTo").is_some() || email.get("references").is_some() { return Err("Forward must not use reply references".into()); }
    let requested = email.get("attachments").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    if requested.len() != approved.attachments.len() { return Err("Attachments do not match approved source".into()); }
    for (item, approved_part) in requested.iter().zip(&approved.attachments) {
        if item["blobId"].as_str() != Some(approved_part.blob_id.as_str()) || item["name"].as_str() != Some(approved_part.filename.as_str()) || item["type"].as_str() != Some(approved_part.content_type.as_str()) { return Err("Attachments do not match approved source".into()); }
    }
    create_draft(cfg, account, store, provider, operation_id, email)
}

/// Upload one bounded attachment. A transport failure has an unknown outcome;
/// callers must not assume a blob identity was created or attach a guessed one.
fn upload_draft_attachment(cfg: &Config, account: &str, provider: &SelfHostedClient, bytes: Vec<u8>, content_type: &str) -> Result<String, String> {
    let scope = scope(cfg, account, provider)?;
    if bytes.len() > 64 * 1024 * 1024 || content_type.is_empty() || content_type.len() > 255 || !content_type.is_ascii() || content_type.chars().any(|c| c.is_control() || c.is_whitespace()) { return Err("Invalid attachment".into()); }
    let size = bytes.len() as u64;
    let response = provider.client.upload(bytes, content_type).map_err(|_| "Attachment upload outcome unknown")?;
    if response["accountId"].as_str() != Some(scope.collection.as_str()) || response["size"].as_u64() != Some(size) { return Err("Invalid attachment upload response".into()); }
    let blob = field(&response, "blobId")?;
    if blob.len() > 1024 || blob.chars().any(char::is_control) { return Err("Invalid attachment identity".into()); }
    Ok(blob.into())
}

/// Persist the exact upload intent before dispatch. A lost response is never
/// replayed automatically because JMAP blob upload has no client operation ID.
pub fn upload_draft_attachment_once(cfg: &Config, account: &str, store: &Store, provider: &SelfHostedClient, operation_id: &str, filename: &str, content_type: &str, bytes: Vec<u8>) -> Result<SendOutcome, String> {
    let scope = scope(cfg, account, provider)?;
    if operation_id.is_empty() || operation_id.len() > 1024 || filename.is_empty() || filename.len() > 255 || filename.chars().any(char::is_control)
        || bytes.len() > 64 * 1024 * 1024 || content_type.is_empty() || content_type.len() > 255
        || !content_type.is_ascii() || content_type.chars().any(|c| c.is_control() || c.is_whitespace()) { return Err("Invalid attachment upload".into()); }
    let intent = serde_json::json!({"filename":filename,"content_type":content_type,"size":bytes.len(),"sha256":format!("{:x}",Sha256::digest(&bytes))}).to_string();
    let digest = format!("{:x}", Sha256::digest(intent.as_bytes()));
    if let Some((previous, state, result)) = store.provider_mutation(&scope, operation_id).map_err(|e| e.to_string())? {
        if previous != digest { return Err("Upload differs from recorded intent".into()); }
        return Ok(match state.as_str() { "confirmed" => SendOutcome::Confirmed(result.ok_or("Missing native blob identity")?), "rejected" => SendOutcome::Rejected, _ => SendOutcome::Unknown });
    }
    if !store.claim_provider_mutation(&scope, operation_id, "mail_attachment_upload", &intent, &digest).map_err(|e| e.to_string())? { return Ok(SendOutcome::Unknown); }
    match upload_draft_attachment(cfg, account, provider, bytes, content_type) {
        Ok(blob) if store.finish_provider_mutation(&scope, operation_id, Some(&blob)).map_err(|e| e.to_string())? => Ok(SendOutcome::Confirmed(blob)),
        _ => Ok(SendOutcome::Unknown),
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AttachmentUploadStatus {
    pub remote_account: String,
    pub filename: String,
    pub content_type: String,
    pub size: u64,
    pub sha256: String,
    pub state: String,
    pub blob_id: Option<String>,
}

/// Read the scoped durable upload result without contacting the provider.
pub fn draft_attachment_upload_status(cfg: &Config, account: &str, store: &Store, operation_id: &str) -> Result<Option<AttachmentUploadStatus>, String> {
    if operation_id.is_empty() || operation_id.len() > 1024 { return Err("Invalid operation identity".into()); }
    let origin = match cfg.provider_accounts.get(account) { Some(ProviderConfig::SelfHosted { origin, .. }) => origin, None => return Err("Not a self-hosted account".into()) };
    let Some((scope, intent, state, result)) = store.provider_mail_upload_intent(account, origin, operation_id).map_err(|e| e.to_string())? else { return Ok(None); };
    let value: Value = serde_json::from_str(&intent).map_err(|_| "Invalid recorded upload")?;
    let filename = field(&value, "filename")?.to_owned();
    let content_type = field(&value, "content_type")?.to_owned();
    let sha256 = field(&value, "sha256")?.to_owned();
    let size = value["size"].as_u64().ok_or("Invalid recorded upload")?;
    let blob_id = match state.as_str() {
        "confirmed" => Some(result.filter(|id| !id.is_empty()).ok_or("Missing native blob identity")?),
        "unknown" | "rejected" | "conflict" => None,
        _ => return Err("Invalid upload state".into()),
    };
    Ok(Some(AttachmentUploadStatus { remote_account:scope.collection, filename, content_type, size, sha256, state, blob_id }))
}

/// Resolve only durable, confirmed uploads in the same provider account for a
/// draft approval. Unknown and duplicate operations never produce attachments.
pub fn resolve_uploaded_attachments(cfg: &Config, account: &str, store: &Store, operation_ids: &[String]) -> Result<Vec<AttachmentUploadStatus>, String> {
    if operation_ids.len() > 32 { return Err("Too many attachments".into()); }
    let mut seen = std::collections::HashSet::new();
    let mut blobs = std::collections::HashSet::new();
    let mut output = Vec::with_capacity(operation_ids.len());
    let mut remote_account: Option<String> = None;
    for operation in operation_ids {
        if !seen.insert(operation) { return Err("Duplicate attachment operation".into()); }
        let status = draft_attachment_upload_status(cfg, account, store, operation)?.ok_or("Attachment upload unavailable")?;
        if remote_account.as_ref().is_some_and(|id| id != &status.remote_account) { return Err("Attachments belong to different native accounts".into()); }
        remote_account.get_or_insert_with(|| status.remote_account.clone());
        let blob = status.blob_id.as_ref().filter(|_| status.state == "confirmed").ok_or("Attachment upload not confirmed")?;
        if !blobs.insert(blob.clone()) { return Err("Duplicate attachment blob".into()); }
        output.push(status);
    }
    Ok(output)
}

/// Fetch only a blob referenced by the live MIME tree of this exact email.
/// The transport limits the response to 64 MiB; metadata never supplies a URL.
pub fn list_attachments(cfg:&Config, account:&str, provider:&SelfHostedClient, email_id:&str) -> Result<Vec<AttachmentInfo>,String> {
    scope(cfg,account,provider)?;
    if email_id.is_empty() { return Err("Invalid email identity".into()); }
    let result=provider.client.get("Email",Some(&[email_id.to_owned()]),Some(&["id".into(),"bodyStructure".into(),"textBody".into(),"htmlBody".into()])).map_err(|_| "Email unavailable")?;
    if !list(&result,"notFound")?.is_empty() { return Err("Email unavailable".into()); }
    let rows=result["list"].as_array().ok_or("Invalid email")?;
    if rows.len()!=1 || rows[0]["id"].as_str()!=Some(email_id) { return Err("Invalid email identity".into()); }
    mime_attachments(&rows[0])
}

fn mime_attachments(email:&Value) -> Result<Vec<AttachmentInfo>,String> {
    let mut body_ids=std::collections::HashSet::new();
    for key in ["textBody","htmlBody"] {
        for part in email[key].as_array().ok_or("MIME body parts unavailable")? {
            if let Some(id) = part["partId"].as_str().filter(|id| !id.is_empty()) {
                body_ids.insert(id.to_owned());
            } else if part["type"].as_str().is_none_or(|kind| kind.starts_with("text/")) {
                return Err("MIME body identity unavailable".into());
            }
        }
    }
    let root=email.get("bodyStructure").filter(|v| v.is_object()).ok_or("MIME structure unavailable")?;
    let mut pending=vec![(root,0usize)]; let mut found=Vec::new(); let mut visited=0usize;
    while let Some((part,depth))=pending.pop() {
        visited+=1;
        if depth>32 || visited>10000 { return Err("MIME structure exceeds limit".into()); }
        if let Some(blob_id)=part.get("blobId").filter(|v| !v.is_null() && !part["partId"].as_str().is_some_and(|id| body_ids.contains(id))) {
            let blob_id=blob_id.as_str().filter(|v| !v.is_empty() && v.len()<=1024 && !v.chars().any(char::is_control)).ok_or("Invalid attachment identity")?;
            let size=part["size"].as_u64().ok_or("Attachment size unavailable")?;
            let original=part["name"].as_str().unwrap_or("attachment");
            let filename:String=original.chars().take(200).map(|c| if c.is_control() || matches!(c,'/'|'\\'|'"') { '_' } else { c }).collect();
            let filename=if filename.is_empty() || filename=="." || filename==".." { "attachment".into() } else { filename };
            let content_type=part["type"].as_str().filter(|s| s.len()<=255 && s.is_ascii() && !s.chars().any(|c| c.is_control() || c.is_whitespace())).unwrap_or("application/octet-stream").to_owned();
            found.push(AttachmentInfo {blob_id:blob_id.into(),filename,content_type,size});
        }
        if let Some(children)=part.get("subParts").filter(|v| !v.is_null()) {
            for child in children.as_array().ok_or("Invalid MIME structure")? {
                if !child.is_object() { return Err("Invalid MIME structure".into()); }
                pending.push((child,depth+1));
            }
        }
    }
    Ok(found)
}

pub fn download_attachment(cfg:&Config, account:&str, provider:&SelfHostedClient, email_id:&str, blob_id:&str) -> Result<DownloadedAttachment,String> {
    if blob_id.is_empty() || blob_id.len()>1024 { return Err("Invalid attachment identity".into()); }
    let matches:Vec<_>=list_attachments(cfg,account,provider,email_id)?.into_iter().filter(|part| part.blob_id==blob_id).collect();
    if matches.len()!=1 { return Err("Attachment unavailable or ambiguous".into()); }
    let part=matches.into_iter().next().ok_or("Attachment unavailable")?;
    if part.size>64*1024*1024 { return Err("Attachment exceeds download limit".into()); }
    let bytes=provider.client.download(blob_id,&part.filename,&part.content_type).map_err(|_| "Attachment download failed")?;
    Ok(DownloadedAttachment {filename:part.filename,content_type:part.content_type,blob_id:part.blob_id,bytes})
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

#[derive(Debug, Clone, serde::Serialize)]
pub struct SendingIdentity {
    pub id: String,
    pub name: String,
    pub email: String,
}

/// Return only the fields needed to explicitly choose a native identity.
pub fn selectable_sending_identities(cfg:&Config, account:&str, provider:&SelfHostedClient) -> Result<Vec<SendingIdentity>,String> {
    let values = sending_identities(cfg,account,provider)?;
    if values.len()>1000 { return Err("Too many sending identities".into()); }
    let mut identities=Vec::with_capacity(values.len());
    let mut ids=std::collections::HashSet::new();
    for value in values {
        let id=value["id"].as_str().filter(|v| !v.is_empty() && v.len()<=1024 && !v.chars().any(char::is_control)).ok_or("Invalid sending identity")?;
        let name=value["name"].as_str().filter(|v| v.len()<=1024 && !v.chars().any(char::is_control)).ok_or("Invalid sending identity")?;
        let email=value["email"].as_str().filter(|v| !v.is_empty() && v.len()<=320 && !v.chars().any(char::is_control)).ok_or("Invalid sending identity")?;
        if !ids.insert(id.to_owned()) { return Err("Duplicate sending identity".into()); }
        identities.push(SendingIdentity {id:id.into(),name:name.into(),email:email.into()});
    }
    Ok(identities)
}

/// Re-resolve an explicitly selected identity at the time of use.
pub fn selected_sending_identity(cfg:&Config, account:&str, provider:&SelfHostedClient, identity_id:&str) -> Result<SendingIdentity,String> {
    let mut matches=selectable_sending_identities(cfg,account,provider)?.into_iter().filter(|identity| identity.id==identity_id);
    matches.next().ok_or_else(|| "Sending identity unavailable".into())
}

/// Read the native seen flag without using a stale cache result.
pub fn read_state(cfg: &Config, account: &str, provider: &SelfHostedClient, email_id: &str) -> Result<bool, String> {
    scope(cfg,account,provider)?;
    if email_id.is_empty() { return Err("Missing email identity".into()); }
    let ids = vec![email_id.to_owned()];
    let result = provider.client.get("Email",Some(&ids),None).map_err(|_| "JMAP email unavailable")?;
    if !list(&result,"notFound")?.is_empty() { return Err("JMAP email unavailable".into()); }
    let rows = result["list"].as_array().ok_or("Invalid JMAP email")?;
    if rows.len() != 1 || rows[0]["id"].as_str() != Some(email_id) { return Err("Invalid JMAP email identity".into()); }
    let keywords = rows[0]["keywords"].as_object().ok_or("Invalid JMAP keywords")?;
    match keywords.get("$seen") { None => Ok(false), Some(Value::Bool(true)) => Ok(true), _ => Err("Invalid JMAP seen flag".into()) }
}

/// Actions with an explicit, reviewable native mailbox effect. Trash is a move,
/// never permanent destruction.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MailAction {
    SetFlag { flagged: bool },
    SetRead { is_read: bool },
    Move { source_mailbox_id: String, destination_mailbox_id: String },
    Trash,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MailActionPreview {
    pub account: String,
    pub origin: String,
    pub remote_account: String,
    pub email_id: String,
    pub blob_id: String,
    pub email_state: String,
    pub subject: Value,
    pub from: Value,
    pub mailbox_ids: Value,
    pub mailboxes: Value,
    pub keywords: Value,
    pub action: MailAction,
    pub patch: Value,
}

fn pointer_token(value: &str) -> String { value.replace('~', "~0").replace('/', "~1") }

/// Resolve the native object and current mailbox rights for a consent preview.
pub fn preview_mail_action(cfg: &Config, account: &str, provider: &SelfHostedClient, email_id: &str, action: MailAction) -> Result<MailActionPreview, String> {
    let scope = scope(cfg, account, provider)?;
    if email_id.is_empty() || email_id.len() > 1024 { return Err("Invalid email identity".into()); }
    let response = provider.client.get("Email", Some(&[email_id.into()]), None).map_err(|_| "Email unavailable")?;
    if !list(&response, "notFound")?.is_empty() { return Err("Email unavailable".into()); }
    let rows = response["list"].as_array().ok_or("Invalid email")?;
    if rows.len() != 1 || rows[0]["id"].as_str() != Some(email_id) { return Err("Invalid email identity".into()); }
    let email = &rows[0];
    let memberships = email["mailboxIds"].as_object().ok_or("Email memberships unavailable")?;
    if memberships.is_empty() || memberships.len() > 1000 || memberships.iter().any(|(id, present)| id.is_empty() || id.len() > 1024 || id.chars().any(char::is_control) || present.as_bool() != Some(true)) { return Err("Invalid email memberships".into()); }
    let keywords = email["keywords"].as_object().ok_or("Email keywords unavailable")?;
    if keywords.values().any(|v| v.as_bool() != Some(true)) { return Err("Invalid email keywords".into()); }
    let boxes = provider.client.get("Mailbox", None, None).map_err(|_| "Mailboxes unavailable")?;
    let boxes = boxes["list"].as_array().ok_or("Invalid mailboxes")?;
    if boxes.len() > 10000 { return Err("Too many mailboxes".into()); }
    let find = |id: &str| -> Result<&Value, String> {
        let mut matches = boxes.iter().filter(|b| b["id"].as_str() == Some(id));
        let value = matches.next().ok_or("Mailbox unavailable")?;
        if matches.next().is_some() { return Err("Ambiguous mailbox".into()); }
        Ok(value)
    };
    let mut patch = serde_json::Map::new();
    match &action {
        MailAction::SetRead { is_read } => {
            for id in memberships.keys() {
                if find(id)?.pointer("/myRights/maySetSeen").and_then(Value::as_bool) != Some(true) { return Err("Read state is not writable".into()); }
            }
            if keywords.contains_key("$seen") == *is_read { return Err("Read state already has requested value".into()); }
            patch.insert("keywords/$seen".into(), if *is_read { Value::Bool(true) } else { Value::Null });
        }
        MailAction::SetFlag { flagged } => {
            for id in memberships.keys() {
                if find(id)?.pointer("/myRights/maySetKeywords").and_then(Value::as_bool) != Some(true) { return Err("Flag is not writable".into()); }
            }
            if keywords.contains_key("$flagged") == *flagged { return Err("Flag already has requested value".into()); }
            patch.insert("keywords/$flagged".into(), if *flagged { Value::Bool(true) } else { Value::Null });
        }
        MailAction::Move { source_mailbox_id, destination_mailbox_id } => {
            if destination_mailbox_id.is_empty() || destination_mailbox_id.len() > 1024 || destination_mailbox_id.chars().any(char::is_control) || source_mailbox_id == destination_mailbox_id || !memberships.contains_key(source_mailbox_id) || memberships.contains_key(destination_mailbox_id) { return Err("Invalid mailbox move".into()); }
            if find(source_mailbox_id)?.pointer("/myRights/mayRemoveItems").and_then(Value::as_bool) != Some(true) || find(destination_mailbox_id)?.pointer("/myRights/mayAddItems").and_then(Value::as_bool) != Some(true) { return Err("Mailbox move is not permitted".into()); }
            patch.insert(format!("mailboxIds/{}", pointer_token(source_mailbox_id)), Value::Null);
            patch.insert(format!("mailboxIds/{}", pointer_token(destination_mailbox_id)), Value::Bool(true));
        }
        MailAction::Trash => {
            if memberships.len() >= 128 { return Err("Too many mailbox memberships for one operation".into()); }
            let trash: Vec<_> = boxes.iter().filter(|b| b["role"].as_str() == Some("trash")).collect();
            if trash.len() != 1 { return Err("Trash mailbox unavailable or ambiguous".into()); }
            let trash_id = field(trash[0], "id")?;
            if trash_id.len() > 1024 || trash_id.chars().any(char::is_control) || memberships.contains_key(trash_id) || trash[0].pointer("/myRights/mayAddItems").and_then(Value::as_bool) != Some(true) { return Err("Trash mailbox is not writable or email is already in trash".into()); }
            for id in memberships.keys() {
                if find(id)?.pointer("/myRights/mayRemoveItems").and_then(Value::as_bool) != Some(true) { return Err("Email cannot be removed from a mailbox".into()); }
                patch.insert(format!("mailboxIds/{}", pointer_token(id)), Value::Null);
            }
            patch.insert(format!("mailboxIds/{}", pointer_token(trash_id)), Value::Bool(true));
        }
    }
    let mut mailbox_descriptions: Vec<Value> = boxes.iter().filter(|b| {
        let id = b["id"].as_str().unwrap_or("");
        memberships.contains_key(id) || match &action {
            MailAction::Move { destination_mailbox_id, .. } => id == destination_mailbox_id.as_str(),
            MailAction::Trash => b["role"].as_str() == Some("trash"),
            _ => false,
        }
    }).map(|b| serde_json::json!({"id": b["id"], "name": b["name"], "role": b["role"]})).collect();
    mailbox_descriptions.sort_by(|a,b| a["id"].as_str().cmp(&b["id"].as_str()));
    Ok(MailActionPreview { account:account.into(), origin:scope.origin, remote_account:scope.collection,
        email_id:email_id.into(), blob_id:field(email,"blobId")?.into(), email_state:field(&response,"state")?.into(),
        subject:email["subject"].clone(), from:email["from"].clone(), mailbox_ids:email["mailboxIds"].clone(),
        mailboxes:Value::Array(mailbox_descriptions), keywords:email["keywords"].clone(), action, patch:Value::Object(patch) })
}

/// Dispatch an approved native action at most once. A lost response stays
/// unknown; observing the desired state alone cannot prove which actor set it.
pub fn apply_mail_action(cfg: &Config, account: &str, store: &Store, provider: &SelfHostedClient, operation_id: &str, approved: &MailActionPreview) -> Result<SendOutcome, String> {
    if operation_id.is_empty() || operation_id.len() > 1024 { return Err("Invalid operation identity".into()); }
    let scope = scope(cfg, account, provider)?;
    let intent = serde_json::to_string(approved).map_err(|_| "Invalid mail action")?;
    let digest = format!("{:x}", Sha256::digest(intent.as_bytes()));
    if let Some((previous, state, result)) = store.provider_mutation(&scope, operation_id).map_err(|e| e.to_string())? {
        if previous != digest { return Err("Mail action differs from recorded intent".into()); }
        return Ok(match state.as_str() { "confirmed" => SendOutcome::Confirmed(result.ok_or("Missing native email identity")?), "rejected" => SendOutcome::Rejected, _ => SendOutcome::Unknown });
    }
    let current = preview_mail_action(cfg, account, provider, &approved.email_id, approved.action.clone())?;
    if &current != approved { return Err("Mail action changed; new approval required".into()); }
    if !store.claim_provider_target_mutation(&scope, operation_id, "mail_action", &approved.email_id, &intent, &digest).map_err(|e| e.to_string())? { return Ok(SendOutcome::Unknown); }
    match provider.client.update_email(&approved.email_id, &approved.email_state, &approved.patch) {
        Ok(EmailUpdateResult::Applied) if store.finish_provider_mutation(&scope, operation_id, Some(&approved.email_id)).map_err(|e| e.to_string())? => Ok(SendOutcome::Confirmed(approved.email_id.clone())),
        Ok(EmailUpdateResult::Rejected) if store.finish_provider_mutation(&scope, operation_id, None).map_err(|e| e.to_string())? => Ok(SendOutcome::Rejected),
        Err(JmapError::Method(kind)) if kind == "stateMismatch" => {
            if store.finish_provider_mutation(&scope, operation_id, None).map_err(|e| e.to_string())? { Ok(SendOutcome::Rejected) } else { Ok(SendOutcome::Unknown) }
        }
        _ => Ok(SendOutcome::Unknown),
    }
}

/// Initial enumeration is reconciled only when all pages share one query state.
/// Subsequent changes are applied with their cursor in the same transaction.
pub fn sync(
    cfg: &Config,
    account: &str,
    store: &Store,
    client: &SelfHostedClient,
) -> Result<usize, String> {
    sync_detailed(cfg,account,store,client).map_err(|(_,error)| error)
}

/// Fixed, non-sensitive phase for diagnostics; never contains account or provider data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MailSyncPhase { Configuration, SharedAccess, Primary, SharedSelection, SharedMailbox, SharedEmail }

pub fn sync_with_phase(cfg:&Config, account:&str, store:&Store, client:&SelfHostedClient) -> Result<usize,MailSyncPhase> {
    sync_detailed(cfg,account,store,client).map_err(|(phase,_)| phase)
}

pub fn last_sync_failure(client:&SelfHostedClient) -> Option<isyncyou_connectors::jmap::JmapSyncFailure> { client.client.last_sync_failure() }

fn sync_detailed(cfg:&Config, account:&str, store:&Store, client:&SelfHostedClient) -> Result<usize,(MailSyncPhase,String)> {
    client.client.clear_sync_failure();
    let primary = scope(cfg, account, client).map_err(|e|(MailSyncPhase::Configuration,e))?;
    if primary.service != "mail" { return Err((MailSyncPhase::Configuration,"Primary mail client required".into())); }
    // Only scopes absent from the newly authenticated discovery are revoked.
    // Existing scopes remain readable during a refresh; product reads still
    // require fresh discovery and mailbox rights on every request.
    for old in store.provider_scopes(account, &primary.origin, "mail-shared").map_err(|e| {client.client.record_store_failure(); (MailSyncPhase::SharedAccess,e.to_string())})? {
        if !client.client.mail_accounts().iter().any(|(native,_)| native==&old.collection && native!=client.client.primary_account_id()) {
            store.set_shared_mail_access(account, &primary.origin, &old.collection, false).map_err(|e| {client.client.record_store_failure(); (MailSyncPhase::SharedAccess,e.to_string())})?;
        }
    }
    let mut count = sync_with_page_limit(cfg,account,store,client,100).map_err(|e|(MailSyncPhase::Primary,e))?;
    client.client.clear_sync_failure();
    for (native, _) in client.client.mail_accounts() {
        if native == client.client.primary_account_id() { continue; }
        let scope=ProviderScope {account_id:account.into(),origin:primary.origin.clone(),service:"mail-shared".into(),collection:native.clone()};
        if store.provider_accessible(&scope).map_err(|e| {client.client.record_store_failure(); (MailSyncPhase::SharedAccess,e.to_string())})?.is_none() {
            store.set_shared_mail_access(account, &primary.origin, native, false).map_err(|e| {client.client.record_store_failure(); (MailSyncPhase::SharedAccess,e.to_string())})?;
        }
        let selected = client.client.for_mail_account(native).map_err(|_| (MailSyncPhase::SharedSelection,"Shared mail account unavailable".into()))?;
        let shared = SelfHostedClient { client:selected, account:account.into(), origin:primary.origin.clone() };
        let refresh = (|| {
            let folders = sync_mailboxes_inner(cfg,account,store,&shared,true).map_err(|e|(MailSyncPhase::SharedMailbox,e))?;
            let messages = sync_inner(cfg,account,store,&shared,true,100).map_err(|e|(MailSyncPhase::SharedEmail,e))?;
            Ok::<_,(MailSyncPhase,String)>(folders+messages)
        })();
        let refreshed = match refresh { Ok(count)=>count, Err(error)=>{
            store.set_shared_mail_access(account, &primary.origin, native, false).map_err(|e| {client.client.record_store_failure(); (MailSyncPhase::SharedAccess,e.to_string())})?;
            return Err(error);
        }};
        store.set_shared_mail_access(account, &primary.origin, native, true).map_err(|e| {client.client.record_store_failure(); (MailSyncPhase::SharedAccess,e.to_string())})?;
        count += refreshed;
    }
    client.client.clear_sync_failure();
    Ok(count)
}

/// Run the normal sync with a smaller bounded page size. This permits callers
/// to exercise pagination without modifying server-wide limits.
pub fn sync_with_page_limit(
    cfg: &Config,
    account: &str,
    store: &Store,
    client: &SelfHostedClient,
    page_limit: usize,
) -> Result<usize, String> {
    if page_limit == 0 || page_limit > 100 { return Err("Invalid mail page limit".into()); }
    let before = client.client.sync_metrics();
    let folders = sync_mailboxes_inner(cfg, account, store, client, false)?;
    let messages = sync_inner(cfg, account, store, client, false, page_limit)?;
    let after = client.client.sync_metrics();
    eprintln!("isyncyou_provider_mail_cycle email_query={} email_get={} email_changes={} created={} updated={} body_fetch={} blob_download={} response_bytes={} blob_bytes={}",
        after.email_query.saturating_sub(before.email_query), after.email_get.saturating_sub(before.email_get), after.email_changes.saturating_sub(before.email_changes),
        after.created.saturating_sub(before.created), after.updated.saturating_sub(before.updated),
        after.body_fetch.saturating_sub(before.body_fetch), after.blob_download.saturating_sub(before.blob_download),
        after.response_bytes.saturating_sub(before.response_bytes), after.blob_bytes.saturating_sub(before.blob_bytes));
    Ok(folders + messages)
}

/// Aggregate transport counters without exposing account data or credentials.
pub fn sync_metrics(client: &SelfHostedClient) -> isyncyou_connectors::jmap::JmapSyncMetrics {
    client.client.sync_metrics()
}

/// Mailbox identity and state are distinct from Email identity and state.
pub fn mailbox_scope(
    cfg: &Config,
    account: &str,
    provider: &SelfHostedClient,
) -> Result<ProviderScope, String> {
    let mut scope = scope(cfg, account, provider)?;
    scope.service = if scope.service == "mail-shared" { "mailboxes-shared" } else { "mailboxes" }.into();
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
        store.provider_cursor(&scope).map_err(|e| { client.record_store_failure(); e.to_string() })?
    };
    let mut cursor = if let Some(cursor) = saved {
        cursor
    } else {
        let empty: Vec<String> = Vec::new();
        let baseline = client
            .get("Mailbox", Some(&empty), None)
            .map_err(|_| "JMAP mailbox state unavailable")?;
        let baseline = field(&baseline, "state")?.to_owned();
        let staging = StageGuard::new(store, &scope, client)?;
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
                client.clear_sync_failure();
                return sync_mailboxes_inner(cfg, account, store, provider, true)
            }
            Err(_) => return Err("JMAP mailbox delta unavailable".into()),
        };
        if field(&page, "oldState")? != cursor {
            return Err("JMAP mailbox state mismatch".into());
        }
        let next = field(&page, "newState")?.to_owned();
        let mut ids = list(&page, "created")?;
        let updated = list(&page, "updated")?;
        ids.extend(updated);
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
        let more = page["hasMoreChanges"].as_bool().ok_or("Invalid JMAP mailbox pagination")?;
        if more && next == cursor {
            return Err("JMAP mailbox pagination did not advance".into());
        }
        if !store.apply_provider_page_if_cursor(&scope, &cursor, &changes, &next)
            .map_err(|e| { client.record_store_failure(); e.to_string() })? {
            return Err("JMAP mailbox checkpoint changed; retry required".into());
        }
        cursor = next;
        if !more { break; }
    }
    Ok(count)
}

fn sync_inner(
    cfg: &Config,
    account: &str,
    store: &Store,
    provider: &SelfHostedClient,
    reconcile: bool,
    page_limit: usize,
) -> Result<usize, String> {
    let scope = scope(cfg, account, provider)?;
    let client = &provider.client;
    let page_size = client.max_objects_in_get().min(page_limit);
    if page_size == 0 {
        return Err("JMAP server has no usable batch size".into());
    }
    let mut count = 0;
    let cursor = if reconcile {
        None
    } else {
        store.provider_cursor(&scope).map_err(|e| { client.record_store_failure(); e.to_string() })?
    };
    let mut cursor = if let Some(cursor) = cursor {
        cursor
    } else {
        let empty: Vec<String> = Vec::new();
        let baseline = client
            .get("Email", Some(&empty), None)
            .map_err(|_| "JMAP email state unavailable")?;
        let baseline = field(&baseline, "state")?.to_string();
        let staging = StageGuard::new(store, &scope, client)?;
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
                client.clear_sync_failure();
                client.record_email_reconciliation();
                return sync_inner(cfg, account, store, provider, true, page_limit)
            }
            Err(_) => return Err("JMAP delta unavailable".into()),
        };
        if field(&page, "oldState")? != cursor {
            return Err("JMAP delta state mismatch".into());
        }
        let next = field(&page, "newState")?.to_owned();
        let mut ids = list(&page, "created")?;
        let updated = list(&page, "updated")?;
        client.record_email_changes(ids.len(), updated.len());
        ids.extend(updated);
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
        let more = page["hasMoreChanges"].as_bool().ok_or("Invalid JMAP pagination")?;
        if more && next == cursor {
            return Err("JMAP pagination did not advance".into());
        }
        if !store.apply_provider_page_if_cursor(&scope, &cursor, &changes, &next)
            .map_err(|e| { client.record_store_failure(); e.to_string() })? {
            return Err("JMAP checkpoint changed; retry required".into());
        }
        cursor = next;
        if !more { break; }
    }
    Ok(count)
}

/// A previously confirmed exact native email and SMTP envelope. The host must
/// bind its owner/session/expiry approval to all fields before invoking send.
pub struct ApprovedSubmission {
    pub operation_id: String,
    pub remote_account: String,
    pub email_id: String,
    pub blob_id: String,
    pub identity_id: String,
    pub mail_from: String,
    pub recipients: Vec<String>,
    pub draft_mailbox_id: String,
    pub sent_mailbox_id: String,
}

/// Live, account-bound material for an explicit send confirmation. The host
/// must bind every field of the resulting submission to its approval ledger.
#[derive(serde::Serialize)]
pub struct PreparedSubmission {
    pub account: String,
    pub origin: String,
    pub remote_account: String,
    pub email_id: String,
    pub blob_id: String,
    pub identity_id: String,
    pub mail_from: String,
    pub recipients: Vec<String>,
    pub draft_mailbox_id: String,
    pub sent_mailbox_id: String,
    pub email: Value,
}

/// Read the current native draft and associated send identities for preview.
/// A truncated body is never presented as a complete approval snapshot.
pub fn prepare_submission(cfg: &Config, account: &str, provider: &SelfHostedClient, email_id: &str, identity_id: &str) -> Result<PreparedSubmission, String> {
    let scope = scope(cfg, account, provider)?;
    if email_id.is_empty() || identity_id.is_empty() { return Err("Missing draft or identity".into()); }
    let ids = vec![email_id.to_owned()];
    let result = provider.client.get_email_bodies(&ids, 16 * 1024 * 1024).map_err(|_| "Draft unavailable")?;
    if !list(&result, "notFound")?.is_empty() { return Err("Draft unavailable".into()); }
    let rows = result["list"].as_array().ok_or("Invalid draft")?;
    if rows.len()!=1 || rows[0]["id"].as_str()!=Some(email_id) { return Err("Invalid draft identity".into()); }
    let email = &rows[0];
    let blob_id = email["blobId"].as_str().filter(|v| !v.is_empty()).ok_or("Missing draft blob")?;
    validate_complete_body(email)?;
    if serde_json::to_vec(email).map_err(|_| "Invalid draft")?.len()>16*1024*1024 { return Err("Draft preview exceeds limit".into()); }
    let recipients = native_recipients(email)?;
    if recipients.is_empty() { return Err("Draft recipients required".into()); }
    let identities = provider.client.get("Identity",Some(&[identity_id.to_owned()]),None).map_err(|_| "Sending identity unavailable")?;
    let identities = identities["list"].as_array().ok_or("Sending identity unavailable")?;
    if identities.len()!=1 || identities[0]["id"].as_str()!=Some(identity_id) { return Err("Sending identity unavailable".into()); }
    let mail_from=identities[0]["email"].as_str().filter(|s| !s.is_empty() && s.len()<=320 && !s.chars().any(char::is_control)).ok_or("Invalid sending identity")?;
    let boxes=provider.client.get("Mailbox",None,None).map_err(|_| "Mailboxes unavailable")?;
    let boxes=boxes["list"].as_array().ok_or("Mailboxes unavailable")?;
    let drafts:Vec<_>=boxes.iter().filter(|b| b["role"].as_str()==Some("drafts") && email["mailboxIds"].get(b["id"].as_str().unwrap_or("")).and_then(Value::as_bool)==Some(true) && b.pointer("/myRights/mayRemoveItems").and_then(Value::as_bool)==Some(true)).collect();
    let sent:Vec<_>=boxes.iter().filter(|b| b["role"].as_str()==Some("sent") && b.pointer("/myRights/mayAddItems").and_then(Value::as_bool)==Some(true)).collect();
    if drafts.len()!=1 || sent.len()!=1 { return Err("Drafts or Sent mailbox is unavailable or ambiguous".into()); }
    let draft_mailbox_id=field(drafts[0],"id")?.to_owned();
    let sent_mailbox_id=field(sent[0],"id")?.to_owned();
    if draft_mailbox_id==sent_mailbox_id { return Err("Invalid submission mailboxes".into()); }
    Ok(PreparedSubmission { account:account.into(),origin:scope.origin,remote_account:scope.collection,email_id:email_id.into(),blob_id:blob_id.into(),identity_id:identity_id.into(),mail_from:mail_from.into(),recipients,draft_mailbox_id,sent_mailbox_id,email:email.clone() })
}

fn validate_complete_body(email: &Value) -> Result<(), String> {
    let values = email["bodyValues"].as_object().ok_or("Draft body unavailable")?;
    if values.values().any(|v| !complete_body_value(v)) { return Err("Draft body is incomplete".into()); }
    for key in ["textBody", "htmlBody"] {
        let parts = email[key].as_array().ok_or("Draft body structure unavailable")?;
        for part in parts {
            let media_type = part["type"].as_str().ok_or("Draft body type unavailable")?;
            if !media_type.starts_with("text/") { continue; }
            let id = part["partId"].as_str().filter(|s| !s.is_empty()).ok_or("Draft body part unavailable")?;
            if !values.contains_key(id) { return Err("Draft body is incomplete".into()); }
        }
    }
    Ok(())
}

fn complete_body_value(value: &Value) -> bool {
    let clear = |name: &str| matches!(value.get(name), None | Some(Value::Bool(false)));
    value["value"].as_str().is_some() && clear("isTruncated") && clear("isEncodingProblem")
}

#[derive(Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Confirmed(String),
    Submitted { id: String, sent_update: String },
    Rejected,
    Unknown,
}

fn draft_addresses(value: &Value, key: &str, required: bool) -> Result<Vec<String>, String> {
    let Some(addresses) = value.get(key).filter(|v| !v.is_null()) else {
        return if required { Err("Draft recipients required".into()) } else { Ok(Vec::new()) };
    };
    let addresses = addresses.as_array().ok_or("Invalid draft addresses")?;
    if addresses.len() > 1000 || (required && addresses.is_empty()) {
        return Err("Invalid draft addresses".into());
    }
    addresses.iter().map(|address| {
        let fields = address.as_object().ok_or("Invalid draft address")?;
        let email = fields.get("email").and_then(Value::as_str).ok_or("Invalid draft address")?;
        if fields.len() != 1 || email.is_empty() || email.len() > 320 || email.chars().any(char::is_control) {
            return Err("Invalid draft address".into());
        }
        Ok(email.to_owned())
    }).collect()
}

fn native_recipients(email: &Value) -> Result<Vec<String>, String> {
    let mut result = Vec::new();
    for key in ["to", "cc", "bcc"] {
        if let Some(addresses) = email.get(key).filter(|value| !value.is_null()) {
            for address in addresses.as_array().ok_or("Invalid native recipients")? {
                let value = address.get("email").and_then(Value::as_str).ok_or("Invalid native recipient")?;
                if value.is_empty() || value.len() > 320 || value.chars().any(char::is_control) {
                    return Err("Invalid native recipient".into());
                }
                result.push(value.to_owned());
                if result.len() > 1000 { return Err("Too many native recipients".into()); }
            }
        }
    }
    result.sort();
    result.dedup();
    Ok(result)
}

#[cfg(test)]
mod recipient_tests {
    use super::{complete_body_value, draft_addresses, mime_attachments, native_recipients, pointer_token, validate_complete_body};
    use serde_json::json;

    #[test]
    fn discovery_access_observation_is_scoped_and_does_not_touch_credentials() {
        use isyncyou_core::{config::ProviderConfig,Config};
        let mut cfg=Config::default();
        cfg.provider_accounts.insert("observation-test".into(),ProviderConfig::SelfHosted {origin:"https://example.test".into(),client_id:"client".into()});
        let denied:Result<super::JmapClient,super::JmapError>=Err(super::JmapError::Http(403));
        super::observe_discovery("observation-test","https://example.test",&denied);
        assert_eq!(super::mail_access_observation(&cfg,"observation-test"),super::MailAccessObservation::PermissionDenied);
        let unauthorized:Result<super::JmapClient,super::JmapError>=Err(super::JmapError::Http(401));
        super::observe_discovery("observation-test","https://example.test",&unauthorized);
        assert_eq!(super::mail_access_observation(&cfg,"observation-test"),super::MailAccessObservation::AuthenticationRejected);
        super::observe_unknown("observation-test","https://example.test");
        assert_eq!(super::mail_access_observation(&cfg,"observation-test"),super::MailAccessObservation::Unknown);
    }

    #[test]
    fn shared_body_hides_unreadable_mailbox_memberships() {
        let mut response=json!({"list":[{"id":"m","mailboxIds":{"readable":true,"revoked":true}}]});
        super::retain_readable_memberships(&mut response,"m",&["readable".into()]).unwrap();
        assert_eq!(response["list"][0]["mailboxIds"],json!({"readable":true}));
        assert!(super::retain_readable_memberships(&mut response,"m",&[]).is_err());
    }

    #[test]
    fn offline_upload_resolution_requires_confirmed_scoped_unique_operations() {
        use super::{draft_attachment_upload_status, resolve_uploaded_attachments};
        use isyncyou_core::{config::ProviderConfig, Config};
        use isyncyou_store::{ProviderScope, Store};
        let store = Store::open_in_memory().unwrap();
        let mut cfg = Config::default();
        cfg.provider_accounts.insert("a".into(), ProviderConfig::SelfHosted {origin:"https://example.test".into(),client_id:"client".into()});
        let scope = ProviderScope {account_id:"a".into(),origin:"https://example.test".into(),service:"mail".into(),collection:"native".into()};
        let intent = json!({"filename":"file.txt","content_type":"text/plain","size":4,"sha256":"hash"}).to_string();
        assert!(store.claim_provider_mutation(&scope,"op","mail_attachment_upload",&intent,"digest").unwrap());
        assert!(resolve_uploaded_attachments(&cfg,"a",&store,&["op".into()]).is_err());
        assert!(draft_attachment_upload_status(&cfg,"a",&store,"op").unwrap().unwrap().blob_id.is_none());
        assert!(store.finish_provider_mutation(&scope,"op",Some("blob")).unwrap());
        let result = resolve_uploaded_attachments(&cfg,"a",&store,&["op".into()]).unwrap();
        assert_eq!(result[0].blob_id.as_deref(),Some("blob"));
        assert_eq!(result[0].remote_account,"native");
        assert!(resolve_uploaded_attachments(&cfg,"a",&store,&["op".into(),"op".into()]).is_err());
        assert!(resolve_uploaded_attachments(&cfg,"a",&store,&["missing".into()]).is_err());
    }

    #[test]
    fn body_values_accept_default_false_but_reject_incomplete_or_invalid() {
        assert!(complete_body_value(&json!({"value":"complete"})));
        assert!(complete_body_value(&json!({"value":"complete","isTruncated":false,"isEncodingProblem":false})));
        assert!(!complete_body_value(&json!({"value":"partial","isTruncated":true})));
        assert!(!complete_body_value(&json!({"value":"bad","isEncodingProblem":null})));
        let mut email = json!({"bodyValues":{"text":{"value":"complete"}},"textBody":[{"partId":"text","type":"text/plain"}],"htmlBody":[{"partId":null,"type":"image/png"}]});
        assert!(validate_complete_body(&email).is_ok());
        email["bodyValues"]["text"]["isTruncated"] = json!(true);
        assert!(validate_complete_body(&email).is_err());
        assert_eq!(pointer_token("folder/~id"), "folder~1~0id");
    }

    #[test]
    fn draft_addresses_reject_header_injection_and_unexpected_fields() {
        assert!(draft_addresses(&json!({"to":[{"email":"a@example.org\r\nBcc: x"}]}), "to", true).is_err());
        assert!(draft_addresses(&json!({"to":[{"email":"a@example.org", "name":"A"}]}), "to", true).is_err());
        assert!(draft_addresses(&json!({"to":[]}), "to", true).is_err());
        assert_eq!(draft_addresses(&json!({}), "cc", false).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn native_envelope_includes_cc_and_bcc_and_rejects_malformed_rows() {
        let email = json!({"to":[{"email":"a@example.org","name":"A"}], "cc":[{"email":"b@example.org"}], "bcc":[{"email":"c@example.org"}]});
        assert_eq!(native_recipients(&email).unwrap(), vec!["a@example.org", "b@example.org", "c@example.org"]);
        assert!(native_recipients(&json!({"bcc":[{}]})).is_err());
    }

    #[test]
    fn mime_list_excludes_body_but_keeps_inline_and_attachment() {
        let email=json!({"textBody":[{"partId":"text"}],"htmlBody":[{"partId":"html"}],"bodyStructure":{"subParts":[
            {"partId":"text","blobId":"text-blob","type":"text/plain","size":4},
            {"partId":"html","blobId":"html-blob","type":"text/html","size":4},
            {"partId":"inline","blobId":"inline-blob","type":"image/png","size":8},
            {"partId":"attachment","blobId":"file-blob","type":"application/pdf","name":"a.pdf","size":9}
        ]}});
        let mut ids:Vec<_>=mime_attachments(&email).unwrap().into_iter().map(|item| item.blob_id).collect();
        ids.sort();
        assert_eq!(ids,vec!["file-blob","inline-blob"]);
        assert!(mime_attachments(&json!({"bodyStructure":{}})).is_err());
    }
}

/// Update the seen flag once using a native conditional state and durable intent.
pub fn set_read(cfg:&Config,account:&str,store:&Store,provider:&SelfHostedClient,operation_id:&str,email_id:&str,is_read:bool) -> Result<SendOutcome,String> {
    if operation_id.is_empty() || email_id.is_empty() { return Err("Invalid mail operation".into()); }
    let scope=scope(cfg,account,provider)?;
    let intent=serde_json::to_string(&serde_json::json!({"kind":"set_read","email_id":email_id,"is_read":is_read})).map_err(|_| "Invalid mail operation")?;
    let digest=format!("{:x}",Sha256::digest(intent.as_bytes()));
    if let Some((previous,state,result))=store.provider_mutation(&scope,operation_id).map_err(|e| e.to_string())? {
        if previous!=digest { return Err("Mail operation differs from recorded intent".into()); }
        return Ok(match state.as_str() { "confirmed"=>SendOutcome::Confirmed(result.ok_or("Missing native email identity")?), "rejected"=>SendOutcome::Rejected, _=>SendOutcome::Unknown });
    }
    let ids=vec![email_id.to_owned()];
    let email=provider.client.get("Email",Some(&ids),None).map_err(|_| "Email unavailable")?;
    if !list(&email,"notFound")?.is_empty() { return Err("Email unavailable".into()); }
    let rows=email["list"].as_array().ok_or("Invalid email response")?;
    if rows.len()!=1 || rows[0]["id"].as_str()!=Some(email_id) { return Err("Invalid email identity".into()); }
    let memberships=rows[0]["mailboxIds"].as_object().ok_or("Email mailbox membership unavailable")?;
    if memberships.iter().any(|(id,present)| id.is_empty() || present.as_bool()!=Some(true)) { return Err("Invalid email mailbox membership".into()); }
    let mailbox_ids:Vec<String>=memberships.iter().filter(|(_,present)| present.as_bool()==Some(true)).map(|(id,_)|id.clone()).collect();
    if mailbox_ids.is_empty() || mailbox_ids.len()>provider.client.max_objects_in_get() { return Err("Email mailbox rights unavailable".into()); }
    let boxes=provider.client.get("Mailbox",Some(&mailbox_ids),None).map_err(|_| "Email mailbox rights unavailable")?;
    if !list(&boxes,"notFound")?.is_empty() { return Err("Email mailbox rights unavailable".into()); }
    let values=boxes["list"].as_array().ok_or("Email mailbox rights unavailable")?;
    let expected:std::collections::HashSet<&str>=mailbox_ids.iter().map(String::as_str).collect();
    let mut seen=std::collections::HashSet::new();
    for value in values {
        let id=value["id"].as_str().ok_or("Invalid mailbox identity")?;
        if !expected.contains(id) || !seen.insert(id) || value.pointer("/myRights/maySetSeen").and_then(Value::as_bool)!=Some(true) { return Err("Email seen flag is not writable".into()); }
    }
    if seen.len()!=expected.len() { return Err("Email mailbox rights unavailable".into()); }
    let state=field(&email,"state")?;
    if !store.claim_provider_target_mutation(&scope,operation_id,"mail_set_read",email_id,&intent,&digest).map_err(|e| e.to_string())? { return Ok(SendOutcome::Unknown); }
    let patch=serde_json::json!({"keywords/$seen":if is_read { Some(true) } else { None }});
    match provider.client.update_email(email_id,state,&patch) {
        Ok(EmailUpdateResult::Applied) => {
            if store.finish_provider_mutation(&scope,operation_id,Some(email_id)).map_err(|e| e.to_string())? { Ok(SendOutcome::Confirmed(email_id.into())) } else { Ok(SendOutcome::Unknown) }
        }
        Ok(EmailUpdateResult::Rejected) => {
            if store.finish_provider_mutation(&scope,operation_id,None).map_err(|e| e.to_string())? { Ok(SendOutcome::Rejected) } else { Ok(SendOutcome::Unknown) }
        }
        _ => Ok(SendOutcome::Unknown),
    }
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
        "cc",
        "bcc",
        "header:Importance:asText",
        "header:Disposition-Notification-To:asAddresses",
        "bodyValues",
        "htmlBody",
        "inReplyTo",
        "references",
        "attachments",
    ];
    if email
        .as_object()
        .is_none_or(|fields| fields.keys().any(|key| !allowed.contains(&key.as_str())))
    {
        return Err("Unsupported draft fields".into());
    }
    for key in ["inReplyTo", "references"] {
        if let Some(value) = email.get(key) {
            let ids = value.as_array().ok_or("Invalid message references")?;
            if ids.is_empty() || ids.len() > 100 || ids.iter().any(|v| v.as_str().is_none_or(|s| s.is_empty() || s.len() > 998 || s.chars().any(char::is_control))) { return Err("Invalid message references".into()); }
        }
    }
    if let Some(value) = email.get("attachments") {
        let attachments = value.as_array().ok_or("Invalid attachments")?;
        if attachments.len() > 32 { return Err("Too many attachments".into()); }
        for part in attachments {
            let fields = part.as_object().ok_or("Invalid attachment")?;
            if fields.keys().any(|key| !matches!(key.as_str(), "blobId" | "type" | "name" | "disposition"))
                || part["blobId"].as_str().is_none_or(|s| s.is_empty() || s.len() > 1024 || s.chars().any(char::is_control))
                || part["name"].as_str().is_none_or(|s| s.is_empty() || s.len() > 255 || s.chars().any(char::is_control))
                || part["type"].as_str().is_none_or(|s| s.is_empty() || s.len() > 255 || !s.is_ascii() || s.chars().any(|c| c.is_control() || c.is_whitespace()))
                || part["disposition"].as_str() != Some("attachment") { return Err("Invalid attachment".into()); }
        }
    }
    let subject = email["subject"].as_str().ok_or("Draft subject required")?;
    let to = draft_addresses(email,"to",true)?;
    let cc = draft_addresses(email,"cc",false)?;
    let bcc = draft_addresses(email,"bcc",false)?;
    if subject.contains('\r')
        || subject.contains('\n')
        || to.len()+cc.len()+bcc.len() > 1000
    {
        return Err("Invalid draft recipients or subject".into());
    }
    if let Some(importance) = email.get("header:Importance:asText") {
        if !matches!(importance.as_str(),Some("low"|"normal"|"high")) { return Err("Invalid draft importance".into()); }
    }
    let receipt = draft_addresses(email,"header:Disposition-Notification-To:asAddresses",false)?;
    if receipt.len()>1 { return Err("Invalid read receipt address".into()); }
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
    if let Some(address) = receipt.first() {
        let identities = provider.client.get("Identity",None,None).map_err(|_| "Sending identities unavailable")?;
        let identities = identities["list"].as_array().ok_or("Sending identities unavailable")?;
        if !identities.iter().any(|identity| identity["email"].as_str()==Some(address.as_str())) { return Err("Read receipt address is not an authorized sending identity".into()); }
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

// Inspect native creations after a lost response; without a native correlation
// key even a matching creation cannot prove which client submitted it.
fn reconcile_submission(
    _store: &Store,
    _scope: &ProviderScope,
    client: &JmapClient,
    op: &ProviderSubmission,
) -> Result<SendOutcome, String> {
    let intent: Value = serde_json::from_str(&op.intent_json).map_err(|_| "Invalid recorded submission")?;
    let Some(mut state) = intent.get("submission_state").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned) else {
        return Ok(SendOutcome::Unknown);
    };
    let mut created = Vec::new();
    for page in 0..100 {
        let Ok(changes) = client.changes("EmailSubmission", &state, 100) else { return Ok(SendOutcome::Unknown) };
        if changes["oldState"].as_str() != Some(state.as_str()) { return Ok(SendOutcome::Unknown); }
        let Some(next) = changes["newState"].as_str().filter(|s| !s.is_empty()) else { return Ok(SendOutcome::Unknown) };
        let Ok(ids) = list(&changes, "created") else { return Ok(SendOutcome::Unknown) };
        // A destroyed submission may have been ours, so another matching
        // creation cannot establish an unambiguous result in that interval.
        match list(&changes, "destroyed") {
            Ok(ids) if ids.is_empty() => {}
            _ => return Ok(SendOutcome::Unknown),
        }
        created.extend(ids);
        if created.len() > 10_000 { return Ok(SendOutcome::Unknown); }
        match changes["hasMoreChanges"].as_bool() {
            Some(false) => break,
            Some(true) if next != state => state = next.to_owned(),
            _ => return Ok(SendOutcome::Unknown),
        }
        if page == 99 { return Ok(SendOutcome::Unknown); }
    }
    let mut matches = Vec::new();
    for ids in created.chunks(client.max_objects_in_get().max(1)) {
        let Ok(response) = client.get_submissions(Some(ids)) else { return Ok(SendOutcome::Unknown) };
        let Some(rows) = response["list"].as_array() else { return Ok(SendOutcome::Unknown) };
        if rows.len() != ids.len() { return Ok(SendOutcome::Unknown); }
        for row in rows {
            let Some(id) = row["id"].as_str().filter(|id| ids.iter().any(|v| v == *id)) else { return Ok(SendOutcome::Unknown) };
            let Some(recipients) = row.pointer("/envelope/rcptTo").and_then(Value::as_array) else { return Ok(SendOutcome::Unknown) };
            let mut addresses: Vec<&str> = recipients.iter().filter_map(|r| r["email"].as_str()).collect();
            let Some(expected) = intent["recipients"].as_array() else { return Ok(SendOutcome::Unknown) };
            let mut expected_addresses: Vec<&str> = expected.iter().filter_map(Value::as_str).collect();
            if addresses.len() != recipients.len() || expected_addresses.len() != expected.len() { return Ok(SendOutcome::Unknown); }
            addresses.sort_unstable(); expected_addresses.sort_unstable();
            if row["emailId"].as_str() == Some(op.draft_id.as_str())
                && row["identityId"].as_str() == Some(op.identity_id.as_str())
                && row.pointer("/envelope/mailFrom/email") == intent.get("mail_from")
                && addresses == expected_addresses {
                matches.push(id.to_owned());
            }
        }
    }
    // Another client can submit the same immutable email and envelope in the
    // observed interval. Even one matching creation does not prove ownership.
    // Do not attribute this observation to us.
    let _ = matches;
    Ok(SendOutcome::Unknown)
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
        || approved.remote_account.is_empty()
        || approved.remote_account.len() > 256
        || approved.remote_account.chars().any(char::is_control)
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
    with_approved_submission_scope(&approved.remote_account, &scope, || {
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
            _ => return reconcile_submission(store, &scope, client, &existing),
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
    let mut approved_recipients = approved.recipients.clone();
    approved_recipients.sort();
    approved_recipients.dedup();
    if native_recipients(&rows[0])? != approved_recipients {
        return Err("Approved recipients do not match the native draft".into());
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
    let mut recorded_intent: Value = serde_json::from_str(&intent).map_err(|_| "Invalid submission")?;
    recorded_intent["submission_state"] = Value::String(submission_state.clone());
    let op = ProviderSubmission {
        scope: scope.clone(),
        operation_id: approved.operation_id.clone(),
        draft_id: approved.email_id.clone(),
        identity_id: approved.identity_id.clone(),
        email_state: field(&email, "state")?.into(),
        snapshot_digest: digest,
        creation_id: creation_id.clone(),
        intent_json: serde_json::to_string(&recorded_intent).map_err(|_| "Invalid submission")?,
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
    })
}

fn with_approved_submission_scope<T>(approved: &str, scope: &ProviderScope, dispatch: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    if approved != scope.collection { return Err("Approved native account changed".into()); }
    dispatch()
}

#[cfg(test)]
mod submission_account_binding_tests {
    use super::*;

    #[test]
    fn changed_primary_account_at_same_origin_is_rejected() {
        let mut scope = ProviderScope { account_id: "local".into(), origin: "https://example.invalid".into(), service: "mail".into(), collection: "primary-before".into() };
        let mut claims = 0;
        let mut submissions = 0;
        assert!(with_approved_submission_scope("primary-before", &scope, || { claims += 1; submissions += 1; Ok(()) }).is_ok());
        assert_eq!((claims, submissions), (1, 1));
        scope.collection = "primary-after".into();
        assert_eq!(with_approved_submission_scope("primary-before", &scope, || { claims += 1; submissions += 1; Ok(()) }).unwrap_err(), "Approved native account changed");
        assert_eq!((claims, submissions), (1, 1));
    }
}
