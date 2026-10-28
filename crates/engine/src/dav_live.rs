//! Authenticated DAV discovery and collection sync for configured accounts.

use isyncyou_connectors::dav::{AclEntry, Collection, DavClient, DavError, ExpandedCalendar};
use isyncyou_core::{config::ProviderConfig, Config};
use isyncyou_store::{ProviderObject, ProviderScope, Store};
use crate::provider_credential::ProviderCredentialStore;

fn configured(cfg: &Config, account: &str) -> Result<(String, Store), String> {
    let origin = match cfg.provider_accounts.get(account) {
        Some(ProviderConfig::SelfHosted { origin, .. }) => origin.clone(),
        None => return Err("Not a self-hosted account".into()),
    };
    let store = crate::provider_store::open(cfg, account)?;
    Ok((origin, store))
}

fn client(cfg: &Config, account: &str, origin: &str) -> Result<DavClient, String> {
    let token = crate::provider_auth::self_hosted_access_token(cfg, account)?;
    DavClient::new(origin, token).map_err(|e| e.to_string())
}

fn client_with_store(cfg: &Config, account: &str, origin: &str, credentials: &dyn ProviderCredentialStore) -> Result<DavClient, String> {
    let token = crate::provider_auth::self_hosted_access_token_with_store(cfg, account, credentials)?;
    DavClient::new(origin, token).map_err(|e| e.to_string())
}

/// Resolve server-advertised collections for the configured account.
pub fn discover(cfg: &Config, account: &str, entry: &str) -> Result<Vec<Collection>, String> {
    let (origin, _) = configured(cfg, account)?;
    client(cfg, account, &origin)?.discover(entry).map_err(|e| e.to_string())
}

/// Platform credential variant for the same discovery path.
pub fn discover_with_store(cfg: &Config, account: &str, entry: &str, credentials: &dyn ProviderCredentialStore) -> Result<Vec<Collection>, String> {
    let (origin, _) = configured(cfg, account)?;
    client_with_store(cfg, account, &origin, credentials)?.discover(entry).map_err(|e| e.to_string())
}

/// Synchronize only a collection returned by authenticated discovery. Calendar
/// resources may contain VEVENT and VTODO, which intentionally share one scope.
pub fn sync_collection(cfg: &Config, account: &str, entry: &str, collection: &str, service: &str) -> Result<usize, String> {
    sync_collection_inner(cfg, account, entry, collection, service, None)
}

/// Platform credential variant for the same synchronization path.
pub fn sync_collection_with_store(cfg: &Config, account: &str, entry: &str, collection: &str, service: &str, credentials: &dyn ProviderCredentialStore) -> Result<usize, String> {
    sync_collection_inner(cfg, account, entry, collection, service, Some(credentials))
}

/// Result of a single account pass. Partial collection failure does not hide
/// successful sync of other collections and never deletes an inaccessible one.
#[derive(Debug, Default)]
pub struct SyncPass {
    pub collections: usize,
    pub changes: usize,
    pub failed: usize,
}

/// Discover once and synchronize every available calendar and address book.
/// VTODO resources share the calendar collection and cursor with events.
pub fn sync_all_with_store(cfg: &Config, account: &str, entry: &str, credentials: &dyn ProviderCredentialStore) -> Result<SyncPass, String> {
    sync_all_inner(cfg, account, entry, Some(credentials))
}

/// Desktop variant using the configured system keyring.
pub fn sync_all(cfg: &Config, account: &str, entry: &str) -> Result<SyncPass, String> {
    sync_all_inner(cfg, account, entry, None)
}

