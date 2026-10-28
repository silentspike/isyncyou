//! JMAP mail transport. Store checkpoints and mutation intents belong to the caller.

use reqwest::blocking::{Client, Response};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::Duration;
use url::Url;

const CORE: &str = "urn:ietf:params:jmap:core";
const MAIL: &str = "urn:ietf:params:jmap:mail";
const SUBMISSION: &str = "urn:ietf:params:jmap:submission";
const VAPID: &str = "urn:ietf:params:jmap:webpush-vapid";
const MAX_JSON: u64 = 16 * 1024 * 1024;
const MAX_BLOB: u64 = 64 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum JmapError {
    InvalidUrl,
    InvalidResponse,
    Unsupported,
    Http(u16),
    Method(String),
    Transport,
    /// The request may have reached the server. Never blindly repeat a mutation.
    OutcomeUnknown,
}

impl std::fmt::Display for JmapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for JmapError {}

/// Native server response for the single requested submission.
#[derive(Debug, PartialEq, Eq)]
pub enum SubmissionResult {
    Created {
        id: String,
        email_update: EmailUpdateResult,
    },
    Rejected {
        kind: String,
    },
}
#[derive(Debug, PartialEq, Eq)]
pub enum EmailUpdateResult {
    Applied,
    Rejected,
    Unknown,
}

/// Session state must be compared to a fresh discovery after registration.
pub struct PushMutation {
    pub session_state: String,
    pub outcome: PushOutcome,
}
pub enum PushOutcome {
    Applied { id: String, expires: Option<String> },
    Rejected { kind: String },
}

pub enum MailboxCreateResult {
    Created { id: String },
    Rejected { kind: String },
}

/// Read-only API-key client for a local exporter. No writable client is exposed.
pub struct LoopbackJmapReader(JmapClient);
impl LoopbackJmapReader {
    pub fn discover(origin: &str, api_key: String, trusted_public_origin: &str) -> Result<Self, JmapError> {
        JmapClient::discover_inner(origin, api_key, Some(trusted_public_origin)).map(Self)
    }
    pub fn account_id(&self) -> &str { self.0.account_id() }
    pub fn max_objects_in_get(&self) -> usize { self.0.max_objects_in_get() }
    pub fn get(&self, kind: &str, ids: Option<&[String]>, properties: Option<&[String]>) -> Result<Value, JmapError> {
        if !matches!(kind, "Email" | "Mailbox") { return Err(JmapError::Unsupported); }
        self.0.get(kind, ids, properties)
    }
    pub fn query_emails(&self, position: usize, limit: usize, filter: Option<Value>) -> Result<Value, JmapError> { self.0.query_emails(position, limit, filter) }
    pub fn query_mailboxes(&self, position: usize, limit: usize) -> Result<Value, JmapError> { self.0.query_mailboxes(position, limit) }
    pub fn changes(&self, kind: &str, since: &str, max: usize) -> Result<Value, JmapError> { self.0.changes(kind, since, max) }
    pub fn download(&self, blob_id: &str, name: &str, media_type: &str) -> Result<Vec<u8>, JmapError> { self.0.download(blob_id, name, media_type) }
}

struct Counts {
    requests: usize,
    attachments: usize,
    request_limit: usize,
    attachment_limit: usize,
}
impl Default for Counts {
    fn default() -> Self {
        Self {
            requests: 0,
            attachments: 0,
            request_limit: 4,
            attachment_limit: 2,
        }
    }
}
#[derive(Default)]
struct Gate {
    counts: Mutex<Counts>,
    changed: Condvar,
}
struct Permit<'a> {
    gate: &'a Gate,
    attachment: bool,
}
impl Gate {
    fn acquire(&self, attachment: bool) -> Permit<'_> {
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        while counts.requests >= counts.request_limit
            || (attachment && counts.attachments >= counts.attachment_limit)
        {
            counts = self.changed.wait(counts).unwrap_or_else(|e| e.into_inner());
        }
        counts.requests += 1;
        if attachment {
            counts.attachments += 1;
        }
        Permit {
            gate: self,
            attachment,
        }
    }
}
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut counts = self.gate.counts.lock().unwrap_or_else(|e| e.into_inner());
        counts.requests -= 1;
        if self.attachment {
            counts.attachments -= 1;
        }
        self.gate.changed.notify_all();
    }
}
fn account_gate(key: &str, request_limit: usize, attachment_limit: usize) -> Arc<Gate> {
    static GATES: OnceLock<Mutex<HashMap<String, Weak<Gate>>>> = OnceLock::new();
    let mut gates = GATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    gates.retain(|_, v| v.strong_count() > 0);
    let gate = gates.get(key).and_then(Weak::upgrade).unwrap_or_else(|| {
        let gate = Arc::new(Gate::default());
        gates.insert(key.to_owned(), Arc::downgrade(&gate));
        gate
    });
    {
        let mut counts = gate.counts.lock().unwrap_or_else(|e| e.into_inner());
        counts.request_limit = counts.request_limit.min(request_limit);
        counts.attachment_limit = counts.attachment_limit.min(attachment_limit);
    }
    gate
}

