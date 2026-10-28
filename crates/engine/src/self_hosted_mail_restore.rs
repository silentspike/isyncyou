//! Confirmed personal mail recovery through the ordinary JMAP write grant.
//! The selected MIME and exact intent are persisted in the encrypted provider
//! ledger before any remote write. No submission method is used.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use isyncyou_connectors::jmap::{JmapClient, JmapError, MailboxCreateResult};
use isyncyou_core::{config::ProviderConfig, Config};
use isyncyou_store::{ProviderScope, Store};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use crate::provider_credential::ProviderCredentialStore;

pub struct ApprovedMailRestore {
    pub operation_id: String,
    pub account_id: String,
    pub origin: String,
    pub mailbox_id: String,
    pub source_generation: String,
    pub source_object: String,
    pub source_version: String,
    pub original_sha256: String,
    pub original_mime: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RestoreOutcome {
    Verified(String),
    /// Existing native folder is absent or cannot safely be identified.
    RecoveredFolderUnavailable,
    Rejected,
    /// A remote result is ambiguous; never re-upload or re-import blindly.
    Unknown,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FolderOutcome { Ready(String), Rejected, Unknown }

trait Transport {
    fn discover_recovered(&self) -> Result<(String, Vec<String>), JmapError>;
    fn create_recovered(&self, operation: &str, state: &str) -> Result<Option<String>, JmapError>;
    fn recovered_folder(&self, id: &str) -> Result<bool, JmapError>;
    fn upload(&self, mime: &[u8]) -> Result<String, JmapError>;
    fn import(&self, operation: &str, blob: &str, folder: &str) -> Result<Option<String>, JmapError>;
    fn readback(&self, id: &str, folder: &str, expected_sha: &str) -> Result<bool, JmapError>;
}

impl Transport for JmapClient {
    fn discover_recovered(&self) -> Result<(String, Vec<String>), JmapError> {
        let mut position = 0;
        let mut query_state = None::<String>;
        let mut ids = Vec::new();
        loop {
            let page = self.query_mailboxes(position, 100)?;
            let state = page.get("queryState").and_then(Value::as_str).ok_or(JmapError::InvalidResponse)?;
            if query_state.as_deref().is_some_and(|old| old != state) { return Err(JmapError::InvalidResponse); }
            query_state.get_or_insert_with(|| state.to_owned());
            let page_ids = page.get("ids").and_then(Value::as_array).ok_or(JmapError::InvalidResponse)?;
            let total = page.get("total").and_then(Value::as_u64).ok_or(JmapError::InvalidResponse)? as usize;
            if total > 1000 || position + page_ids.len() > total { return Err(JmapError::InvalidResponse); }
            for id in page_ids { ids.push(id.as_str().filter(|x| !x.is_empty()).ok_or(JmapError::InvalidResponse)?.to_owned()); }
            position += page_ids.len();
            if position == total { break; }
            if page_ids.is_empty() { return Err(JmapError::InvalidResponse); }
        }
        let mut unique = std::collections::HashSet::new();
        if ids.iter().any(|id| !unique.insert(id)) { return Err(JmapError::InvalidResponse); }
        let mut state = None::<String>;
        let mut found = Vec::new();
        for chunk in ids.chunks(self.max_objects_in_get()) {
            let properties: Vec<String> = ["id", "name", "role", "myRights", "shareWith"].into_iter().map(str::to_owned).collect();
            let response = self.get("Mailbox", Some(chunk), Some(&properties))?;
            let page_state = response.get("state").and_then(Value::as_str).ok_or(JmapError::InvalidResponse)?;
            if state.as_deref().is_some_and(|old| old != page_state) { return Err(JmapError::InvalidResponse); }
            state.get_or_insert_with(|| page_state.to_owned());
            let list = response.get("list").and_then(Value::as_array).ok_or(JmapError::InvalidResponse)?;
            if list.len() != chunk.len() || response.get("notFound").and_then(Value::as_array).is_none_or(|x| !x.is_empty()) { return Err(JmapError::InvalidResponse); }
            let mut page_ids = std::collections::HashSet::new();
            for item in list {
                let id = item.get("id").and_then(Value::as_str).ok_or(JmapError::InvalidResponse)?;
                if !chunk.iter().any(|x| x == id) || !page_ids.insert(id) { return Err(JmapError::InvalidResponse); }
                if item.get("name").and_then(Value::as_str) == Some("Recovered") {
                    if !item.get("role").is_some_and(Value::is_null) ||
                       item.pointer("/myRights/mayAddItems").and_then(Value::as_bool) != Some(true) ||
                       !item.get("shareWith").and_then(Value::as_object).is_some_and(|acl| acl.is_empty()) {
                        return Err(JmapError::InvalidResponse);
                    }
                    found.push(id.to_owned());
                }
            }
        }
        let final_state = self.get("Mailbox", Some(&[]), None)?;
        let final_state = final_state.get("state").and_then(Value::as_str).ok_or(JmapError::InvalidResponse)?;
        if state.as_deref().is_some_and(|old| old != final_state) { return Err(JmapError::InvalidResponse); }
        Ok((final_state.to_owned(), found))
    }
    fn create_recovered(&self, operation: &str, state: &str) -> Result<Option<String>, JmapError> {
        match self.create_mailbox(operation, "Recovered", state)? {
            MailboxCreateResult::Created { id } => Ok(Some(id)),
            MailboxCreateResult::Rejected { .. } => Ok(None),
        }
    }
    fn recovered_folder(&self, id: &str) -> Result<bool, JmapError> {
        let properties: Vec<String> = ["id", "name", "role", "myRights", "shareWith"].into_iter().map(str::to_owned).collect();
        let response = self.get("Mailbox", Some(&[id.to_owned()]), Some(&properties))?;
        let list = response.get("list").and_then(Value::as_array).ok_or(JmapError::InvalidResponse)?;
        if list.len() != 1 || response.get("notFound").and_then(Value::as_array).is_none_or(|x| !x.is_empty()) { return Ok(false); }
        let item = &list[0];
        Ok(item.get("id").and_then(Value::as_str) == Some(id)
            && item.get("name").and_then(Value::as_str) == Some("Recovered")
            && item.get("role").is_some_and(Value::is_null)
            && item.pointer("/myRights/mayAddItems").and_then(Value::as_bool) == Some(true)
            && item.get("shareWith").and_then(Value::as_object).is_some_and(|acl| acl.is_empty()))
    }
    fn upload(&self, mime: &[u8]) -> Result<String, JmapError> {
        self.upload(mime.to_vec(), "message/rfc822")?.get("blobId").and_then(Value::as_str)
            .filter(|s| !s.is_empty()).map(str::to_owned).ok_or(JmapError::OutcomeUnknown)
    }
    fn import(&self, operation: &str, blob: &str, folder: &str) -> Result<Option<String>, JmapError> {
        let mut memberships = serde_json::Map::new();
        memberships.insert(folder.to_owned(), Value::Bool(true));
        let response = self.import_email(operation, blob, Value::Object(memberships))?;
        let created = response.get("created").and_then(|x| x.get(operation));
        let rejected = response.get("notCreated").and_then(|x| x.get(operation));
        match (created, rejected) {
            (Some(item), None) => item.get("id").and_then(Value::as_str).filter(|x| !x.is_empty())
                .map(|id| Some(id.to_owned())).ok_or(JmapError::OutcomeUnknown),
            (None, Some(item)) if item.get("type").and_then(Value::as_str).is_some() => Ok(None),
            _ => Err(JmapError::OutcomeUnknown),
        }
    }
    fn readback(&self, id: &str, folder: &str, expected_sha: &str) -> Result<bool, JmapError> {
        let response = self.get("Email", Some(&[id.to_owned()]), None)?;
        let list = response.get("list").and_then(Value::as_array).ok_or(JmapError::InvalidResponse)?;
        if list.len() != 1 || response.get("notFound").and_then(Value::as_array).is_none_or(|x| !x.is_empty()) ||
            list[0].get("id").and_then(Value::as_str) != Some(id) ||
            list[0].get("mailboxIds").and_then(|v| v.get(folder)).and_then(Value::as_bool) != Some(true) {
            return Ok(false);
        }
        let blob = list[0].get("blobId").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or(JmapError::InvalidResponse)?;
        let mime = self.download(blob, "recovered.eml", "message/rfc822")?;
        Ok(format!("{:x}", Sha256::digest(&mime)) == expected_sha)
    }
}

fn run<T: Transport>(store: &Store, transport: &T, scope: &ProviderScope, op: &ApprovedMailRestore) -> Result<RestoreOutcome, String> {
    let is_hash = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    if op.operation_id.is_empty() || op.operation_id.len() > 128 ||
        !op.operation_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') ||
        [&op.mailbox_id, &op.source_generation, &op.source_object, &op.source_version].iter().any(|s| s.is_empty() || s.len() > 512) ||
        !is_hash(&op.source_generation) || !is_hash(&op.source_object) || !is_hash(&op.source_version) ||
        op.original_mime.len() > 64 * 1024 * 1024 ||
        format!("{:x}", Sha256::digest(&op.original_mime)) != op.original_sha256 {
        return Err("Invalid recovered mail intent".into());
    }
    // The destination is rechecked using the current native grant, never an
    // arbitrary mailbox or a client-supplied server path.
    match transport.recovered_folder(&op.mailbox_id) {
        Ok(true) => {}, Ok(false) => return Ok(RestoreOutcome::RecoveredFolderUnavailable),
        Err(_) => return Ok(RestoreOutcome::Unknown),
    }
    let intent = serde_json::to_string(&json!({"kind":"jmap-mail-recovery","account":scope.account_id,
        "origin":scope.origin,"native_account":scope.collection,"mailbox":op.mailbox_id,
        "source_generation":op.source_generation,"source_object":op.source_object,"source_version":op.source_version,
        "sha256":op.original_sha256,"mime_base64":STANDARD.encode(&op.original_mime)}))
        .map_err(|_| "Invalid restore intent")?;
    let digest = format!("{:x}", Sha256::digest(intent.as_bytes()));
    if let Some((previous, state, id)) = store.provider_mutation(scope, &op.operation_id).map_err(|e| e.to_string())? {
        if previous != digest { return Err("Recovery operation does not match original intent".into()); }
        if state == "confirmed" { return Ok(RestoreOutcome::Verified(id.ok_or("Missing native ID")?)); }
        if state == "rejected" { return Ok(RestoreOutcome::Rejected); }
        // A known native ID permits read-only reconciliation; no resend.
        if let Some(id) = id {
            return finish_readback(store, transport, scope, op, &id);
        }
        return Ok(RestoreOutcome::Unknown);
    }
    if !store.claim_provider_mutation(scope, &op.operation_id, "jmap-mail-recovery", &intent, &digest)
        .map_err(|e| e.to_string())? { return Ok(RestoreOutcome::Unknown); }
    let blob = match transport.upload(&op.original_mime) { Ok(blob) => blob, Err(_) => return Ok(RestoreOutcome::Unknown) };
    // ACL changes during a potentially long upload must be observed before the
    // import. A server-side ACL race still requires native qualification.
    if !matches!(transport.recovered_folder(&op.mailbox_id), Ok(true)) { return Ok(RestoreOutcome::Unknown); }
    let id = match transport.import(&op.operation_id, &blob, &op.mailbox_id) {
        Ok(Some(id)) => id,
        Ok(None) => { return if store.finish_provider_mutation(scope, &op.operation_id, None).unwrap_or(false) {
            Ok(RestoreOutcome::Rejected) } else { Ok(RestoreOutcome::Unknown) }; },
        Err(_) => return Ok(RestoreOutcome::Unknown),
    };
    if !store.checkpoint_provider_mutation(scope, &op.operation_id, &digest, &id).unwrap_or(false) {
        return Ok(RestoreOutcome::Unknown);
    }
    finish_readback(store, transport, scope, op, &id)
}

fn finish_readback<T: Transport>(store: &Store, transport: &T, scope: &ProviderScope,
                                  op: &ApprovedMailRestore, id: &str) -> Result<RestoreOutcome, String> {
    if !matches!(transport.recovered_folder(&op.mailbox_id), Ok(true)) { return Ok(RestoreOutcome::Unknown); }
    match transport.readback(id, &op.mailbox_id, &op.original_sha256) {
        Ok(true) if store.finish_provider_mutation(scope, &op.operation_id, Some(id)).unwrap_or(false) => Ok(RestoreOutcome::Verified(id.into())),
        _ => Ok(RestoreOutcome::Unknown),
    }
}

fn ensure_folder<T: Transport>(store: &Store, transport: &T, scope: &ProviderScope,
                               operation: &str) -> Result<FolderOutcome, String> {
    if operation.is_empty() || operation.len() > 110 ||
        !operation.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err("Invalid folder operation".into());
    }
    let folder_op = format!("{operation}-folder");
    // A prior ambiguous create is never resolved merely by matching the name:
    // another client may have created the same folder concurrently.
    if let Some((_digest, state, id)) = store.provider_mutation(scope, &folder_op).map_err(|e| e.to_string())? {
        if state == "rejected" { return Ok(FolderOutcome::Rejected); }
        let Some(id) = id else { return Ok(FolderOutcome::Unknown); };
        return match transport.recovered_folder(&id) {
            Ok(true) if state == "confirmed" => Ok(FolderOutcome::Ready(id)),
            Ok(true) if store.finish_provider_mutation(scope, &folder_op, Some(&id)).unwrap_or(false) => Ok(FolderOutcome::Ready(id)),
            _ => Ok(FolderOutcome::Unknown),
        };
    }
    let (state, existing) = match transport.discover_recovered() { Ok(value) => value, Err(_) => return Ok(FolderOutcome::Unknown) };
    match existing.as_slice() {
        [id] => return Ok(FolderOutcome::Ready(id.clone())),
        [] => {},
        _ => return Ok(FolderOutcome::Unknown),
    }
    let intent = serde_json::to_string(&json!({"kind":"jmap-recovered-folder","account":scope.account_id,
        "origin":scope.origin,"native_account":scope.collection,"name":"Recovered","if_in_state":state}))
        .map_err(|_| "Invalid folder intent")?;
    let digest = format!("{:x}", Sha256::digest(intent.as_bytes()));
    if !store.claim_provider_mutation(scope, &folder_op, "jmap-recovered-folder", &intent, &digest)
        .map_err(|e| e.to_string())? { return Ok(FolderOutcome::Unknown); }
    let id = match transport.create_recovered(&folder_op, &state) {
        Ok(Some(id)) => id,
        Ok(None) => return if store.finish_provider_mutation(scope, &folder_op, None).unwrap_or(false) {
            Ok(FolderOutcome::Rejected) } else { Ok(FolderOutcome::Unknown) },
        Err(_) => return Ok(FolderOutcome::Unknown),
    };
    if !store.checkpoint_provider_mutation(scope, &folder_op, &digest, &id).unwrap_or(false) {
        return Ok(FolderOutcome::Unknown);
    }
    match transport.recovered_folder(&id) {
        Ok(true) if store.finish_provider_mutation(scope, &folder_op, Some(&id)).unwrap_or(false) => Ok(FolderOutcome::Ready(id)),
        _ => Ok(FolderOutcome::Unknown),
    }
}

/// Obtain or create a personal Recovered folder after an effect-bound approval.
/// Unknown folder-creation outcomes require native reconciliation, not retry.
pub fn ensure_recovered_folder_with_store(cfg: &Config, account: &str, origin: &str,
                                          operation: &str, credentials: &dyn ProviderCredentialStore) -> Result<FolderOutcome, String> {
    ensure_folder_inner(cfg, account, origin, operation, Some(credentials))
}

pub fn ensure_recovered_folder(cfg: &Config, account: &str, origin: &str,
                               operation: &str) -> Result<FolderOutcome, String> {
    ensure_folder_inner(cfg, account, origin, operation, None)
}

fn ensure_folder_inner(cfg: &Config, account: &str, origin: &str, operation: &str,
                       credentials: Option<&dyn ProviderCredentialStore>) -> Result<FolderOutcome, String> {
    match cfg.provider_accounts.get(account) {
        Some(ProviderConfig::SelfHosted { origin: current, .. }) if current == origin => {},
        _ => return Err("Account or origin changed after approval".into()),
    }
    let token = match credentials {
        Some(store) => crate::provider_auth::self_hosted_access_token_with_store(cfg, account, store)?,
        None => crate::provider_auth::self_hosted_access_token(cfg, account)?,
    };
    let client = JmapClient::discover(origin, token).map_err(|_| "Mail service unavailable")?;
    let scope = ProviderScope { account_id: account.into(), origin: origin.into(), service: "mail".into(),
                                collection: client.account_id().to_owned() };
    let store = crate::provider_store::open(cfg, account)?;
    ensure_folder(&store, &client, &scope, operation)
}

/// Caller must supply an approval bound to every field and exact original bytes.
pub fn execute_with_store(cfg: &Config, op: &ApprovedMailRestore, credentials: &dyn ProviderCredentialStore) -> Result<RestoreOutcome, String> {
    execute_inner(cfg, op, Some(credentials))
}

pub fn execute(cfg: &Config, op: &ApprovedMailRestore) -> Result<RestoreOutcome, String> {
    execute_inner(cfg, op, None)
}

fn execute_inner(cfg: &Config, op: &ApprovedMailRestore, credentials: Option<&dyn ProviderCredentialStore>) -> Result<RestoreOutcome, String> {
    match cfg.provider_accounts.get(&op.account_id) {
        Some(ProviderConfig::SelfHosted { origin, .. }) if origin == &op.origin => {},
        _ => return Err("Account or origin changed after approval".into()),
    }
    let token = match credentials {
        Some(store) => crate::provider_auth::self_hosted_access_token_with_store(cfg, &op.account_id, store)?,
        None => crate::provider_auth::self_hosted_access_token(cfg, &op.account_id)?,
    };
    let client = JmapClient::discover(&op.origin, token).map_err(|_| "Mail service unavailable")?;
    let scope = ProviderScope { account_id: op.account_id.clone(), origin: op.origin.clone(),
        service: "mail".into(), collection: client.account_id().to_owned() };
    let store = crate::provider_store::open(cfg, &op.account_id)?;
    run(&store, &client, &scope, op)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    struct Fake { folder: Cell<bool>, folder_count: Cell<u32>, upload_count: Cell<u32>, import_count: Cell<u32>,
                  lost_import: bool, readable: Cell<bool> }
    impl Transport for Fake {
        fn discover_recovered(&self) -> Result<(String, Vec<String>), JmapError> {
            Ok(("state".into(), if self.folder.get() { vec!["recovered".into()] } else { vec![] }))
        }
        fn create_recovered(&self, _: &str, _: &str) -> Result<Option<String>, JmapError> {
            self.folder_count.set(self.folder_count.get() + 1);
            self.folder.set(true);
            Ok(Some("recovered".into()))
        }
        fn recovered_folder(&self, _: &str) -> Result<bool, JmapError> { Ok(self.folder.get()) }
        fn upload(&self, _: &[u8]) -> Result<String, JmapError> {
            self.upload_count.set(self.upload_count.get() + 1); Ok("blob".into())
        }
        fn import(&self, _: &str, _: &str, _: &str) -> Result<Option<String>, JmapError> {
            self.import_count.set(self.import_count.get() + 1);
            if self.lost_import { Err(JmapError::OutcomeUnknown) } else { Ok(Some("native-id".into())) }
        }
        fn readback(&self, _: &str, _: &str, _: &str) -> Result<bool, JmapError> { Ok(self.readable.get()) }
    }
    fn fixture() -> (ProviderScope, ApprovedMailRestore) {
        let mime = b"Subject: saved\r\n\r\noriginal".to_vec();
        (ProviderScope { account_id:"a".into(), origin:"https://example.test/".into(),
                         service:"mail".into(), collection:"native-a".into() },
         ApprovedMailRestore { operation_id:"restore-1".into(), account_id:"a".into(),
            origin:"https://example.test/".into(), mailbox_id:"recovered".into(),
            source_generation:"a".repeat(64), source_object:"b".repeat(64), source_version:"c".repeat(64),
            original_sha256:format!("{:x}", Sha256::digest(&mime)), original_mime:mime })
    }
    fn fake(folder: bool, lost_import: bool, readable: bool) -> Fake {
        Fake { folder:Cell::new(folder), folder_count:Cell::new(0), upload_count:Cell::new(0), import_count:Cell::new(0), lost_import, readable:Cell::new(readable) }
    }
    #[test]
    fn readback_verified_before_success_and_repeat_does_not_resend() {
        let store = Store::open_in_memory().unwrap(); let (scope, op) = fixture(); let transport = fake(true, false, true);
        assert_eq!(run(&store, &transport, &scope, &op).unwrap(), RestoreOutcome::Verified("native-id".into()));
        assert_eq!(run(&store, &transport, &scope, &op).unwrap(), RestoreOutcome::Verified("native-id".into()));
        assert_eq!((transport.upload_count.get(), transport.import_count.get()), (1,1));
    }
    #[test]
    fn missing_folder_and_lost_import_never_trigger_unapproved_retry() {
        let store = Store::open_in_memory().unwrap(); let (scope, op) = fixture(); let denied = fake(false, false, true);
        assert_eq!(run(&store, &denied, &scope, &op).unwrap(), RestoreOutcome::RecoveredFolderUnavailable);
        assert_eq!(denied.upload_count.get(), 0);
        let lost = fake(true, true, true);
        assert_eq!(run(&store, &lost, &scope, &op).unwrap(), RestoreOutcome::Unknown);
        assert_eq!(run(&store, &lost, &scope, &op).unwrap(), RestoreOutcome::Unknown);
        assert_eq!((lost.upload_count.get(), lost.import_count.get()), (1,1));
    }
    #[test]
    fn known_native_id_reconciles_read_only_without_second_import() {
        let store = Store::open_in_memory().unwrap(); let (scope, op) = fixture(); let transport = fake(true, false, false);
        assert_eq!(run(&store, &transport, &scope, &op).unwrap(), RestoreOutcome::Unknown);
        transport.readable.set(true);
        assert_eq!(run(&store, &transport, &scope, &op).unwrap(), RestoreOutcome::Verified("native-id".into()));
        assert_eq!((transport.upload_count.get(), transport.import_count.get()), (1,1));
    }
    #[test]
    fn recovered_folder_create_is_persisted_verified_and_not_duplicated() {
        let store = Store::open_in_memory().unwrap(); let (scope, _op) = fixture(); let transport = fake(false, false, true);
        assert_eq!(ensure_folder(&store, &transport, &scope, "restore-1").unwrap(), FolderOutcome::Ready("recovered".into()));
        assert_eq!(ensure_folder(&store, &transport, &scope, "restore-1").unwrap(), FolderOutcome::Ready("recovered".into()));
        assert_eq!(transport.folder_count.get(), 1);
    }
    #[test]
    fn unknown_folder_creation_is_not_repeated_or_inferred_from_name() {
        struct Lost { calls: Cell<u32> }
        impl Transport for Lost {
            fn discover_recovered(&self) -> Result<(String, Vec<String>), JmapError> { Ok(("s".into(), vec![])) }
            fn create_recovered(&self, _: &str, _: &str) -> Result<Option<String>, JmapError> {
                self.calls.set(self.calls.get() + 1); Err(JmapError::OutcomeUnknown)
            }
            fn recovered_folder(&self, _: &str) -> Result<bool, JmapError> { panic!("no known native id") }
            fn upload(&self, _: &[u8]) -> Result<String, JmapError> { panic!("no upload") }
            fn import(&self, _: &str, _: &str, _: &str) -> Result<Option<String>, JmapError> { panic!("no import") }
            fn readback(&self, _: &str, _: &str, _: &str) -> Result<bool, JmapError> { panic!("no readback") }
        }
        let store = Store::open_in_memory().unwrap(); let (scope, _op) = fixture(); let transport = Lost { calls:Cell::new(0) };
        assert_eq!(ensure_folder(&store, &transport, &scope, "restore-1").unwrap(), FolderOutcome::Unknown);
        assert_eq!(ensure_folder(&store, &transport, &scope, "restore-1").unwrap(), FolderOutcome::Unknown);
        assert_eq!(transport.calls.get(), 1);
    }
}