fn sync_all_inner(cfg: &Config, account: &str, entry: &str, credentials: Option<&dyn ProviderCredentialStore>) -> Result<SyncPass, String> {
    let (origin, store) = configured(cfg, account)?;
    let client = match credentials {
        Some(credentials) => client_with_store(cfg, account, &origin, credentials)?,
        None => client(cfg, account, &origin)?,
    };
    let collections = client.discover(entry).map_err(|e| e.to_string())?;
    let mut report = SyncPass::default();
    let mut present = std::collections::HashSet::new();
    for collection in collections {
        for (enabled, service) in [(collection.calendar, "calendar"), (collection.addressbook, "contacts")] {
            if !enabled { continue; }
            report.collections += 1;
            present.insert((service.to_owned(), collection.url.clone()));
            let scope = ProviderScope { account_id: account.into(), origin: origin.clone(), service: service.into(), collection: collection.url.clone() };
            match client.sync_collection(&store, &scope) {
                Ok(changes) => report.changes += changes,
                Err(_) => report.failed += 1,
            }
        }
    }
    for service in ["calendar", "contacts"] {
        for scope in store.provider_scopes(account, &origin, service).map_err(|e| e.to_string())? {
            if !present.contains(&(service.to_owned(), scope.collection.clone())) {
                store.set_provider_accessible(&scope, false).map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(report)
}

fn sync_collection_inner(cfg: &Config, account: &str, entry: &str, collection: &str, service: &str, credentials: Option<&dyn ProviderCredentialStore>) -> Result<usize, String> {
    if !matches!(service, "calendar" | "contacts") { return Err("Unsupported DAV service".into()); }
    let (origin, store) = configured(cfg, account)?;
    let client = match credentials {
        Some(credentials) => client_with_store(cfg, account, &origin, credentials)?,
        None => client(cfg, account, &origin)?,
    };
    let scope = ProviderScope { account_id: account.into(), origin, service: service.into(), collection: collection.into() };
    let collections = match client.discover(entry) {
        Ok(collections) => collections,
        Err(DavError::Http(403)) => {
            store.set_provider_accessible(&scope, false).map_err(|e| e.to_string())?;
            return Err("Collection is not currently accessible".into());
        }
        Err(error) => return Err(error.to_string()),
    };
    if !collections.iter().any(|c| c.url == collection && if service == "calendar" { c.calendar } else { c.addressbook }) {
        store.set_provider_accessible(&scope, false).map_err(|e| e.to_string())?;
        return Err("Collection is not currently accessible".into());
    }
    client.sync_collection(&store, &scope).map_err(|e| e.to_string())
}

/// Query one currently discoverable calendar's free/busy, not an aggregate
/// availability calculation for the calendar owner.
pub fn free_busy(cfg: &Config, account: &str, entry: &str, collection: &str, start: &str, end: &str, credentials: Option<&dyn ProviderCredentialStore>) -> Result<Vec<u8>, String> {
    let (origin, _) = configured(cfg, account)?;
    let client = match credentials {
        Some(credentials) => client_with_store(cfg, account, &origin, credentials)?,
        None => client(cfg, account, &origin)?,
    };
    if !client.discover(entry).map_err(|e| e.to_string())?.iter().any(|c| c.url == collection && c.calendar) {
        return Err("Collection is not currently accessible".into());
    }
    let scope = ProviderScope { account_id: account.into(), origin, service: "calendar".into(), collection: collection.into() };
    client.free_busy(&scope, start, end).map_err(|e| e.to_string())
}

/// Read native collection sharing entries when the account has DAV read-acl.
pub fn read_acl(cfg: &Config, account: &str, entry: &str, collection: &str, service: &str, credentials: Option<&dyn ProviderCredentialStore>) -> Result<Vec<AclEntry>, String> {
    if !matches!(service, "calendar" | "contacts") { return Err("Unsupported DAV service".into()); }
    let (origin, _) = configured(cfg, account)?;
    let client = match credentials {
        Some(credentials) => client_with_store(cfg, account, &origin, credentials)?,
        None => client(cfg, account, &origin)?,
    };
    if !client.discover(entry).map_err(|e| e.to_string())?.iter().any(|c| c.url == collection && if service == "calendar" {c.calendar} else {c.addressbook}) { return Err("Collection is not currently accessible".into()); }
    let scope = ProviderScope {account_id:account.into(), origin, service:service.into(), collection:collection.into()};
    client.acl(&scope).map_err(|e| e.to_string())
}

/// A live server-expanded event view; original ICS resources stay untouched.
pub fn expanded_events(cfg: &Config, account: &str, entry: &str, collection: &str, start: &str, end: &str, credentials: Option<&dyn ProviderCredentialStore>) -> Result<Vec<ExpandedCalendar>, String> {
    let (origin, _) = configured(cfg, account)?;
    let client = match credentials {
        Some(credentials) => client_with_store(cfg, account, &origin, credentials)?,
        None => client(cfg, account, &origin)?,
    };
    if !client.discover(entry).map_err(|e| e.to_string())?.iter().any(|c| c.url == collection && c.calendar) {
        return Err("Collection is not currently accessible".into());
    }
    let scope = ProviderScope { account_id: account.into(), origin, service: "calendar".into(), collection: collection.into() };
    client.expanded_events(&scope, start, end).map_err(|e| e.to_string())
}

/// Parse a live bounded recurrence expansion while retaining the resource's
/// native identity and ETag. This view never replaces stored source bytes.
pub fn expanded_event_views(cfg: &Config, account: &str, entry: &str, collection: &str, start: &str, end: &str, credentials: Option<&dyn ProviderCredentialStore>) -> Result<Vec<ProjectedObject>, String> {
    let events = expanded_events(cfg, account, entry, collection, start, end, credentials)?;
    Ok(events.into_iter().map(|event| ProjectedObject {
        remote_id: event.href,
        etag: Some(event.etag),
        view: isyncyou_connectors::dav::projection::calendar(event.calendar_data.as_bytes()),
    }).collect())
}

/// Read a single currently accessible native object; useful for explicit
/// conflict inspection and restore verification, without changing the cache.
pub fn read_current(cfg: &Config, account: &str, entry: &str, collection: &str, service: &str, href: &str, credentials: Option<&dyn ProviderCredentialStore>) -> Result<ProviderObject, String> {
    if !matches!(service, "calendar" | "contacts") { return Err("Unsupported DAV service".into()); }
    let (origin, _) = configured(cfg, account)?;
    let client = match credentials {
        Some(credentials) => client_with_store(cfg, account, &origin, credentials)?,
        None => client(cfg, account, &origin)?,
    };
    if !client.discover(entry).map_err(|e| e.to_string())?.iter().any(|c| c.url == collection && if service == "calendar" { c.calendar } else { c.addressbook }) {
        return Err("Collection is not currently accessible".into());
    }
    let scope = ProviderScope { account_id: account.into(), origin, service: service.into(), collection: collection.into() };
    client.read_resource(&scope, href).map_err(|e| e.to_string())
}

/// Cached original bytes and the last known collection access status.
pub struct CachedObjects {
    pub account_id: String,
    pub origin: String,
    pub accessible: Option<bool>,
    /// Local commit time, not a server freshness guarantee.
    pub observed_at: Option<i64>,
    pub cursor: Option<String>,
    pub objects: Vec<ProviderObject>,
}

pub struct ProjectedObject {
    pub remote_id: String,
    pub etag: Option<String>,
    pub view: Result<serde_json::Value, String>,
}

pub struct ProjectedPage {
    pub account_id: String,
    pub origin: String,
    pub accessible: Option<bool>,
    pub observed_at: Option<i64>,
    pub cursor: Option<String>,
    pub objects: Vec<ProjectedObject>,
}

/// Exact bytes and version proposed for a later effect-bound confirmation.
pub struct PreparedEdit {
    pub href: String,
    pub expected_etag: String,
    /// Required on edits that depend on the entire collection state.
    pub expected_collection_token: Option<String>,
    pub raw: Vec<u8>,
}

/// Prepare an edit on one VEVENT/VTODO master or exception from the cached
/// original. No operation is dispatched; the host must bind approval to the
/// returned bytes, account, target, version, and scheduling effects.
pub fn prepare_text_edit(cfg: &Config, account: &str, collection: &str, href: &str, uid: &str, recurrence_id: Option<&str>, changes: &[(&str, &str)]) -> Result<PreparedEdit, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope { account_id: account.into(), origin, service:"calendar".into(), collection:collection.into() };
    if store.provider_accessible(&scope).map_err(|e| e.to_string())? != Some(true) { return Err("Calendar access is unknown or unavailable".into()); }
    let object = store.provider_object(&scope, href).map_err(|e| e.to_string())?.ok_or("Cached calendar resource missing")?;
    let expected_etag = object.etag.filter(|tag| !tag.is_empty() && !tag.starts_with("W/")).ok_or("Strong ETag required")?;
    let raw = isyncyou_connectors::dav::edit::edit_text(&object.raw, uid, recurrence_id, changes)?;
    Ok(PreparedEdit { href:object.remote_id, expected_etag, expected_collection_token:None, raw })
}

/// Prepare one contact-property edit without altering the cached source.
pub fn prepare_contact_text_edit(cfg: &Config, account: &str, collection: &str, href: &str, key: &str, index: usize, value: &str) -> Result<PreparedEdit, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope { account_id: account.into(), origin, service:"contacts".into(), collection:collection.into() };
    if store.provider_accessible(&scope).map_err(|e| e.to_string())? != Some(true) { return Err("Address book access is unknown or unavailable".into()); }
    let object = store.provider_object(&scope, href).map_err(|e| e.to_string())?.ok_or("Cached contact missing")?;
    let expected_etag = object.etag.filter(|tag| !tag.is_empty() && !tag.starts_with("W/")).ok_or("Strong ETag required")?;
    let raw = isyncyou_connectors::dav::edit::edit_contact_text(&object.raw, key, index, value)?;
    Ok(PreparedEdit { href:object.remote_id, expected_etag, expected_collection_token:None, raw })
}

/// Prepare completion or reopening of one task master or existing exception.
pub fn prepare_task_completion(cfg: &Config, account: &str, collection: &str, href: &str, uid: &str, recurrence_id: Option<&str>, completed_at: Option<&str>) -> Result<PreparedEdit, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope { account_id: account.into(), origin, service:"calendar".into(), collection:collection.into() };
    if store.provider_accessible(&scope).map_err(|e| e.to_string())? != Some(true) { return Err("Task collection access is unknown or unavailable".into()); }
    let object = store.provider_object(&scope, href).map_err(|e| e.to_string())?.ok_or("Cached task resource missing")?;
    let expected_etag = object.etag.filter(|tag| !tag.is_empty() && !tag.starts_with("W/")).ok_or("Strong ETag required")?;
    let raw = isyncyou_connectors::dav::edit::task_completion(&object.raw, uid, recurrence_id, completed_at)?;
    Ok(PreparedEdit { href:object.remote_id, expected_etag, expected_collection_token:None, raw })
}