fn body(response: Response, max: u64) -> Result<Vec<u8>, JmapError> {
    let mut bytes = Vec::new();
    response
        .take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| JmapError::Transport)?;
    if bytes.len() as u64 > max {
        return Err(JmapError::InvalidResponse);
    }
    Ok(bytes)
}
fn endpoint(origin: &Url, value: &str) -> Result<Url, JmapError> {
    let url = origin.join(value).map_err(|_| JmapError::InvalidUrl)?;
    if url.origin() != origin.origin()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(JmapError::InvalidUrl);
    }
    Ok(url)
}
fn allowed_origin(origin: &Url, loopback: bool) -> bool {
    if loopback {
        origin.scheme() == "http" && (matches!(origin.host(), Some(url::Host::Ipv4(ip)) if ip == std::net::Ipv4Addr::LOCALHOST)
            || matches!(origin.host(), Some(url::Host::Ipv6(ip)) if ip == std::net::Ipv6Addr::LOCALHOST))
    } else { origin.scheme() == "https" }
}
fn rewrite_loopback_endpoint(local: &Url, public: &Url, value: &str) -> Result<String, JmapError> {
    let vars = [("{accountId}", "JMAPVARACCOUNT"), ("{blobId}", "JMAPVARBLOB"), ("{name}", "JMAPVARNAME"), ("{type}", "JMAPVARTYPE")];
    let mut encoded = value.to_owned();
    for (var, marker) in vars { if encoded.contains(marker) { return Err(JmapError::InvalidUrl); } encoded = encoded.replace(var, marker); }
    let url = local.join(&encoded).map_err(|_| JmapError::InvalidUrl)?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() || (url.origin() != local.origin() && url.origin() != public.origin()) { return Err(JmapError::InvalidUrl); }
    let mut mapped = local.clone();
    mapped.set_path(url.path());
    mapped.set_query(url.query());
    let mut result = mapped.to_string();
    for (var, marker) in vars { result = result.replace(marker, var); }
    Ok(result)
}
fn nonempty(value: Option<&Value>) -> Result<&str, JmapError> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(JmapError::InvalidResponse)
}

/// Credentials are private and deliberately excluded from Debug and error text.
pub struct JmapClient {
    http: Client,
    origin: Url,
    token: String,
    account: String,
    api: Url,
    download: String,
    upload: String,
    submission: bool,
    gate: Arc<Gate>,
    max_get: usize,
    max_request: usize,
    max_upload: u64,
    session_state: String,
    vapid_public_key: Option<String>,
}

