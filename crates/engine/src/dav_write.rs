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
    ) -> Result<Option<String>, DavError> {
        DavClient::put(self, scope, href, raw, etag)
    }
    fn delete(&self, scope: &ProviderScope, href: &str, etag: &str) -> Result<(), DavError> {
        DavClient::delete(self, scope, href, etag)
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
        {
            return Err("A strong resource ETag is required".into());
        }
    } else if op.raw.is_none() {
        return Err("Delete requires a strong resource ETag".into());
    }
    let kind = if op.raw.is_some() {
        "dav-put"
    } else {
        "dav-delete"
    };
    let intent = serde_json::to_string(&serde_json::json!({"kind":kind,"account":scope.account_id,"origin":scope.origin,"service":scope.service,"collection":scope.collection,"href":target,"etag":op.expected_etag,"raw_base64":op.raw.as_ref().map(|v| STANDARD.encode(v))})).map_err(|_| "Invalid DAV intent")?;
    let digest = format!("{:x}", Sha256::digest(intent.as_bytes()));
    if let Some(outcome) = recorded(store, scope, &op.operation_id, &digest)? {
        return Ok(outcome);
    }
    if !store
        .claim_provider_mutation(scope, &op.operation_id, kind, &intent, &digest)
        .map_err(|e| e.to_string())?
    {
        return recorded(store, scope, &op.operation_id, &digest)?
            .ok_or_else(|| "Mutation claim disappeared".into());
    }
    let result = match &op.raw {
        Some(raw) => transport
            .put(scope, target, raw.clone(), op.expected_etag.as_deref())
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

fn execute_inner(
    cfg: &Config,
    entry: &str,
    op: &ApprovedDavWrite,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<WriteOutcome, String> {
    if !matches!(op.service.as_str(), "calendar" | "contacts") {
        return Err("Unsupported DAV service".into());
    }
    let origin = match cfg.provider_accounts.get(&op.account_id) {
        Some(ProviderConfig::SelfHosted { origin, .. }) if origin == &op.origin => origin,
        _ => return Err("Account or provider changed after approval".into()),
    };
    let store = crate::provider_store::open(cfg, &op.account_id)?;
    let token = match credentials {
        Some(credentials) => crate::provider_auth::self_hosted_access_token_with_store(
            cfg,
            &op.account_id,
            credentials,
        )?,
        None => crate::provider_auth::self_hosted_access_token(cfg, &op.account_id)?,
    };
    let client = DavClient::new(origin, token).map_err(|e| e.to_string())?;
    let collection = client
        .discover(entry)
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|c| {
            c.url == op.collection
                && if op.service == "calendar" {
                    c.calendar
                } else {
                    c.addressbook
                }
        })
        .ok_or("Collection is not currently accessible")?;
    let scope = ProviderScope {
        account_id: op.account_id.clone(),
        origin: origin.clone(),
        service: op.service.clone(),
        collection: collection.url,
    };
    let target = client
        .validate_target(&scope, &op.href)
        .map_err(|e| e.to_string())?;
    let (resource, privilege) = match (&op.raw, &op.expected_etag) {
        (Some(_), None) => (None, "bind"),
        (Some(_), Some(_)) => (Some(target.as_str()), "write-content"),
        (None, _) => (None, "unbind"),
    };
    if !client
        .permits(&scope, resource, privilege)
        .map_err(|e| e.to_string())?
    {
        return Err("DAV privilege no longer permits this effect".into());
    }
    dispatch(&store, &client, &scope, op, &target)
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
            raw: Some(b"original bytes".to_vec()),
        };
        (scope, op)
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