/// Prepare priority and/or all-day or UTC due-date changes for one task.
pub fn prepare_task_attributes(cfg: &Config, account: &str, collection: &str, href: &str, uid: &str, recurrence_id: Option<&str>, priority: Option<u8>, due: Option<(&str, bool)>) -> Result<PreparedEdit, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope { account_id: account.into(), origin, service:"calendar".into(), collection:collection.into() };
    if store.provider_accessible(&scope).map_err(|e| e.to_string())? != Some(true) { return Err("Task collection access is unknown or unavailable".into()); }
    let object = store.provider_object(&scope, href).map_err(|e| e.to_string())?.ok_or("Cached task resource missing")?;
    let expected_etag = object.etag.filter(|tag| !tag.is_empty() && !tag.starts_with("W/")).ok_or("Strong ETag required")?;
    let raw = isyncyou_connectors::dav::edit::task_attributes(&object.raw, uid, recurrence_id, priority, due)?;
    Ok(PreparedEdit { href:object.remote_id, expected_etag, expected_collection_token:None, raw })
}

pub(crate) fn prepare_task_parent_in_store(store: &Store, scope: &ProviderScope, href: &str, uid: &str, parent: Option<&str>) -> Result<PreparedEdit, String> {
    use isyncyou_store::ProviderDavIndex;
    if store.provider_accessible(scope).map_err(|e| e.to_string())? != Some(true) || store.provider_dav_index_incomplete_count(scope).map_err(|e| e.to_string())? != 0 { return Err("Task graph is unavailable or incomplete".into()); }
    let snapshot = store.provider_export_snapshot(&scope.account_id, &scope.origin).map_err(|e| e.to_string())?;
    let collection = snapshot.collections.iter().find(|c| c.scope == *scope && c.complete && c.accessible).ok_or("Task collection is not fully synchronized")?;
    let token = collection.cursor.clone().filter(|v| !v.is_empty()).ok_or("Task collection checkpoint missing")?;
    let lookup = |id: &str| -> Result<isyncyou_store::ProviderDavIndexEntry, String> {
        let mut matches = store.provider_dav_uid_lookup(scope, id).map_err(|e| e.to_string())?;
        if matches.len() != 1 { return Err("Task identity is missing or ambiguous".into()); }
        let entry = matches.pop().ok_or("Task missing")?;
        if !matches!(&entry.index, ProviderDavIndex::Valid {kind,..} if kind == "task") { return Err("Parent must be a task in the same collection".into()); }
        Ok(entry)
    };
    let child = lookup(uid)?;
    if child.remote_id != href { return Err("Task target no longer matches its UID".into()); }
    let mut seen = std::collections::HashSet::new();
    seen.insert(uid.to_owned());
    let mut current = parent.map(str::to_owned);
    while let Some(id) = current {
        if seen.len() >= 1024 || !seen.insert(id.clone()) { return Err("Task parent would create a cycle or exceeds depth limit".into()); }
        let entry = lookup(&id)?;
        current = match entry.index { ProviderDavIndex::Valid {parent_uid,..} => parent_uid, ProviderDavIndex::Invalid => return Err("Invalid task graph".into()) };
    }
    let object = store.provider_object(scope, href).map_err(|e| e.to_string())?.ok_or("Cached task resource missing")?;
    let expected_etag = object.etag.filter(|tag| !tag.is_empty() && !tag.starts_with("W/")).ok_or("Strong ETag required")?;
    let raw = isyncyou_connectors::dav::edit::task_parent(&object.raw, uid, parent)?;
    Ok(PreparedEdit { href:object.remote_id, expected_etag, expected_collection_token:Some(token), raw })
}

