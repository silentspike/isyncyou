//! Authenticated DAV discovery and collection sync for configured accounts.

use isyncyou_connectors::dav::{Collection, DavClient, DavError, ExpandedCalendar};
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