impl JmapClient {
    pub fn discover(origin: &str, token: String) -> Result<Self, JmapError> {
        Self::discover_inner(origin, token, None)
    }
    fn discover_inner(origin: &str, token: String, trusted_public_origin: Option<&str>) -> Result<Self, JmapError> {
        let loopback = trusted_public_origin.is_some();
        let origin = Url::parse(origin).map_err(|_| JmapError::InvalidUrl)?;
        if !allowed_origin(&origin, loopback)
            || origin.host_str().is_none()
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
            || token.is_empty()
        {
            return Err(JmapError::InvalidUrl);
        }
        let public = if let Some(value) = trusted_public_origin {
            let url = Url::parse(value).map_err(|_| JmapError::InvalidUrl)?;
            if !allowed_origin(&url, false) || url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() || url.path() != "/" || url.query().is_some() || url.fragment().is_some() { return Err(JmapError::InvalidUrl); }
            Some(url)
        } else { None };
        let mut builder = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(60));
        if loopback { builder = builder.no_proxy(); }
        let http = builder.build()
            .map_err(|_| JmapError::Transport)?;
        let response = http
            .get(endpoint(&origin, "/.well-known/jmap")?)
            .bearer_auth(&token)
            .send()
            .map_err(|_| JmapError::Transport)?;
        if !response.status().is_success() {
            return Err(JmapError::Http(response.status().as_u16()));
        }
        let session: Value = serde_json::from_slice(&body(response, MAX_JSON)?)
            .map_err(|_| JmapError::InvalidResponse)?;
        let capabilities = session
            .get("capabilities")
            .and_then(Value::as_object)
            .ok_or(JmapError::InvalidResponse)?;
        let session_state = nonempty(session.get("state"))?.to_owned();
        let vapid_public_key = match capabilities.get(VAPID) {
            Some(capability) => Some(nonempty(capability.get("applicationServerKey"))?.to_owned()),
            None => None,
        };
        if !capabilities.contains_key(CORE) || !capabilities.contains_key(MAIL) {
            return Err(JmapError::Unsupported);
        }
        let core = capabilities.get(CORE).ok_or(JmapError::InvalidResponse)?;
        let limit = |name: &str| -> Result<u64, JmapError> {
            core.get(name)
                .and_then(Value::as_u64)
                .filter(|n| *n > 0)
                .ok_or(JmapError::InvalidResponse)
        };
        let request_limit = limit("maxConcurrentRequests")?.min(4) as usize;
        let attachment_limit = limit("maxConcurrentUpload")?.min(2) as usize;
        let max_get = limit("maxObjectsInGet")?.min(usize::MAX as u64) as usize;
        let max_request = limit("maxSizeRequest")?.min(MAX_JSON) as usize;
        let max_upload = limit("maxSizeUpload")?.min(MAX_BLOB);
        if limit("maxCallsInRequest")? < 1 || limit("maxObjectsInSet")? < 1 {
            return Err(JmapError::Unsupported);
        }
        let account =
            nonempty(session.pointer("/primaryAccounts/urn:ietf:params:jmap:mail"))?.to_owned();
        let account_caps = session
            .get("accounts")
            .and_then(|v| v.get(&account))
            .and_then(|v| v.get("accountCapabilities"))
            .and_then(Value::as_object)
            .ok_or(JmapError::InvalidResponse)?;
        if !account_caps.contains_key(MAIL) {
            return Err(JmapError::Unsupported);
        }
        let submission =
            capabilities.contains_key(SUBMISSION) && account_caps.contains_key(SUBMISSION);
        let map = |value: &str| -> Result<String, JmapError> { if let Some(public) = &public { rewrite_loopback_endpoint(&origin, public, value) } else { Ok(value.to_owned()) } };
        let api = endpoint(&origin, &map(nonempty(session.get("apiUrl"))?)?)?;
        let download = map(nonempty(session.get("downloadUrl"))?)?;
        let upload = map(nonempty(session.get("uploadUrl"))?)?;
        // Validate templates before retaining them; substitute safe placeholders.
        endpoint(
            &origin,
            &download
                .replace("{accountId}", "a")
                .replace("{blobId}", "b")
                .replace("{name}", "n")
                .replace("{type}", "t"),
        )?;
        endpoint(&origin, &upload.replace("{accountId}", "a"))?;
        let gate = account_gate(
            &format!("{}|{}", origin.origin().ascii_serialization(), account),
            request_limit,
            attachment_limit,
        );
        Ok(Self {
            http,
            origin,
            token,
            account,
            api,
            download,
            upload,
            submission,
            gate,
            max_get,
            max_request,
            max_upload,
            session_state,
            vapid_public_key,
        })
    }
    pub fn account_id(&self) -> &str {
        &self.account
    }
    pub fn origin(&self) -> &str {
        self.origin.as_str()
    }
    pub fn supports_submission(&self) -> bool {
        self.submission
    }
    pub fn max_objects_in_get(&self) -> usize {
        self.max_get
    }
    pub fn session_state(&self) -> &str {
        &self.session_state
    }
    pub fn vapid_public_key(&self) -> Option<&str> {
        self.vapid_public_key.as_deref()
    }

    fn call_payload(
        &self,
        method: &str,
        args: Value,
        mutation: bool,
        submission: bool,
    ) -> Result<Value, JmapError> {
        let _permit = self.gate.acquire(false);
        let mut using = vec![CORE, MAIL];
        if submission {
            if !self.submission {
                return Err(JmapError::Unsupported);
            }
            using.push(SUBMISSION);
        }
        let request = json!({"using":using,"methodCalls":[[method,args,"c1"]]});
        if serde_json::to_vec(&request)
            .map_err(|_| JmapError::InvalidResponse)?
            .len()
            > self.max_request
        {
            return Err(JmapError::InvalidResponse);
        }
        let response = self
            .http
            .post(self.api.clone())
            .bearer_auth(&self.token)
            .json(&request)
            .send()
            .map_err(|_| {
                if mutation {
                    JmapError::OutcomeUnknown
                } else {
                    JmapError::Transport
                }
            })?;
        if !response.status().is_success() {
            return Err(if mutation && response.status().is_server_error() {
                JmapError::OutcomeUnknown
            } else {
                JmapError::Http(response.status().as_u16())
            });
        }
        let raw = body(response, MAX_JSON).map_err(|e| {
            if mutation {
                JmapError::OutcomeUnknown
            } else {
                e
            }
        })?;
        serde_json::from_slice(&raw).map_err(|_| {
            if mutation {
                JmapError::OutcomeUnknown
            } else {
                JmapError::InvalidResponse
            }
        })
    }
    fn call(
        &self,
        method: &str,
        args: Value,
        mutation: bool,
        submission: bool,
    ) -> Result<Value, JmapError> {
        let payload = self.call_payload(method, args, mutation, submission)?;
        let result = parse_method(&payload, method).map_err(|e| {
            if mutation
                && (e == JmapError::InvalidResponse
                    || e == JmapError::Method("serverPartialFail".into()))
            {
                JmapError::OutcomeUnknown
            } else {
                e
            }
        })?;
        if result.get("accountId").and_then(Value::as_str) != Some(self.account.as_str()) {
            return Err(if mutation {
                JmapError::OutcomeUnknown
            } else {
                JmapError::InvalidResponse
            });
        }
        Ok(result)
    }

    /// `kind` is Email, Mailbox, Identity or EmailSubmission; IDs and raw objects
    /// remain native so the caller can retain mailbox membership as many-to-many.
    pub fn get(
        &self,
        kind: &str,
        ids: Option<&[String]>,
        properties: Option<&[String]>,
    ) -> Result<Value, JmapError> {
        if !matches!(kind, "Email" | "Mailbox" | "Identity" | "EmailSubmission") {
            return Err(JmapError::Unsupported);
        }
        if ids.is_some_and(|values| values.len() > self.max_get) {
            return Err(JmapError::InvalidResponse);
        }
        self.call(
            &format!("{kind}/get"),
            json!({"accountId":self.account,"ids":ids,"properties":properties}),
            false,
            matches!(kind, "Identity" | "EmailSubmission"),
        )
    }
    pub fn query_emails(
        &self,
        position: usize,
        limit: usize,
        filter: Option<Value>,
    ) -> Result<Value, JmapError> {
        if limit == 0 || limit > 1000 {
            return Err(JmapError::InvalidResponse);
        }
        let mut args = json!({"accountId":self.account,"position":position,"limit":limit,"calculateTotal":true});
        if let Some(filter) = filter {
            args["filter"] = filter;
        }
        self.call("Email/query", args, false, false)
    }
    pub fn query_mailboxes(&self, position: usize, limit: usize) -> Result<Value, JmapError> {
        if limit == 0 || limit > 1000 {
            return Err(JmapError::InvalidResponse);
        }
        self.call("Mailbox/query", json!({"accountId":self.account,"position":position,"limit":limit,"calculateTotal":true}), false, false)
    }
    pub fn changes(&self, kind: &str, since: &str, max: usize) -> Result<Value, JmapError> {
        if !matches!(kind, "Email" | "Mailbox") || since.is_empty() || max == 0 || max > 1000 {
            return Err(JmapError::InvalidResponse);
        }
        self.call(
            &format!("{kind}/changes"),
            json!({"accountId":self.account,"sinceState":since,"maxChanges":max}),
            false,
            false,
        )
    }
    pub fn query_changes(
        &self,
        since: &str,
        filter: Option<Value>,
        max: usize,
    ) -> Result<Value, JmapError> {
        if since.is_empty() || max == 0 || max > 1000 {
            return Err(JmapError::InvalidResponse);
        }
        let mut args = json!({"accountId":self.account,"sinceQueryState":since,"maxChanges":max});
        if let Some(filter) = filter {
            args["filter"] = filter;
        }
        self.call("Email/queryChanges", args, false, false)
    }
    /// Fetch only requested messages, with the server's text/html body values.
    /// Truncated values are reported by JMAP in the returned native bodyValues.
    pub fn get_email_bodies(
        &self,
        ids: &[String],
        max_body_value_bytes: usize,
    ) -> Result<Value, JmapError> {
        if ids.is_empty()
            || ids.len() > self.max_get
            || max_body_value_bytes == 0
            || max_body_value_bytes > 16 * 1024 * 1024
        {
            return Err(JmapError::InvalidResponse);
        }
        self.call("Email/get", json!({"accountId":self.account,"ids":ids,"fetchTextBodyValues":true,"fetchHTMLBodyValues":true,"maxBodyValueBytes":max_body_value_bytes}), false, false)
    }
    pub fn query_submissions(
        &self,
        position: usize,
        limit: usize,
        email_id: &str,
    ) -> Result<Value, JmapError> {
        if email_id.is_empty() || limit == 0 || limit > 1000 {
            return Err(JmapError::InvalidResponse);
        }
        self.call("EmailSubmission/query", json!({"accountId":self.account,"position":position,"limit":limit,"filter":{"emailIds":[email_id]}}), false, true)
    }
    pub fn create_draft(&self, creation_id: &str, email: Value) -> Result<Value, JmapError> {
        if creation_id.is_empty() || !email.is_object() {
            return Err(JmapError::InvalidResponse);
        }
        self.call(
            "Email/set",
            json!({"accountId":self.account,"create":{creation_id:email}}),
            true,
            false,
        )
    }
    pub fn import_email(
        &self,
        creation_id: &str,
        blob_id: &str,
        mailbox_ids: Value,
    ) -> Result<Value, JmapError> {
        if creation_id.is_empty() || blob_id.is_empty() || !mailbox_ids.is_object() {
            return Err(JmapError::InvalidResponse);
        }
        self.call("Email/import", json!({"accountId":self.account,"emails":{creation_id:{"blobId":blob_id,"mailboxIds":mailbox_ids}}}), true, false)
    }
    /// The caller records the intent before dispatch and reconciles unknown outcomes.
    pub fn create_mailbox(&self, creation_id: &str, name: &str, if_in_state: &str) -> Result<MailboxCreateResult, JmapError> {
        if creation_id.is_empty() || name.is_empty() || if_in_state.is_empty() { return Err(JmapError::InvalidResponse); }
        let response = self.call("Mailbox/set", json!({"accountId":self.account,"ifInState":if_in_state,"create":{creation_id:{"name":name}}}), true, false)?;
        let created = response.get("created").and_then(|v| v.get(creation_id));
        let rejected = response.get("notCreated").and_then(|v| v.get(creation_id));
        match (created, rejected) {
            (Some(value), None) => Ok(MailboxCreateResult::Created { id: nonempty(value.get("id")).map_err(|_| JmapError::OutcomeUnknown)?.to_owned() }),
            (None, Some(value)) => {
                let kind = nonempty(value.get("type")).map_err(|_| JmapError::OutcomeUnknown)?;
                if kind.len() > 80 || !kind.bytes().all(|c| c.is_ascii_alphanumeric()) { return Err(JmapError::OutcomeUnknown); }
                Ok(MailboxCreateResult::Rejected { kind: kind.to_owned() })
            }
            _ => Err(JmapError::OutcomeUnknown),
        }
    }
    /// Change only mailbox memberships and keywords against an observed Email state.
    /// The caller owns the durable intent, authorization and unknown reconciliation.
    pub fn update_email(&self, email_id: &str, if_in_state: &str, patch: &Value) -> Result<EmailUpdateResult, JmapError> {
        let entries = patch.as_object().filter(|v| !v.is_empty() && v.len() <= 128).ok_or(JmapError::InvalidResponse)?;
        if email_id.is_empty() || if_in_state.is_empty() || entries.iter().any(|(key, value)| {
            let part = key.strip_prefix("keywords/").or_else(|| key.strip_prefix("mailboxIds/"));
            let valid_path = part.is_some_and(|s| !s.is_empty() && !s.contains('/') && valid_pointer_token(s));
            !valid_path || !(value.is_null() || value.as_bool() == Some(true))
        }) { return Err(JmapError::InvalidResponse); }
        let result = self.call("Email/set", json!({"accountId":self.account,"ifInState":if_in_state,"update":{email_id:patch}}), true, false)?;
        let updated = result.get("updated").and_then(|v| v.get(email_id));
        let rejected = result.get("notUpdated").and_then(|v| v.get(email_id));
        Ok(match (updated, rejected) {
            (Some(value), None) if value.is_null() || value.is_object() => EmailUpdateResult::Applied,
            (None, Some(value)) if value.is_object() => EmailUpdateResult::Rejected,
            _ => EmailUpdateResult::Unknown,
        })
    }
    /// Caller must persist the exact approved intent before invoking this once.
    /// Unknown outcomes require native reconciliation, never automatic retry.
    pub fn submit(
        &self,
        creation_id: &str,
        email_id: &str,
        identity_id: &str,
        if_in_state: &str,
        mail_from: &str,
        recipients: &[String],
        draft_mailbox_id: &str,
        sent_mailbox_id: &str,
    ) -> Result<SubmissionResult, JmapError> {
        if [
            creation_id,
            email_id,
            identity_id,
            if_in_state,
            mail_from,
            draft_mailbox_id,
            sent_mailbox_id,
        ]
        .iter()
        .any(|s| s.is_empty())
            || draft_mailbox_id == sent_mailbox_id
            || recipients.is_empty()
            || recipients.len() > 1000
            || recipients
                .iter()
                .any(|s| s.is_empty() || s.len() > 320 || s.contains('\r') || s.contains('\n'))
            || mail_from.len() > 320
            || mail_from.contains('\r')
            || mail_from.contains('\n')
        {
            return Err(JmapError::InvalidResponse);
        }
        let rcpt: Vec<Value> = recipients
            .iter()
            .map(|address| json!({"email":address}))
            .collect();
        let mut patch = serde_json::Map::new();
        patch.insert(
            format!("mailboxIds/{}", pointer_escape(draft_mailbox_id)),
            Value::Null,
        );
        patch.insert(
            format!("mailboxIds/{}", pointer_escape(sent_mailbox_id)),
            json!(true),
        );
        patch.insert("keywords/$draft".into(), Value::Null);
        let reference = format!("#{creation_id}");
        let payload = self.call_payload("EmailSubmission/set", json!({"accountId":self.account,"ifInState":if_in_state,"create":{creation_id:{"emailId":email_id,"identityId":identity_id,"envelope":{"mailFrom":{"email":mail_from},"rcptTo":rcpt}}},"onSuccessUpdateEmail":{reference:patch}}), true, true)?;
        parse_submission(&payload, &self.account, creation_id, email_id)
    }
    pub fn get_submissions(&self, ids: Option<&[String]>) -> Result<Value, JmapError> {
        self.get("EmailSubmission", ids, None)
    }

    /// Returns the native session-scoped response; it can contain private push
    /// URLs and verification codes and must never be logged.
    pub fn push_get(&self, ids: Option<&[String]>) -> Result<Value, JmapError> {
        if ids.is_some_and(|v| v.len() > self.max_get) {
            return Err(JmapError::InvalidResponse);
        }
        let payload =
            self.call_payload("PushSubscription/get", json!({"ids":ids}), false, false)?;
        parse_method(&payload, "PushSubscription/get")?;
        Ok(payload)
    }
    /// The caller persists an intent and reconciles a lost response before any retry.
    pub fn push_create(
        &self,
        creation_id: &str,
        device_client_id: &str,
        endpoint_url: &str,
        p256dh: &str,
        auth: &str,
        types: Option<&[String]>,
        expires: Option<&str>,
    ) -> Result<PushMutation, JmapError> {
        let endpoint_url_parsed = Url::parse(endpoint_url).map_err(|_| JmapError::InvalidUrl)?;
        if endpoint_url_parsed.scheme() != "https"
            || endpoint_url_parsed.host_str().is_none()
            || !endpoint_url_parsed.username().is_empty()
            || endpoint_url_parsed.password().is_some()
            || endpoint_url_parsed.fragment().is_some()
            || [creation_id, device_client_id, p256dh, auth]
                .iter()
                .any(|v| v.is_empty())
        {
            return Err(JmapError::InvalidResponse);
        }
        let mut object = json!({"deviceClientId":device_client_id,"url":endpoint_url,"keys":{"p256dh":p256dh,"auth":auth},"types":types});
        if let Some(expires) = expires {
            object["expires"] = json!(expires);
        }
        self.push_set(
            json!({"create":{creation_id:object}}),
            "created",
            "notCreated",
            creation_id,
            None,
        )
    }
    pub fn push_verify(&self, id: &str, code: &str) -> Result<PushMutation, JmapError> {
        if id.is_empty() || code.is_empty() {
            return Err(JmapError::InvalidResponse);
        }
        self.push_set(
            json!({"update":{id:{"verificationCode":code}}}),
            "updated",
            "notUpdated",
            id,
            Some(id),
        )
    }
    pub fn push_renew(&self, id: &str, expires: &str) -> Result<PushMutation, JmapError> {
        if id.is_empty() || expires.is_empty() {
            return Err(JmapError::InvalidResponse);
        }
        self.push_set(
            json!({"update":{id:{"expires":expires}}}),
            "updated",
            "notUpdated",
            id,
            Some(id),
        )
    }
    pub fn push_destroy(&self, id: &str) -> Result<PushMutation, JmapError> {
        if id.is_empty() {
            return Err(JmapError::InvalidResponse);
        }
        self.push_set(
            json!({"destroy":[id]}),
            "destroyed",
            "notDestroyed",
            id,
            Some(id),
        )
    }
    fn push_set(
        &self,
        args: Value,
        success: &str,
        failure: &str,
        key: &str,
        existing_id: Option<&str>,
    ) -> Result<PushMutation, JmapError> {
        let payload = self.call_payload("PushSubscription/set", args, true, false)?;
        parse_push_mutation(&payload, success, failure, key, existing_id)
    }

    pub fn download(
        &self,
        blob_id: &str,
        name: &str,
        media_type: &str,
    ) -> Result<Vec<u8>, JmapError> {
        let _permit = self.gate.acquire(true);
        let template = self
            .download
            .replace("{accountId}", &encode(&self.account))
            .replace("{blobId}", &encode(blob_id))
            .replace("{name}", &encode(name))
            .replace("{type}", &encode(media_type));
        let response = self
            .http
            .get(endpoint(&self.origin, &template)?)
            .bearer_auth(&self.token)
            .send()
            .map_err(|_| JmapError::Transport)?;
        if !response.status().is_success() {
            return Err(JmapError::Http(response.status().as_u16()));
        }
        body(response, MAX_BLOB)
    }
    pub fn upload(&self, bytes: Vec<u8>, media_type: &str) -> Result<Value, JmapError> {
        if bytes.len() as u64 > self.max_upload {
            return Err(JmapError::InvalidResponse);
        }
        let _permit = self.gate.acquire(true);
        let template = self.upload.replace("{accountId}", &encode(&self.account));
        let response = self
            .http
            .post(endpoint(&self.origin, &template)?)
            .bearer_auth(&self.token)
            .header(reqwest::header::CONTENT_TYPE, media_type)
            .body(bytes)
            .send()
            .map_err(|_| JmapError::OutcomeUnknown)?;
        if !response.status().is_success() {
            return Err(if response.status().is_server_error() {
                JmapError::OutcomeUnknown
            } else {
                JmapError::Http(response.status().as_u16())
            });
        }
        let raw = body(response, MAX_JSON).map_err(|_| JmapError::OutcomeUnknown)?;
        serde_json::from_slice(&raw).map_err(|_| JmapError::OutcomeUnknown)
    }
}
fn encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
fn pointer_escape(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}
fn valid_pointer_token(value: &str) -> bool {
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'~' && !matches!(bytes.next(), Some(b'0' | b'1')) { return false; }
    }
    true
}
fn parse_push_mutation(
    payload: &Value,
    success: &str,
    failure: &str,
    key: &str,
    existing_id: Option<&str>,
) -> Result<PushMutation, JmapError> {
    let session_state = nonempty(payload.get("sessionState"))
        .map_err(|_| JmapError::OutcomeUnknown)?
        .to_owned();
    let result =
        parse_method(payload, "PushSubscription/set").map_err(|_| JmapError::OutcomeUnknown)?;
    let rejected = result.get(failure).and_then(|v| v.get(key));
    let applied = if success == "destroyed" {
        result
            .get(success)
            .and_then(Value::as_array)
            .and_then(|ids| ids.iter().find(|v| v.as_str() == Some(key)))
    } else {
        result.get(success).and_then(|v| v.get(key))
    };
    let outcome = match (applied, rejected) {
        (Some(value), None) => {
            let id = match existing_id {
                Some(id) => id,
                None => nonempty(value.get("id")).map_err(|_| JmapError::OutcomeUnknown)?,
            };
            let expires = value
                .get("expires")
                .and_then(Value::as_str)
                .map(str::to_owned);
            PushOutcome::Applied {
                id: id.to_owned(),
                expires,
            }
        }
        (None, Some(value)) => {
            let kind = nonempty(value.get("type")).map_err(|_| JmapError::OutcomeUnknown)?;
            if kind.len() > 80 || !kind.bytes().all(|c| c.is_ascii_alphanumeric()) {
                return Err(JmapError::OutcomeUnknown);
            }
            PushOutcome::Rejected {
                kind: kind.to_owned(),
            }
        }
        _ => return Err(JmapError::OutcomeUnknown),
    };
    Ok(PushMutation {
        session_state,
        outcome,
    })
}
fn parse_submission(
    payload: &Value,
    account: &str,
    creation_id: &str,
    email_id: &str,
) -> Result<SubmissionResult, JmapError> {
    let responses = payload
        .get("methodResponses")
        .and_then(Value::as_array)
        .ok_or(JmapError::OutcomeUnknown)?;
    let first = responses
        .first()
        .and_then(Value::as_array)
        .filter(|v| v.len() == 3 && v[2].as_str() == Some("c1"))
        .ok_or(JmapError::OutcomeUnknown)?;
    if first[0].as_str() == Some("error") {
        return Err(JmapError::OutcomeUnknown);
    }
    if first[0].as_str() != Some("EmailSubmission/set")
        || first[1].get("accountId").and_then(Value::as_str) != Some(account)
    {
        return Err(JmapError::OutcomeUnknown);
    }
    let value = &first[1];
    let created = value.get("created").and_then(|v| v.get(creation_id));
    let rejected = value.get("notCreated").and_then(|v| v.get(creation_id));
    match (created, rejected) {
        (Some(object), None) => {
            let id = nonempty(object.get("id")).map_err(|_| JmapError::OutcomeUnknown)?;
            let email_update = if responses.len() == 2 {
                match responses[1]
                    .as_array()
                    .filter(|v| v.len() == 3 && v[2].as_str() == Some("c1"))
                {
                    Some(tuple)
                        if tuple[0].as_str() == Some("Email/set")
                            && tuple[1].get("accountId").and_then(Value::as_str)
                                == Some(account) =>
                    {
                        let updated = tuple[1].get("updated").and_then(|v| v.get(email_id));
                        let rejected = tuple[1].get("notUpdated").and_then(|v| v.get(email_id));
                        match (updated, rejected) {
                            (Some(Value::Null), None) => EmailUpdateResult::Applied,
                            (None, Some(v)) if v.is_object() => EmailUpdateResult::Rejected,
                            _ => EmailUpdateResult::Unknown,
                        }
                    }
                    _ => EmailUpdateResult::Unknown,
                }
            } else {
                EmailUpdateResult::Unknown
            };
            Ok(SubmissionResult::Created {
                id: id.to_owned(),
                email_update,
            })
        }
        (None, Some(object)) => {
            let kind = nonempty(object.get("type")).map_err(|_| JmapError::OutcomeUnknown)?;
            if kind.len() > 80 || !kind.bytes().all(|c| c.is_ascii_alphanumeric()) {
                return Err(JmapError::OutcomeUnknown);
            }
            Ok(SubmissionResult::Rejected {
                kind: kind.to_owned(),
            })
        }
        _ => Err(JmapError::OutcomeUnknown),
    }
}
fn parse_method(payload: &Value, method: &str) -> Result<Value, JmapError> {
    let responses = payload
        .get("methodResponses")
        .and_then(Value::as_array)
        .ok_or(JmapError::InvalidResponse)?;
    if responses.len() != 1 {
        return Err(JmapError::InvalidResponse);
    }
    let tuple = responses[0]
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or(JmapError::InvalidResponse)?;
    if tuple[2].as_str() != Some("c1") {
        return Err(JmapError::InvalidResponse);
    }
    if tuple[0].as_str() == Some("error") {
        let kind = nonempty(tuple[1].get("type"))?;
        // Only the server-defined error class is exposed; descriptions can contain private data.
        if kind.len() > 80 || !kind.bytes().all(|c| c.is_ascii_alphanumeric()) {
            return Err(JmapError::InvalidResponse);
        }
        return Err(JmapError::Method(kind.to_owned()));
    }
    if tuple[0].as_str() != Some(method) || !tuple[1].is_object() {
        return Err(JmapError::InvalidResponse);
    }
    Ok(tuple[1].clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn method_responses_require_matching_id_and_keep_errors_distinct() {
        assert_eq!(
            parse_method(
                &json!({"methodResponses":[["error",{"type":"cannotCalculateChanges"},"c1"]]}),
                "Email/changes"
            ),
            Err(JmapError::Method("cannotCalculateChanges".into()))
        );
        assert_eq!(
            parse_method(
                &json!({"methodResponses":[["Email/get",{},"wrong"]]}),
                "Email/get"
            ),
            Err(JmapError::InvalidResponse)
        );
    }
    #[test]
    fn endpoint_rejects_cross_origin_and_credentials() {
        let origin = Url::parse("https://mail.example/").unwrap();
        assert!(endpoint(&origin, "https://other.example/api").is_err());
        assert!(endpoint(&origin, "https://user@mail.example/api").is_err());
        assert!(endpoint(&origin, "/api").is_ok());
        assert!(allowed_origin(&Url::parse("http://127.0.0.1:8080/").unwrap(), true));
        assert!(allowed_origin(&Url::parse("http://[::1]:8080/").unwrap(), true));
        assert!(!allowed_origin(&Url::parse("http://localhost:8080/").unwrap(), true));
        assert!(!allowed_origin(&Url::parse("http://127.0.0.2:8080/").unwrap(), true));
        assert!(!allowed_origin(&Url::parse("http://127.0.0.1:8080/").unwrap(), false));
        assert!(endpoint(&Url::parse("http://127.0.0.1:8080/").unwrap(), "http://127.0.0.1:8081/api").is_err());
        let local = Url::parse("http://127.0.0.1:8080/").unwrap();
        let public = Url::parse("https://mail.example/").unwrap();
        assert_eq!(rewrite_loopback_endpoint(&local, &public, "https://mail.example/download/{accountId}/{blobId}/{name}?accept={type}").unwrap(), "http://127.0.0.1:8080/download/{accountId}/{blobId}/{name}?accept={type}");
        assert!(rewrite_loopback_endpoint(&local, &public, "https://other.example/api").is_err());
        assert!(rewrite_loopback_endpoint(&local, &public, "https://mail.example:444/api").is_err());
        assert!(rewrite_loopback_endpoint(&local, &public, "https://user@mail.example/api").is_err());
        assert!(rewrite_loopback_endpoint(&local, &public, "http://127.0.0.1:8081/api").is_err());
        assert!(valid_pointer_token("a~1b~0c"));
        assert!(!valid_pointer_token("a~2b"));
    }
    #[test]
    fn account_gates_are_shared() {
        assert!(Arc::ptr_eq(
            &account_gate("a", 4, 2),
            &account_gate("a", 1, 1)
        ));
        assert!(!Arc::ptr_eq(
            &account_gate("a", 4, 2),
            &account_gate("b", 4, 2)
        ));
    }
    #[test]
    fn submission_requires_a_single_native_outcome() {
        let submission =
            json!(["EmailSubmission/set",{"accountId":"acc","created":{"a":{"id":"s1"}}},"c1"]);
        let applied = json!(["Email/set",{"accountId":"acc","updated":{"e1":null}},"c1"]);
        let rejected =
            json!(["Email/set",{"accountId":"acc","notUpdated":{"e1":{"type":"forbidden"}}},"c1"]);
        assert_eq!(
            parse_submission(
                &json!({"methodResponses":[submission.clone(), applied]}),
                "acc",
                "a",
                "e1"
            ),
            Ok(SubmissionResult::Created {
                id: "s1".into(),
                email_update: EmailUpdateResult::Applied
            })
        );
        assert_eq!(
            parse_submission(
                &json!({"methodResponses":[submission.clone(), rejected]}),
                "acc",
                "a",
                "e1"
            ),
            Ok(SubmissionResult::Created {
                id: "s1".into(),
                email_update: EmailUpdateResult::Rejected
            })
        );
        assert_eq!(
            parse_submission(&json!({"methodResponses":[submission]}), "acc", "a", "e1"),
            Ok(SubmissionResult::Created {
                id: "s1".into(),
                email_update: EmailUpdateResult::Unknown
            })
        );
        assert_eq!(
            parse_submission(
                &json!({"methodResponses":[["EmailSubmission/set",{"accountId":"acc","notCreated":{"a":{"type":"forbidden"}}},"c1"]]}),
                "acc",
                "a",
                "e1"
            ),
            Ok(SubmissionResult::Rejected {
                kind: "forbidden".into()
            })
        );
        assert_eq!(
            parse_submission(
                &json!({"methodResponses":[["EmailSubmission/set",{"accountId":"acc","created":{},"notCreated":{}},"c1"]]}),
                "acc",
                "a",
                "e1"
            ),
            Err(JmapError::OutcomeUnknown)
        );
        assert_eq!(pointer_escape("a~/b"), "a~0~1b");
    }
    #[test]
    fn push_mutations_require_native_outcome_and_session_state() {
        let created = json!({"sessionState":"s1","methodResponses":[["PushSubscription/set",{"created":{"c":{"id":"p1","expires":"2030-01-01T00:00:00Z"}}},"c1"]]});
        let mutation = parse_push_mutation(&created, "created", "notCreated", "c", None).unwrap();
        assert_eq!(mutation.session_state, "s1");
        assert!(
            matches!(mutation.outcome, PushOutcome::Applied { id, expires: Some(_) } if id == "p1")
        );
        let verified = json!({"sessionState":"s1","methodResponses":[["PushSubscription/set",{"updated":{"p1":null}},"c1"]]});
        assert!(matches!(
            parse_push_mutation(&verified, "updated", "notUpdated", "p1", Some("p1"))
                .unwrap()
                .outcome,
            PushOutcome::Applied { .. }
        ));
        let missing =
            json!({"sessionState":"s1","methodResponses":[["PushSubscription/set",{},"c1"]]});
        assert!(matches!(
            parse_push_mutation(&missing, "created", "notCreated", "c", None),
            Err(JmapError::OutcomeUnknown)
        ));
    }
}