/// Prepare native parentage after checking the complete, indexed task graph.
/// The returned sync token must be bound into the approved DAV write.
pub fn prepare_task_parent(cfg: &Config, account: &str, collection: &str, href: &str, uid: &str, parent: Option<&str>) -> Result<PreparedEdit, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope { account_id:account.into(), origin, service:"calendar".into(), collection:collection.into() };
    prepare_task_parent_in_store(&store, &scope, href, uid, parent)
}

/// Prepare an explicit attendee response; server scheduling effects require
/// separate effect-bound confirmation and real provider verification.
pub fn prepare_attendee_response(cfg: &Config, account: &str, collection: &str, href: &str, uid: &str, recurrence_id: Option<&str>, attendee: &str, response: &str) -> Result<PreparedEdit, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope { account_id: account.into(), origin, service:"calendar".into(), collection:collection.into() };
    if store.provider_accessible(&scope).map_err(|e| e.to_string())? != Some(true) { return Err("Calendar access is unknown or unavailable".into()); }
    let object = store.provider_object(&scope, href).map_err(|e| e.to_string())?.ok_or("Cached event missing")?;
    let expected_etag = object.etag.filter(|tag| !tag.is_empty() && !tag.starts_with("W/")).ok_or("Strong ETag required")?;
    let raw = isyncyou_connectors::dav::edit::respond_attendee(&object.raw, uid, recurrence_id, attendee, response)?;
    Ok(PreparedEdit { href:object.remote_id, expected_etag, expected_collection_token:None, raw })
}

/// Prepare cancellation with an explicit advanced sequence and exact ETag.
pub fn prepare_event_cancellation(cfg: &Config, account: &str, collection: &str, href: &str, uid: &str, recurrence_id: Option<&str>, sequence: u32) -> Result<PreparedEdit, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope { account_id: account.into(), origin, service:"calendar".into(), collection:collection.into() };
    if store.provider_accessible(&scope).map_err(|e| e.to_string())? != Some(true) { return Err("Calendar access is unknown or unavailable".into()); }
    let object = store.provider_object(&scope, href).map_err(|e| e.to_string())?.ok_or("Cached event missing")?;
    let expected_etag = object.etag.filter(|tag| !tag.is_empty() && !tag.starts_with("W/")).ok_or("Strong ETag required")?;
    let raw = isyncyou_connectors::dav::edit::cancel_event(&object.raw, uid, recurrence_id, sequence)?;
    Ok(PreparedEdit { href:object.remote_id, expected_etag, expected_collection_token:None, raw })
}

/// Prepare a validated series-level recurrence change; overrides stay in the
/// original resource and the host must confirm any affected occurrences.
pub fn prepare_series_recurrence(cfg: &Config, account: &str, collection: &str, href: &str, uid: &str, rule: Option<&str>) -> Result<PreparedEdit, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope { account_id: account.into(), origin, service:"calendar".into(), collection:collection.into() };
    if store.provider_accessible(&scope).map_err(|e| e.to_string())? != Some(true) { return Err("Calendar access is unknown or unavailable".into()); }
    let object = store.provider_object(&scope, href).map_err(|e| e.to_string())?.ok_or("Cached series missing")?;
    let expected_etag = object.etag.filter(|tag| !tag.is_empty() && !tag.starts_with("W/")).ok_or("Strong ETag required")?;
    let raw = isyncyou_connectors::dav::edit::series_recurrence(&object.raw, uid, rule)?;
    Ok(PreparedEdit { href:object.remote_id, expected_etag, expected_collection_token:None, raw })
}

/// Build the exact payload to be confirmed and persisted before creating a
/// recovered resource. The verified original remains with the recovery caller.
pub fn prepare_recovered_calendar(original: &[u8], uid_map: &std::collections::HashMap<String, String>) -> Result<Vec<u8>, String> {
    isyncyou_connectors::dav::edit::recovered_calendar(original, uid_map)
}

/// Strict byte comparison for a native recovery readback. A server-normalized
/// result remains unverified until a semantic comparison is implemented.
pub fn recovery_readback_matches(prepared: &[u8], current: &ProviderObject) -> bool {
    !prepared.is_empty() && prepared == current.raw.as_slice() && current.etag.as_ref().is_some_and(|etag| !etag.is_empty())
}

/// Domain views carry provider identity and observation metadata. Individual
/// invalid resources are explicit errors rather than silently discarded.
pub fn project_page(cfg: &Config, account: &str, collection: &str, service: &str, after: &str, limit: usize) -> Result<ProjectedPage, String> {
    let page = objects(cfg, account, collection, service, after, limit)?;
    let objects = page.objects.into_iter().map(|object| {
        let view = if service == "calendar" { isyncyou_connectors::dav::projection::calendar(&object.raw) }
            else { isyncyou_connectors::dav::projection::contact(&object.raw) };
        ProjectedObject { remote_id:object.remote_id, etag:object.etag, view }
    }).collect();
    Ok(ProjectedPage { account_id:page.account_id, origin:page.origin, accessible:page.accessible, observed_at:page.observed_at, cursor:page.cursor, objects })
}

/// Read a bounded keyset page without presenting cached bytes as live data.
/// Use an empty `after` for the first page and the final `remote_id` for the next.
pub fn objects(cfg: &Config, account: &str, collection: &str, service: &str, after: &str, limit: usize) -> Result<CachedObjects, String> {
    if !matches!(service, "calendar" | "contacts") { return Err("Unsupported DAV service".into()); }
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope { account_id: account.into(), origin, service: service.into(), collection: collection.into() };
    Ok(CachedObjects {
        account_id: scope.account_id.clone(),
        origin: scope.origin.clone(),
        accessible: store.provider_accessible(&scope).map_err(|e| e.to_string())?,
        observed_at: store.provider_observed_at(&scope).map_err(|e| e.to_string())?,
        cursor: store.provider_cursor(&scope).map_err(|e| e.to_string())?,
        objects: store.provider_objects_page(&scope, after, limit).map_err(|e| e.to_string())?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use isyncyou_store::ProviderDavIndex;

    #[test]
    fn parent_preparation_rejects_cycle_and_binds_collection_checkpoint() {
        let store = Store::open_in_memory().unwrap();
        let scope = ProviderScope {account_id:"fixture".into(), origin:"https://example.test".into(), service:"calendar".into(), collection:"https://example.test/cal/".into()};
        store.begin_provider_staging(&scope,"generation").unwrap();
        for (id,parent) in [("a",None),("b",Some("a"))] {
            let href = format!("https://example.test/cal/{id}.ics");
            let relationship = parent.map(|p| format!("RELATED-TO:{p}\r\n")).unwrap_or_default();
            let raw = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:{id}\r\n{relationship}END:VTODO\r\nEND:VCALENDAR\r\n").into_bytes();
            let object = ProviderObject {remote_id:href.clone(), etag:Some("\"v1\"".into()), raw};
            store.replace_provider_staged_object(&scope,"generation",&object).unwrap();
            let index = ProviderDavIndex::Valid {uid:id.into(),kind:"task".into(),parent_uid:parent.map(str::to_owned)};
            store.stage_provider_dav_index(&scope,"generation",&href,&index).unwrap();
        }
        store.finish_provider_staging(&scope,"generation","https://example.test/token/1").unwrap();
        assert!(prepare_task_parent_in_store(&store,&scope,"https://example.test/cal/a.ics","a",Some("b")).is_err());
        let edit = prepare_task_parent_in_store(&store,&scope,"https://example.test/cal/b.ics","b",None).unwrap();
        assert_eq!(edit.expected_collection_token.as_deref(),Some("https://example.test/token/1"));
        assert_eq!(edit.expected_etag,"\"v1\"");
    }
}
