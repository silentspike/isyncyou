//! Authenticated DAV discovery and collection sync for configured accounts.

use crate::provider_credential::ProviderCredentialStore;
use isyncyou_connectors::dav::{
    AclEntry, Collection, DavClient, DavError, DetachedSyncStage, ExpandedCalendar,
};
use isyncyou_core::{config::ProviderConfig, Config};
use isyncyou_store::{ProviderObject, ProviderScope, Store};

fn configured_origin(cfg: &Config, account: &str) -> Result<String, String> {
    match cfg.provider_accounts.get(account) {
        Some(ProviderConfig::SelfHosted { origin, .. }) => Ok(origin.clone()),
        None => Err("Not a self-hosted account".into()),
    }
}

fn configured(cfg: &Config, account: &str) -> Result<(String, Store), String> {
    let origin = configured_origin(cfg, account)?;
    let store = crate::provider_store::open(cfg, account)?;
    Ok((origin, store))
}

fn reject_known_dav_access_loss(
    cfg: &Config,
    account: &str,
    collection: &str,
    service: &str,
) -> Result<(), String> {
    let origin = configured_origin(cfg, account)?;
    let store = match crate::provider_store::open_readonly(cfg, account) {
        Ok(store) => store,
        Err(error) if error == "Provider cache is not initialized" => return Ok(()),
        Err(error) => return Err(error),
    };
    let scope = ProviderScope {
        account_id: account.into(),
        origin,
        service: service.into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        == Some(false)
    {
        return Err("DAV collection is not currently accessible".into());
    }
    Ok(())
}

fn client(cfg: &Config, account: &str, origin: &str) -> Result<DavClient, String> {
    let token = crate::provider_auth::self_hosted_access_token(cfg, account)?;
    DavClient::new(origin, token).map_err(|e| e.to_string())
}

fn client_with_store(
    cfg: &Config,
    account: &str,
    origin: &str,
    credentials: &dyn ProviderCredentialStore,
) -> Result<DavClient, String> {
    let token =
        crate::provider_auth::self_hosted_access_token_with_store(cfg, account, credentials)?;
    DavClient::new(origin, token).map_err(|e| e.to_string())
}

/// Resolve server-advertised collections for the configured account.
pub fn discover(cfg: &Config, account: &str, entry: &str) -> Result<Vec<Collection>, String> {
    let origin = configured_origin(cfg, account)?;
    client(cfg, account, &origin)?
        .discover(entry)
        .map_err(|e| e.to_string())
}

/// Platform credential variant for the same discovery path.
pub fn discover_with_store(
    cfg: &Config,
    account: &str,
    entry: &str,
    credentials: &dyn ProviderCredentialStore,
) -> Result<Vec<Collection>, String> {
    let origin = configured_origin(cfg, account)?;
    client_with_store(cfg, account, &origin, credentials)?
        .discover(entry)
        .map_err(|e| e.to_string())
}

/// Synchronize only a collection returned by authenticated discovery. Calendar
/// resources may contain VEVENT and VTODO, which intentionally share one scope.
pub fn sync_collection(
    cfg: &Config,
    account: &str,
    entry: &str,
    collection: &str,
    service: &str,
) -> Result<usize, String> {
    sync_collection_inner(cfg, account, entry, collection, service, None)
}

/// Platform credential variant for the same synchronization path.
pub fn sync_collection_with_store(
    cfg: &Config,
    account: &str,
    entry: &str,
    collection: &str,
    service: &str,
    credentials: &dyn ProviderCredentialStore,
) -> Result<usize, String> {
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
pub fn sync_all_with_store(
    cfg: &Config,
    account: &str,
    entry: &str,
    credentials: &dyn ProviderCredentialStore,
) -> Result<SyncPass, String> {
    sync_all_inner(cfg, account, entry, Some(credentials))
}

/// Desktop variant using the configured system keyring.
pub fn sync_all(cfg: &Config, account: &str, entry: &str) -> Result<SyncPass, String> {
    sync_all_inner(cfg, account, entry, None)
}

/// One cross-process lease per account/origin/collection keeps two detached
/// network observations from racing; it is independent of the provider writer.
fn collection_sync_lease(cfg: &Config, scope: &ProviderScope) -> Result<std::fs::File, String> {
    use fs2::FileExt;
    use sha2::{Digest, Sha256};
    let account = cfg
        .accounts
        .iter()
        .find(|a| a.id == scope.account_id)
        .ok_or("Account missing")?;
    std::fs::create_dir_all(&account.archive_root)
        .map_err(|_| "DAV archive directory unavailable")?;
    let mut h = Sha256::new();
    for value in [
        &scope.account_id,
        &scope.origin,
        &scope.service,
        &scope.collection,
    ] {
        h.update((value.len() as u64).to_be_bytes());
        h.update(value.as_bytes());
    }
    let path = account
        .archive_root
        .join(format!(".isyncyou-dav-sync-{:x}.lock", h.finalize()));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|_| "DAV sync lease unavailable")?;
    file.try_lock_exclusive()
        .map_err(|_| "DAV collection sync already active")?;
    Ok(file)
}

struct DetachedStage<'a> {
    store: Store,
    scope: &'a ProviderScope,
    generation: Option<String>,
    cursor: Option<String>,
    accessible: Option<bool>,
    full: bool,
}

impl DetachedStage<'_> {
    fn abort(&mut self) -> Result<(), DavError> {
        if let Some(generation) = self.generation.take() {
            self.store
                .abort_provider_staging(self.scope, &generation)
                .map_err(|e| DavError::Store(e.to_string()))?;
        }
        Ok(())
    }
}

impl DetachedSyncStage for DetachedStage<'_> {
    fn reset(&mut self, full: bool) -> Result<(), DavError> {
        use ring::rand::{SecureRandom, SystemRandom};
        self.abort()?;
        let store = &self.store;
        if store
            .provider_cursor(self.scope)
            .map_err(|e| DavError::Store(e.to_string()))?
            != self.cursor
            || store
                .provider_accessible(self.scope)
                .map_err(|e| DavError::Store(e.to_string()))?
                != self.accessible
        {
            return Err(DavError::Conflict);
        }
        let mut bytes = [0u8; 16];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| DavError::Transport("DAV staging random source unavailable".into()))?;
        let generation: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        store
            .begin_provider_staging(self.scope, &generation)
            .map_err(|e| DavError::Store(e.to_string()))?;
        self.generation = Some(generation);
        if self
            .store
            .provider_cursor(self.scope)
            .map_err(|e| DavError::Store(e.to_string()))?
            != self.cursor
            || self
                .store
                .provider_accessible(self.scope)
                .map_err(|e| DavError::Store(e.to_string()))?
                != self.accessible
        {
            return Err(DavError::Conflict);
        }
        self.full = full;
        Ok(())
    }
    fn existing(&mut self, id: &str) -> Result<Option<ProviderObject>, DavError> {
        self.store
            .provider_object(self.scope, id)
            .map_err(|e| DavError::Store(e.to_string()))
    }
    fn put(
        &mut self,
        object: &ProviderObject,
        index: &isyncyou_store::ProviderDavIndex,
        alarm: &isyncyou_store::ProviderAlarmIndex,
    ) -> Result<(), DavError> {
        let store = &self.store;
        let generation = self.generation.as_deref().ok_or(DavError::Conflict)?;
        store
            .replace_provider_staged_object(self.scope, generation, object)
            .map_err(|e| DavError::Store(e.to_string()))?;
        if self.scope.service == "calendar" {
            store
                .stage_provider_dav_index(self.scope, generation, &object.remote_id, index)
                .map_err(|e| DavError::Store(e.to_string()))?;
            store
                .stage_provider_alarm_index(self.scope, generation, &object.remote_id, alarm)
                .map_err(|e| DavError::Store(e.to_string()))?;
        }
        Ok(())
    }
    fn remove(&mut self, id: &str) -> Result<(), DavError> {
        let generation = self.generation.as_deref().ok_or(DavError::Conflict)?;
        if self.full {
            self.store
                .remove_provider_staged_object(self.scope, generation, id)
        } else {
            self.store
                .stage_provider_dav_tombstone(self.scope, generation, id)
        }
        .map_err(|e| DavError::Store(e.to_string()))
    }
    fn finish(&mut self, cursor: &str) -> Result<(), DavError> {
        let generation = self.generation.as_deref().ok_or(DavError::Conflict)?;
        if self.full {
            self.store
                .finish_provider_staging(self.scope, generation, cursor)
        } else {
            self.store
                .finish_provider_delta_staging(self.scope, generation, cursor)
        }
        .map_err(|e| DavError::Store(e.to_string()))?;
        self.generation = None;
        Ok(())
    }
}

impl Drop for DetachedStage<'_> {
    fn drop(&mut self) {
        let _ = self.abort();
    }
}

fn sync_detached(cfg: &Config, client: &DavClient, scope: &ProviderScope) -> Result<usize, String> {
    let _lease = collection_sync_lease(cfg, scope)?;
    let snapshot = match crate::provider_store::open_readonly(cfg, &scope.account_id) {
        Ok(store) => store,
        Err(error) if error == "Provider cache is not initialized" => {
            drop(crate::provider_store::open_for_session(
                cfg,
                &scope.account_id,
            )?);
            crate::provider_store::open_readonly(cfg, &scope.account_id)?
        }
        Err(error) => return Err(error),
    };
    let cursor = snapshot.provider_cursor(scope).map_err(|e| e.to_string())?;
    let accessible = snapshot
        .provider_accessible(scope)
        .map_err(|e| e.to_string())?;
    let full = scope.service == "calendar"
        && (snapshot
            .provider_dav_unindexed_count(scope)
            .map_err(|e| e.to_string())?
            > 0
            || snapshot
                .provider_alarm_unindexed_count(scope)
                .map_err(|e| e.to_string())?
                > 0);
    drop(snapshot);
    let _staging_lease = crate::provider_store::sync_staging_lease(cfg, &scope.account_id)?;
    let store = crate::provider_store::open_sync(cfg, &scope.account_id)?;
    let mut stage = DetachedStage {
        store,
        scope,
        generation: None,
        cursor: cursor.clone(),
        accessible,
        full,
    };
    let count = match client.sync_detached_staged(
        scope,
        cursor.clone(),
        accessible,
        full,
        cfg.sync.dav_page_size,
        &mut stage,
    ) {
        Ok(count) => count,
        Err(DavError::Http(403)) => {
            // A denial must not overwrite an access/cursor change observed by
            // another writer while this network request was in flight.
            let store = crate::provider_store::open_for_session(cfg, &scope.account_id)?;
            store
                .set_provider_accessible_if_state(scope, cursor.as_deref(), accessible, false)
                .map_err(|e| e.to_string())?;
            return Err("DAV collection access denied".into());
        }
        Err(error) => return Err(error.to_string()),
    };
    let store = crate::provider_store::open_for_session(cfg, &scope.account_id)?;
    store
        .backfill_provider_dav_search(scope, 100)
        .map_err(|e| e.to_string())?;
    Ok(count)
}

fn sync_all_inner(
    cfg: &Config,
    account: &str,
    entry: &str,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<SyncPass, String> {
    let origin = configured_origin(cfg, account)?;
    let client = match credentials {
        Some(credentials) => client_with_store(cfg, account, &origin, credentials)?,
        None => client(cfg, account, &origin)?,
    };
    let collections = client.discover(entry).map_err(|e| e.to_string())?;
    let mut report = SyncPass::default();
    let mut present = std::collections::HashSet::new();
    for collection in collections {
        for (enabled, service) in [
            (collection.calendar, "calendar"),
            (collection.addressbook, "contacts"),
        ] {
            if !enabled {
                continue;
            }
            report.collections += 1;
            present.insert((service.to_owned(), collection.url.clone()));
            let scope = ProviderScope {
                account_id: account.into(),
                origin: origin.clone(),
                service: service.into(),
                collection: collection.url.clone(),
            };
            match sync_detached(cfg, &client, &scope) {
                Ok(changes) => {
                    report.changes += changes;
                }
                Err(_) => report.failed += 1,
            }
        }
    }
    let store = crate::provider_store::open_for_session(cfg, account)?;
    for service in ["calendar", "contacts"] {
        for scope in store
            .provider_scopes(account, &origin, service)
            .map_err(|e| e.to_string())?
        {
            if !present.contains(&(service.to_owned(), scope.collection.clone())) {
                store
                    .set_provider_accessible(&scope, false)
                    .map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(report)
}

fn sync_collection_inner(
    cfg: &Config,
    account: &str,
    entry: &str,
    collection: &str,
    service: &str,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<usize, String> {
    if !matches!(service, "calendar" | "contacts") {
        return Err("Unsupported DAV service".into());
    }
    let origin = configured_origin(cfg, account)?;
    let client = match credentials {
        Some(credentials) => client_with_store(cfg, account, &origin, credentials)?,
        None => client(cfg, account, &origin)?,
    };
    let scope = ProviderScope {
        account_id: account.into(),
        origin,
        service: service.into(),
        collection: collection.into(),
    };
    let collections = match client.discover(entry) {
        Ok(collections) => collections,
        Err(DavError::Http(403)) => {
            let store = crate::provider_store::open_for_session(cfg, account)?;
            store
                .set_provider_accessible(&scope, false)
                .map_err(|e| e.to_string())?;
            return Err("Collection is not currently accessible".into());
        }
        Err(error) => return Err(error.to_string()),
    };
    if !collections.iter().any(|c| {
        c.url == collection
            && if service == "calendar" {
                c.calendar
            } else {
                c.addressbook
            }
    }) {
        let store = crate::provider_store::open_for_session(cfg, account)?;
        store
            .set_provider_accessible(&scope, false)
            .map_err(|e| e.to_string())?;
        return Err("Collection is not currently accessible".into());
    }
    sync_detached(cfg, &client, &scope)
}

/// Query one currently discoverable calendar's free/busy, not an aggregate
/// availability calculation for the calendar owner.
pub fn free_busy(
    cfg: &Config,
    account: &str,
    entry: &str,
    collection: &str,
    start: &str,
    end: &str,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<Vec<u8>, String> {
    let origin = configured_origin(cfg, account)?;
    let client = match credentials {
        Some(credentials) => client_with_store(cfg, account, &origin, credentials)?,
        None => client(cfg, account, &origin)?,
    };
    if !client
        .discover(entry)
        .map_err(|e| e.to_string())?
        .iter()
        .any(|c| c.url == collection && c.calendar)
    {
        return Err("Collection is not currently accessible".into());
    }
    let scope = ProviderScope {
        account_id: account.into(),
        origin,
        service: "calendar".into(),
        collection: collection.into(),
    };
    client
        .free_busy(&scope, start, end)
        .map_err(|e| e.to_string())
}

/// Read native collection sharing entries when the account has DAV read-acl.
pub fn read_acl(
    cfg: &Config,
    account: &str,
    entry: &str,
    collection: &str,
    service: &str,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<Vec<AclEntry>, String> {
    if !matches!(service, "calendar" | "contacts") {
        return Err("Unsupported DAV service".into());
    }
    let origin = configured_origin(cfg, account)?;
    let client = match credentials {
        Some(credentials) => client_with_store(cfg, account, &origin, credentials)?,
        None => client(cfg, account, &origin)?,
    };
    if !client
        .discover(entry)
        .map_err(|e| e.to_string())?
        .iter()
        .any(|c| {
            c.url == collection
                && if service == "calendar" {
                    c.calendar
                } else {
                    c.addressbook
                }
        })
    {
        return Err("Collection is not currently accessible".into());
    }
    let scope = ProviderScope {
        account_id: account.into(),
        origin,
        service: service.into(),
        collection: collection.into(),
    };
    client.acl(&scope).map_err(|e| e.to_string())
}

/// A live server-expanded event view; original ICS resources stay untouched.
pub fn expanded_events(
    cfg: &Config,
    account: &str,
    entry: &str,
    collection: &str,
    start: &str,
    end: &str,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<Vec<ExpandedCalendar>, String> {
    reject_known_dav_access_loss(cfg, account, collection, "calendar")?;
    let origin = configured_origin(cfg, account)?;
    let client = match credentials {
        Some(credentials) => client_with_store(cfg, account, &origin, credentials)?,
        None => client(cfg, account, &origin)?,
    };
    if !client
        .discover(entry)
        .map_err(|e| e.to_string())?
        .iter()
        .any(|c| c.url == collection && c.calendar)
    {
        return Err("Collection is not currently accessible".into());
    }
    let scope = ProviderScope {
        account_id: account.into(),
        origin,
        service: "calendar".into(),
        collection: collection.into(),
    };
    client
        .expanded_events(&scope, start, end)
        .map_err(|e| e.to_string())
}

/// Parse a live bounded recurrence expansion while retaining the resource's
/// native identity and ETag. This view never replaces stored source bytes.
pub fn expanded_event_views(
    cfg: &Config,
    account: &str,
    entry: &str,
    collection: &str,
    start: &str,
    end: &str,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<Vec<ProjectedObject>, String> {
    let events = expanded_events(cfg, account, entry, collection, start, end, credentials)?;
    Ok(events
        .into_iter()
        .map(|event| ProjectedObject {
            remote_id: event.href,
            etag: Some(event.etag),
            view: isyncyou_connectors::dav::projection::calendar(event.calendar_data.as_bytes()),
        })
        .collect())
}

pub fn expanded_task_views(
    cfg: &Config,
    account: &str,
    entry: &str,
    collection: &str,
    start: &str,
    end: &str,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<Vec<ProjectedObject>, String> {
    reject_known_dav_access_loss(cfg, account, collection, "calendar")?;
    let origin = configured_origin(cfg, account)?;
    let client = match credentials {
        Some(c) => client_with_store(cfg, account, &origin, c)?,
        None => client(cfg, account, &origin)?,
    };
    if !client
        .discover(entry)
        .map_err(|e| e.to_string())?
        .iter()
        .any(|c| c.url == collection && c.calendar)
    {
        return Err("Collection is not currently accessible".into());
    }
    let scope = ProviderScope {
        account_id: account.into(),
        origin,
        service: "calendar".into(),
        collection: collection.into(),
    };
    let tasks = client
        .expanded_tasks(&scope, start, end)
        .map_err(|e| e.to_string())?;
    Ok(tasks
        .into_iter()
        .map(|task| ProjectedObject {
            remote_id: task.href,
            etag: Some(task.etag),
            view: isyncyou_connectors::dav::projection::calendar(task.calendar_data.as_bytes()),
        })
        .collect())
}

/// Read a single currently accessible native object; useful for explicit
/// conflict inspection and restore verification, without changing the cache.
pub fn read_current(
    cfg: &Config,
    account: &str,
    entry: &str,
    collection: &str,
    service: &str,
    href: &str,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<ProviderObject, String> {
    if !matches!(service, "calendar" | "contacts") {
        return Err("Unsupported DAV service".into());
    }
    reject_known_dav_access_loss(cfg, account, collection, service)?;
    let origin = configured_origin(cfg, account)?;
    let client = match credentials {
        Some(credentials) => client_with_store(cfg, account, &origin, credentials)?,
        None => client(cfg, account, &origin)?,
    };
    if !client
        .discover(entry)
        .map_err(|e| e.to_string())?
        .iter()
        .any(|c| {
            c.url == collection
                && if service == "calendar" {
                    c.calendar
                } else {
                    c.addressbook
                }
        })
    {
        return Err("Collection is not currently accessible".into());
    }
    let scope = ProviderScope {
        account_id: account.into(),
        origin,
        service: service.into(),
        collection: collection.into(),
    };
    client
        .read_resource(&scope, href)
        .map_err(|e| e.to_string())
}

/// Read a personal DAV resource with the caller's already locked account token.
/// The caller opens its store before acquiring the authorization lock; this
/// read does not reopen it or acquire the lock recursively.
#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub(crate) fn read_current_with_token(
    cfg: &Config,
    account: &str,
    origin: &str,
    _store: &Store,
    entry: &str,
    collection: &str,
    service: &str,
    href: &str,
    token: &str,
) -> Result<ProviderObject, String> {
    if !matches!(service, "calendar" | "contacts") {
        return Err("Unsupported DAV service".into());
    }
    if !matches!(cfg.provider_accounts.get(account), Some(ProviderConfig::SelfHosted { origin: configured, .. }) if configured.as_str() == origin)
    {
        return Err("Account or provider changed".into());
    }
    let client = DavClient::new(origin, token.to_owned()).map_err(|e| e.to_string())?;
    if !client
        .discover(entry)
        .map_err(|e| e.to_string())?
        .iter()
        .any(|c| {
            c.url == collection
                && c.owned_by_current_principal == Some(true)
                && if service == "calendar" {
                    c.calendar
                } else {
                    c.addressbook
                }
        })
    {
        return Err("Personal collection is not currently accessible".into());
    }
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.into(),
        service: service.into(),
        collection: collection.into(),
    };
    if !client
        .permits(&scope, Some(href), "read")
        .map_err(|e| e.to_string())?
    {
        return Err("DAV resource is not readable".into());
    }
    let object = client
        .read_resource(&scope, href)
        .map_err(|e| e.to_string())?;
    if object
        .etag
        .as_deref()
        .filter(|etag| !etag.is_empty() && !etag.starts_with("W/"))
        .is_none()
    {
        return Err("Strong ETag required".into());
    }
    Ok(object)
}

/// Read only an embedded photo from the current authenticated CardDAV object.
/// Remote photo URLs are never fetched or returned to the browser.
pub fn read_contact_photo(
    cfg: &Config,
    account: &str,
    entry: &str,
    collection: &str,
    href: &str,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<Option<(String, Vec<u8>)>, String> {
    let object = read_current(
        cfg,
        account,
        entry,
        collection,
        "contacts",
        href,
        credentials,
    )?;
    let Some(uri) = isyncyou_connectors::dav::edit::contact_photo_uri(&object.raw)? else {
        return Ok(None);
    };
    decode_embedded_contact_photo(&uri).map(Some)
}

fn decode_embedded_contact_photo(uri: &str) -> Result<(String, Vec<u8>), String> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let (media, encoded) = if let Some(data) = uri.strip_prefix("data:image/png;base64,") {
        ("image/png", data)
    } else if let Some(data) = uri.strip_prefix("data:image/jpeg;base64,") {
        ("image/jpeg", data)
    } else {
        return Err("Unsupported contact photo URI".into());
    };
    if encoded.len() > (2usize * 1024 * 1024).div_ceil(3) * 4 {
        return Err("Contact photo exceeds limit".into());
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| "Invalid contact photo encoding")?;
    if bytes.is_empty()
        || bytes.len() > 2 * 1024 * 1024
        || !(media == "image/png" && bytes.starts_with(b"\x89PNG\r\n\x1a\n")
            || media == "image/jpeg" && bytes.starts_with(b"\xff\xd8\xff"))
    {
        return Err("Invalid contact photo".into());
    }
    Ok((media.into(), bytes))
}

pub struct ReminderPlanItem {
    pub identity: isyncyou_connectors::dav::reminders::Reminder,
    /// Full SHA-256 tag; Android uses this tag with a constant numeric ID.
    pub notification_tag: String,
}

/// Plan for one live resource over a caller-supplied occurrence window. It is
/// not an assertion that every account reminder has been enumerated.
pub struct ReminderPlan {
    pub scope: ProviderScope,
    pub href: String,
    pub etag: String,
    pub occurrence_start: String,
    pub occurrence_end: String,
    pub window_start: i64,
    pub window_end: i64,
    pub items: Vec<ReminderPlanItem>,
}

impl ReminderPlan {
    /// Produce native dispatch records for this resource only. An empty list
    /// does not establish complete account coverage or authorize cancellation.
    pub fn native_records(
        &self,
        floating_tzid: Option<&str>,
    ) -> Result<Vec<serde_json::Value>, String> {
        let mut records = Vec::with_capacity(self.items.len());
        for item in &self.items {
            let record = serde_json::json!({
                "account": self.scope.account_id, "collection": self.scope.collection,
                "href": self.href, "occurrence_start": self.occurrence_start,
                "occurrence_end": self.occurrence_end, "window_start": self.window_start,
                "window_end": self.window_end, "floating_tzid": floating_tzid,
                "uid": item.identity.uid, "recurrence_id": item.identity.recurrence_id,
                "alarm_id": item.identity.alarm_id, "trigger_at": item.identity.trigger_at,
                "notification_tag": item.notification_tag,
            });
            for key in [
                "account",
                "collection",
                "href",
                "occurrence_start",
                "occurrence_end",
                "uid",
                "recurrence_id",
                "alarm_id",
            ] {
                reminder_record(&record, key)?;
            }
            if floating_tzid.is_some() {
                reminder_record(&record, "floating_tzid")?;
            }
            if record.to_string().len() > 16384 {
                return Err("Reminder record exceeds native limit".into());
            }
            records.push(record);
        }
        Ok(records)
    }
}

/// A partial plan contains usable records, but does not establish full coverage.
pub struct ReminderAccountPlan {
    pub records: Vec<serde_json::Value>,
    pub unsupported_count: usize,
    pub completed_collections: Vec<String>,
    pub failed_collections: usize,
    pub failure_reason: Option<String>,
}

// Publish a collection only after its entire live plan and cursor checks succeed.
fn finish_reminder_collection(
    plan: &mut ReminderAccountPlan,
    collection: String,
    records: Vec<serde_json::Value>,
    unsupported: usize,
    result: Result<(), String>,
) {
    match result {
        Ok(()) => {
            plan.records.extend(records);
            plan.unsupported_count += unsupported;
            plan.completed_collections.push(collection);
        }
        Err(reason) => {
            plan.failed_collections += 1;
            if plan.failure_reason.is_none() {
                let code = match reason.as_str() {
                    "reminder_sync"
                    | "reminder_cache"
                    | "reminder_access_denied"
                    | "reminder_events_report"
                    | "reminder_tasks_report"
                    | "reminder_changed"
                    | "reminder_access_check"
                    | "reminder_resource_get" => reason,
                    _ => "reminder_plan".into(),
                };
                plan.failure_reason = Some(code);
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ReminderResourceCoverage {
    NoAlarms,
    Standalone,
    Expanded,
    Unsupported,
}

fn reminder_resource_coverage(raw: &[u8]) -> ReminderResourceCoverage {
    match isyncyou_connectors::dav::edit::resource_has_alarms(raw) {
        Ok(false) => return ReminderResourceCoverage::NoAlarms,
        Err(_) => return ReminderResourceCoverage::Unsupported,
        Ok(true) => {}
    }
    match isyncyou_connectors::dav::reminders::is_standalone(raw) {
        Ok(true) => ReminderResourceCoverage::Standalone,
        Ok(false)
            if isyncyou_connectors::dav::reminders::require_bounded_relative_alarms(raw)
                .is_ok() =>
        {
            ReminderResourceCoverage::Expanded
        }
        _ => ReminderResourceCoverage::Unsupported,
    }
}

/// Enumerate a bounded occurrence window across every currently discovered
/// calendar. Discovery errors abort the account pass; collection errors retain
/// only complete, authorized collections and mark the result as partial.
/// Unsupported individual resources are counted without exposing content.
/// This does not establish coverage outside the occurrence window.
#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn plan_account_reminders_with_store(
    cfg: &Config,
    account: &str,
    entry: &str,
    occurrence_start: &str,
    occurrence_end: &str,
    window_start: i64,
    window_end: i64,
    floating_tzid: Option<&str>,
    credentials: &dyn ProviderCredentialStore,
) -> Result<ReminderAccountPlan, String> {
    if window_end <= window_start
        || window_end.saturating_sub(window_start) > 31 * 86400
        || occurrence_start >= occurrence_end
    {
        return Err("reminder_window".into());
    }
    let start = isyncyou_connectors::dav::reminders::utc_window_bound(occurrence_start)
        .map_err(|_| "reminder_window")?;
    let end = isyncyou_connectors::dav::reminders::utc_window_bound(occurrence_end)
        .map_err(|_| "reminder_window")?;
    if start > window_start.saturating_sub(368 * 86400)
        || end < window_end.saturating_add(368 * 86400)
    {
        return Err("reminder_window".into());
    }
    let origin = configured_origin(cfg, account).map_err(|_| "reminder_config")?;
    let client =
        client_with_store(cfg, account, &origin, credentials).map_err(|_| "reminder_client")?;
    let collections = client.discover(entry).map_err(|_| "reminder_discovery")?;
    let mut plan = ReminderAccountPlan {
        records: Vec::new(),
        unsupported_count: 0,
        completed_collections: Vec::new(),
        failed_collections: 0,
        failure_reason: None,
    };
    let mut objects_seen = 0usize;
    for collection in collections.into_iter().filter(|c| c.calendar) {
        let scope = ProviderScope {
            account_id: account.into(),
            origin: origin.clone(),
            service: "calendar".into(),
            collection: collection.url,
        };
        let mut scope_records = Vec::new();
        let mut scope_unsupported = 0usize;
        let result: Result<(), String> = (|| {
            sync_detached(cfg, &client, &scope).map_err(|_| "reminder_sync")?;
            let store =
                crate::provider_store::open_readonly(cfg, account).map_err(|_| "reminder_cache")?;
            let initial_cursor = store
                .provider_cursor(&scope)
                .map_err(|_| "reminder_cache")?;
            let initial_access = store
                .provider_accessible(&scope)
                .map_err(|_| "reminder_cache")?;
            if initial_access != Some(true) {
                return Err("reminder_access_denied".into());
            }
            let mut expanded = client
                .expanded_events(&scope, occurrence_start, occurrence_end)
                .map_err(|_| "reminder_events_report")?;
            expanded.extend(
                client
                    .expanded_tasks(&scope, occurrence_start, occurrence_end)
                    .map_err(|_| "reminder_tasks_report")?,
            );
            let mut by_href = std::collections::HashMap::new();
            for item in expanded {
                if by_href.insert(item.href.clone(), item).is_some() {
                    return Err("reminder_changed".into());
                }
            }
            let mut after = String::new();
            loop {
                let page = store
                    .provider_objects_page(&scope, &after, 128)
                    .map_err(|_| "reminder_cache")?;
                if page.is_empty() {
                    break;
                }
                for object in &page {
                    objects_seen += 1;
                    if objects_seen > 10000 {
                        return Err("Reminder account resource limit exceeded".into());
                    }
                    let expansion = by_href.remove(&object.remote_id);
                    if expansion
                        .as_ref()
                        .is_some_and(|v| object.etag.as_deref() != Some(v.etag.as_str()))
                    {
                        return Err("reminder_changed".into());
                    }
                    let standalone = match reminder_resource_coverage(&object.raw) {
                        ReminderResourceCoverage::NoAlarms => continue,
                        ReminderResourceCoverage::Standalone => true,
                        ReminderResourceCoverage::Expanded => false,
                        ReminderResourceCoverage::Unsupported => {
                            scope_unsupported += 1;
                            continue;
                        }
                    };
                    if !standalone && expansion.is_none() {
                        continue;
                    }
                    if !client
                        .permits(&scope, Some(&object.remote_id), "read")
                        .map_err(|_| "reminder_access_check")?
                    {
                        return Err("reminder_access_denied".into());
                    }
                    let source = client
                        .read_resource(&scope, &object.remote_id)
                        .map_err(|_| "reminder_resource_get")?;
                    let etag = source.etag.clone().ok_or("Reminder ETag unavailable")?;
                    if object.etag.as_deref() != Some(etag.as_str()) {
                        return Err("reminder_changed".into());
                    }
                    let data = if standalone {
                        source.raw.as_slice()
                    } else {
                        expansion
                            .as_ref()
                            .ok_or("Missing expansion")?
                            .calendar_data
                            .as_bytes()
                    };
                    let planned = match isyncyou_connectors::dav::reminders::plan(
                        data,
                        &source.raw,
                        floating_tzid,
                        window_start,
                        window_end,
                    ) {
                        Ok(planned) => planned,
                        Err(_) => {
                            scope_unsupported += 1;
                            continue;
                        }
                    };
                    let items = planned
                        .into_iter()
                        .map(|identity| ReminderPlanItem {
                            notification_tag: identity.notification_tag(&scope),
                            identity,
                        })
                        .collect();
                    let item_plan = ReminderPlan {
                        scope: scope.clone(),
                        href: object.remote_id.clone(),
                        etag,
                        occurrence_start: occurrence_start.into(),
                        occurrence_end: occurrence_end.into(),
                        window_start,
                        window_end,
                        items,
                    };
                    scope_records.extend(item_plan.native_records(floating_tzid)?);
                    if plan.records.len() + scope_records.len() > 10000 {
                        return Err("Reminder account plan limit exceeded".into());
                    }
                }
                after = page
                    .last()
                    .ok_or("Reminder pagination failed")?
                    .remote_id
                    .clone();
            }
            if !by_href.is_empty() {
                return Err("reminder_changed".into());
            }
            if store
                .provider_cursor(&scope)
                .map_err(|_| "reminder_cache")?
                != initial_cursor
                || store
                    .provider_accessible(&scope)
                    .map_err(|_| "reminder_cache")?
                    != initial_access
            {
                return Err("reminder_changed".into());
            }
            Ok(())
        })();
        finish_reminder_collection(
            &mut plan,
            scope.collection,
            scope_records,
            scope_unsupported,
            result,
        );
    }
    Ok(plan)
}

/// A content-free revision for deciding whether a previously complete account
/// plan can be reused. A changed collection set, access state or sync cursor
/// changes the digest. Discovery and every incremental sync must succeed.
pub fn reminder_account_revision_with_store(
    cfg: &Config,
    account: &str,
    entry: &str,
    credentials: &dyn ProviderCredentialStore,
) -> Result<String, String> {
    let origin = configured_origin(cfg, account)?;
    let client = client_with_store(cfg, account, &origin, credentials)?;
    let mut collections: Vec<_> = client
        .discover(entry)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|c| c.calendar)
        .map(|c| c.url)
        .collect();
    collections.sort();
    if collections.len() > 1000 || collections.windows(2).any(|v| v[0] == v[1]) {
        return Err("Reminder collection set is invalid".into());
    }
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    for value in [account, &origin] {
        digest.update(&(value.len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
    }
    for collection in collections {
        let scope = ProviderScope {
            account_id: account.into(),
            origin: origin.clone(),
            service: "calendar".into(),
            collection,
        };
        sync_detached(cfg, &client, &scope)?;
        let store = crate::provider_store::open_readonly(cfg, account)?;
        if store
            .provider_accessible(&scope)
            .map_err(|e| e.to_string())?
            != Some(true)
        {
            return Err("Reminder calendar access unavailable".into());
        }
        let cursor = store
            .provider_cursor(&scope)
            .map_err(|e| e.to_string())?
            .filter(|v| !v.is_empty())
            .ok_or("Reminder calendar cursor unavailable")?;
        for value in [&scope.collection, &cursor] {
            digest.update(&(value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
    }
    Ok(digest
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

pub enum ReminderClaim {
    /// Persisted pending before native notification dispatch.
    New { notification_tag: String },
    /// A prior attempt is pending; retry only with this same platform tag.
    Pending { notification_tag: String },
    /// The operating system previously accepted the notification.
    Delivered,
}

// The synchronous native claim path keeps only a content-free phase on its
// calling thread. No account, URL, item or upstream error leaves this seam.
thread_local! {
    static REMINDER_CLAIM_PHASE: std::cell::Cell<&'static str> = const { std::cell::Cell::new("record") };
}

fn reminder_claim_phase(phase: &'static str) {
    REMINDER_CLAIM_PHASE.with(|current| current.set(phase));
}

/// Re-read one event, enforce current read permission, and generate a bounded
/// plan only when the expanded data and original carry the same native ETag.
#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn plan_event_reminders(
    cfg: &Config,
    account: &str,
    entry: &str,
    collection: &str,
    href: &str,
    occurrence_start: &str,
    occurrence_end: &str,
    window_start: i64,
    window_end: i64,
    floating_tzid: Option<&str>,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<ReminderPlan, String> {
    reminder_claim_phase("credentials");
    let origin = configured_origin(cfg, account)?;
    let client = match credentials {
        Some(credentials) => client_with_store(cfg, account, &origin, credentials)?,
        None => client(cfg, account, &origin)?,
    };
    reminder_claim_phase("discovery");
    if !client
        .discover(entry)
        .map_err(|e| e.to_string())?
        .iter()
        .any(|c| c.url == collection && c.calendar)
    {
        return Err("Calendar is not currently accessible".into());
    }
    let scope = ProviderScope {
        account_id: account.into(),
        origin,
        service: "calendar".into(),
        collection: collection.into(),
    };
    let target = client
        .validate_target(&scope, href)
        .map_err(|e| e.to_string())?;
    reminder_claim_phase("acl");
    if !client
        .permits(&scope, Some(&target), "read")
        .map_err(|e| e.to_string())?
    {
        return Err("Event is not currently readable".into());
    }
    reminder_claim_phase("get");
    let source = client
        .read_resource(&scope, &target)
        .map_err(|e| e.to_string())?;
    let etag = source.etag.clone().ok_or("Event ETag unavailable")?;
    reminder_claim_phase("interpretation");
    let planned = if isyncyou_connectors::dav::reminders::is_standalone(&source.raw)? {
        isyncyou_connectors::dav::reminders::plan(
            &source.raw,
            &source.raw,
            floating_tzid,
            window_start,
            window_end,
        )?
    } else {
        reminder_claim_phase("expansion");
        let mut expanded_items = client
            .expanded_events(&scope, occurrence_start, occurrence_end)
            .map_err(|e| e.to_string())?;
        expanded_items.extend(
            client
                .expanded_tasks(&scope, occurrence_start, occurrence_end)
                .map_err(|e| e.to_string())?,
        );
        let mut matching = expanded_items
            .into_iter()
            .filter(|event| event.href == target);
        let expanded = matching
            .next()
            .ok_or("No occurrence returned for requested window")?;
        if matching.next().is_some() || expanded.etag != etag {
            return Err("Event changed during reminder planning".into());
        }
        reminder_claim_phase("interpretation");
        isyncyou_connectors::dav::reminders::plan(
            expanded.calendar_data.as_bytes(),
            &source.raw,
            floating_tzid,
            window_start,
            window_end,
        )?
    };
    let items = planned
        .into_iter()
        .map(|identity| ReminderPlanItem {
            notification_tag: identity.notification_tag(&scope),
            identity,
        })
        .collect();
    Ok(ReminderPlan {
        scope,
        href: target,
        etag,
        occurrence_start: occurrence_start.into(),
        occurrence_end: occurrence_end.into(),
        window_start,
        window_end,
        items,
    })
}

/// Re-plan immediately before dispatch, then persist one due alarm claim. The
/// caller must use the returned tag as an idempotent native notification ID.
#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn claim_event_reminder(
    cfg: &Config,
    account: &str,
    entry: &str,
    collection: &str,
    href: &str,
    occurrence_start: &str,
    occurrence_end: &str,
    window_start: i64,
    window_end: i64,
    floating_tzid: Option<&str>,
    desired: &isyncyou_connectors::dav::reminders::Reminder,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<ReminderClaim, String> {
    reminder_claim_phase("timing");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "System time unavailable")?
        .as_secs() as i64;
    if desired.trigger_at > now || desired.trigger_at < now.saturating_sub(86400) {
        return Err("Reminder is not due or has expired".into());
    }
    let plan = plan_event_reminders(
        cfg,
        account,
        entry,
        collection,
        href,
        occurrence_start,
        occurrence_end,
        window_start,
        window_end,
        floating_tzid,
        credentials,
    )?;
    reminder_claim_phase("interpretation");
    let item = plan
        .items
        .iter()
        .find(|item| item.identity == *desired)
        .ok_or("Reminder changed or is no longer active")?;
    let alarm = isyncyou_store::ProviderAlarmIdentity {
        uid: desired.uid.clone(),
        recurrence_id: desired.recurrence_id.clone(),
        alarm_id: desired.alarm_id.clone(),
        trigger_at: desired.trigger_at.to_string(),
    };
    reminder_claim_phase("ledger");
    let store = crate::provider_store::open(cfg, account)?;
    if store
        .claim_provider_alarm(&plan.scope, &alarm)
        .map_err(|e| e.to_string())?
    {
        return Ok(ReminderClaim::New {
            notification_tag: item.notification_tag.clone(),
        });
    }
    match store
        .provider_alarm_state(&plan.scope, &alarm)
        .map_err(|e| e.to_string())?
        .as_deref()
    {
        Some("pending") => Ok(ReminderClaim::Pending {
            notification_tag: item.notification_tag.clone(),
        }),
        Some("delivered") => Ok(ReminderClaim::Delivered),
        _ => Err("Reminder claim state unavailable".into()),
    }
}

/// Mark OS-accepted delivery. This does not assert a person saw the alert.
pub fn acknowledge_event_reminder(
    cfg: &Config,
    account: &str,
    collection: &str,
    identity: &isyncyou_connectors::dav::reminders::Reminder,
) -> Result<bool, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin,
        service: "calendar".into(),
        collection: collection.into(),
    };
    let alarm = isyncyou_store::ProviderAlarmIdentity {
        uid: identity.uid.clone(),
        recurrence_id: identity.recurrence_id.clone(),
        alarm_id: identity.alarm_id.clone(),
        trigger_at: identity.trigger_at.to_string(),
    };
    store
        .finish_provider_alarm(&scope, &alarm)
        .map_err(|e| e.to_string())
}

fn reminder_record<'a>(record: &'a serde_json::Value, key: &str) -> Result<&'a str, String> {
    record
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| {
            !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control)
        })
        .ok_or_else(|| format!("Invalid reminder {key}"))
}

fn reminder_identity(
    record: &serde_json::Value,
) -> Result<isyncyou_connectors::dav::reminders::Reminder, String> {
    Ok(isyncyou_connectors::dav::reminders::Reminder {
        uid: reminder_record(record, "uid")?.into(),
        recurrence_id: reminder_record(record, "recurrence_id")?.into(),
        alarm_id: reminder_record(record, "alarm_id")?.into(),
        trigger_at: record
            .get("trigger_at")
            .and_then(serde_json::Value::as_i64)
            .ok_or("Invalid reminder trigger")?,
    })
}

/// Report only the bounded phase on a native claim failure; the original
/// claim result and the existing retry behavior remain unchanged.
pub fn claim_reminder_record_diagnostic(
    cfg: &Config,
    account: &str,
    entry: &str,
    record: &serde_json::Value,
    credentials: &dyn ProviderCredentialStore,
) -> Result<serde_json::Value, &'static str> {
    reminder_claim_phase("record");
    claim_reminder_record(cfg, account, entry, record, credentials)
        .map_err(|_| REMINDER_CLAIM_PHASE.with(std::cell::Cell::get))
}

/// Native-only bridge. The caller supplies a persisted engine-produced record;
/// live re-planning validates every effect again before a durable claim.
pub fn claim_reminder_record(
    cfg: &Config,
    account: &str,
    entry: &str,
    record: &serde_json::Value,
    credentials: &dyn ProviderCredentialStore,
) -> Result<serde_json::Value, String> {
    if reminder_record(record, "account")? != account {
        return Err("Reminder account mismatch".into());
    }
    let identity = reminder_identity(record)?;
    let collection = reminder_record(record, "collection")?;
    let href = reminder_record(record, "href")?;
    let start = reminder_record(record, "occurrence_start")?;
    let end = reminder_record(record, "occurrence_end")?;
    let window_start = record
        .get("window_start")
        .and_then(serde_json::Value::as_i64)
        .ok_or("Invalid reminder window")?;
    let window_end = record
        .get("window_end")
        .and_then(serde_json::Value::as_i64)
        .ok_or("Invalid reminder window")?;
    let timezone = match record.get("floating_tzid") {
        None | Some(serde_json::Value::Null) => None,
        Some(_) => Some(reminder_record(record, "floating_tzid")?),
    };
    match claim_event_reminder(
        cfg,
        account,
        entry,
        collection,
        href,
        start,
        end,
        window_start,
        window_end,
        timezone,
        &identity,
        Some(credentials),
    )? {
        ReminderClaim::New { notification_tag } => {
            Ok(serde_json::json!({"status":"new","notification_tag":notification_tag}))
        }
        ReminderClaim::Pending { notification_tag } => {
            Ok(serde_json::json!({"status":"pending","notification_tag":notification_tag}))
        }
        ReminderClaim::Delivered => Ok(serde_json::json!({"status":"delivered"})),
    }
}

/// Native adapter calls this only after the OS accepted the same notification
/// tag. The tag is re-derived from the current account/origin and identity.
pub fn acknowledge_reminder_record(
    cfg: &Config,
    account: &str,
    record: &serde_json::Value,
    notification_tag: &str,
) -> Result<bool, String> {
    if reminder_record(record, "account")? != account {
        return Err("Reminder account mismatch".into());
    }
    let collection = reminder_record(record, "collection")?;
    let identity = reminder_identity(record)?;
    let origin = match cfg.provider_accounts.get(account) {
        Some(ProviderConfig::SelfHosted { origin, .. }) => origin.clone(),
        _ => return Err("Provider unavailable".into()),
    };
    let scope = ProviderScope {
        account_id: account.into(),
        origin,
        service: "calendar".into(),
        collection: collection.into(),
    };
    if identity.notification_tag(&scope) != notification_tag {
        return Err("Reminder notification identity mismatch".into());
    }
    acknowledge_event_reminder(cfg, account, collection, &identity)
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

/// A jCard field retains its RFC 7095 type, parameters, and complete value.
pub struct ContactField {
    pub value_type: String,
    pub parameters: serde_json::Value,
    pub values: Vec<serde_json::Value>,
}

pub struct ContactSummary {
    pub names: Vec<ContactField>,
    pub emails: Vec<ContactField>,
    pub phones: Vec<ContactField>,
    pub organizations: Vec<ContactField>,
}

pub struct ContactPageItem {
    pub href: String,
    pub collection: String,
    pub etag: Option<String>,
    /// Invalid resources remain visible as an error instead of being omitted.
    pub summary: Result<ContactSummary, String>,
}

pub struct ContactPage {
    pub account_id: String,
    pub origin: String,
    pub collection: String,
    pub accessible: Option<bool>,
    pub observed_at: Option<i64>,
    pub cursor: Option<String>,
    pub items: Vec<ContactPageItem>,
    /// Continue until an empty page; a nonempty page never claims completion.
    pub next_after: Option<String>,
}

fn contact_summary(jcard: &serde_json::Value) -> Result<ContactSummary, String> {
    let array = jcard.as_array().ok_or("Invalid jCard")?;
    if array.len() != 2 || array[0].as_str() != Some("vcard") {
        return Err("Invalid jCard".into());
    }
    let properties = array[1].as_array().ok_or("Invalid jCard properties")?;
    let mut summary = ContactSummary {
        names: Vec::new(),
        emails: Vec::new(),
        phones: Vec::new(),
        organizations: Vec::new(),
    };
    for property in properties {
        let fields = property.as_array().ok_or("Invalid jCard property")?;
        let name = fields
            .first()
            .and_then(|v| v.as_str())
            .ok_or("Invalid jCard property")?;
        let destination = match name.to_ascii_lowercase().as_str() {
            "fn" => &mut summary.names,
            "email" => &mut summary.emails,
            "tel" => &mut summary.phones,
            "org" => &mut summary.organizations,
            _ => continue,
        };
        if fields.len() < 4 || !fields[1].is_object() {
            return Err("Invalid jCard property".into());
        }
        let value_type = fields[2].as_str().ok_or("Invalid jCard property type")?;
        destination.push(ContactField {
            value_type: value_type.into(),
            parameters: fields[1].clone(),
            values: fields[3..].to_vec(),
        });
    }
    if summary.names.is_empty() {
        return Err("Contact name is missing".into());
    }
    Ok(summary)
}

/// Read one bounded collection page; source bytes and unrecognized fields are
/// retained in the provider store. Provenance describes cache observation only.
pub fn contact_page(
    cfg: &Config,
    account: &str,
    collection: &str,
    after: &str,
    limit: usize,
) -> Result<ContactPage, String> {
    if limit == 0 || limit > 200 {
        return Err("Invalid contact page limit".into());
    }
    let page = project_page(cfg, account, collection, "contacts", after, limit)?;
    let next_after = page.objects.last().map(|object| object.remote_id.clone());
    let items = page
        .objects
        .into_iter()
        .map(|object| ContactPageItem {
            href: object.remote_id,
            collection: collection.into(),
            etag: object.etag,
            summary: object.view.and_then(|view| contact_summary(&view)),
        })
        .collect();
    Ok(ContactPage {
        account_id: page.account_id,
        origin: page.origin,
        collection: collection.into(),
        accessible: page.accessible,
        observed_at: page.observed_at,
        cursor: page.cursor,
        items,
        next_after,
    })
}

pub struct CalendarPageItem {
    pub href: String,
    pub collection: String,
    pub etag: Option<String>,
    /// Native VEVENT/VTODO projections and full jCal, including overrides.
    pub view: Result<serde_json::Value, String>,
}

pub struct CalendarPage {
    pub account_id: String,
    pub origin: String,
    pub collection: String,
    pub accessible: Option<bool>,
    /// Local cache commit timestamp, not server freshness.
    pub observed_at: Option<i64>,
    pub cursor: Option<String>,
    pub items: Vec<CalendarPageItem>,
    /// Continue until an empty page; no multi-page snapshot is implied.
    pub next_after: Option<String>,
}

/// Read native event and task resources together: CalDAV collections may
/// contain both, so callers must filter components without dropping overrides.
/// Recurrence is not expanded by this cached view.
pub fn calendar_page(
    cfg: &Config,
    account: &str,
    collection: &str,
    after: &str,
    limit: usize,
) -> Result<CalendarPage, String> {
    if limit == 0 || limit > 200 {
        return Err("Invalid calendar page limit".into());
    }
    let page = project_page(cfg, account, collection, "calendar", after, limit)?;
    let next_after = page.objects.last().map(|object| object.remote_id.clone());
    let items = page
        .objects
        .into_iter()
        .map(|object| CalendarPageItem {
            href: object.remote_id,
            collection: collection.into(),
            etag: object.etag,
            view: object.view,
        })
        .collect();
    Ok(CalendarPage {
        account_id: page.account_id,
        origin: page.origin,
        collection: collection.into(),
        accessible: page.accessible,
        observed_at: page.observed_at,
        cursor: page.cursor,
        items,
        next_after,
    })
}

/// Exact bytes and version proposed for a later effect-bound confirmation.
pub struct PreparedEdit {
    pub href: String,
    pub expected_etag: String,
    /// Required on edits that depend on the entire collection state.
    pub expected_collection_token: Option<String>,
    pub raw: Vec<u8>,
}

/// A constrained, effect-bound resource deletion; callers display the full
/// projection and identity and dispatch with the captured strong ETag.
pub struct PreparedDeletePreview {
    pub origin: String,
    pub href: String,
    pub expected_etag: String,
    pub kind: String,
    pub uid: String,
    pub name: String,
    pub source_sha256: String,
    pub projection: serde_json::Value,
    pub photos: Vec<DeletedPhotoSummary>,
}

pub struct DeletedPhotoSummary {
    pub value_type: String,
    pub encoded_size: usize,
    pub image_size: Option<usize>,
    pub value_sha256: String,
}

fn redact_contact_photos(
    projection: &mut serde_json::Value,
) -> Result<Vec<DeletedPhotoSummary>, String> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use sha2::{Digest, Sha256};
    let properties = projection
        .as_array_mut()
        .and_then(|root| root.get_mut(1))
        .and_then(serde_json::Value::as_array_mut)
        .ok_or("Invalid contact projection")?;
    let mut retained = Vec::with_capacity(properties.len());
    let mut photos = Vec::new();
    for property in std::mem::take(properties) {
        let name = property
            .as_array()
            .and_then(|parts| parts.first())
            .and_then(serde_json::Value::as_str)
            .ok_or("Invalid contact property")?;
        let is_photo = name.eq_ignore_ascii_case("photo");
        if !is_photo {
            if ["logo", "sound", "key"]
                .iter()
                .any(|key| name.eq_ignore_ascii_case(key))
                || property.to_string().len() > 64 * 1024
            {
                return Err("Contact contains data requiring an expanded delete preview".into());
            }
            retained.push(property);
            continue;
        }
        let parts = property.as_array().ok_or("Invalid photo property")?;
        if parts.len() != 4 {
            return Err("Photo effect cannot be summarized safely".into());
        }
        let value_type = parts[2].as_str().ok_or("Invalid photo type")?.to_owned();
        let value = parts[3]
            .as_str()
            .ok_or("Photo effect cannot be summarized safely")?;
        let image_size = value
            .strip_prefix("data:image/png;base64,")
            .or_else(|| value.strip_prefix("data:image/jpeg;base64,"))
            .and_then(|encoded| STANDARD.decode(encoded).ok())
            .map(|bytes| bytes.len());
        photos.push(DeletedPhotoSummary {
            value_type,
            encoded_size: value.len(),
            image_size,
            value_sha256: format!("{:x}", Sha256::digest(value.as_bytes())),
        });
    }
    *properties = retained;
    Ok(photos)
}

pub struct PreparedCreatePreview {
    pub origin: String,
    pub collection: String,
    pub service: String,
    pub href: String,
    pub kind: String,
    pub uid: String,
    pub timestamp: String,
    pub name: String,
    pub start: Option<String>,
    pub end: Option<String>,
    pub raw: Vec<u8>,
}

/// Prepare a new non-recurring, unscheduled item with a fresh random identity.
/// The complete returned effect is confirmed before create-if-absent dispatch.
pub fn prepare_create_simple(
    cfg: &Config,
    account: &str,
    collection: &str,
    kind: &str,
    name: &str,
    start: Option<&str>,
    end: Option<&str>,
) -> Result<PreparedCreatePreview, String> {
    use ring::rand::{SecureRandom, SystemRandom};
    let service = match kind {
        "event" | "task" => "calendar",
        "contact" => "contacts",
        _ => return Err("Unsupported new resource kind".into()),
    };
    let (origin, store) = configured(cfg, account)?;
    let base = reqwest::Url::parse(collection).map_err(|_| "Invalid collection URL")?;
    let provider = reqwest::Url::parse(&origin).map_err(|_| "Invalid provider origin")?;
    if base.origin() != provider.origin()
        || !base.path().ends_with('/')
        || base.query().is_some()
        || base.fragment().is_some()
        || !base.username().is_empty()
        || base.password().is_some()
    {
        return Err("Invalid collection URL".into());
    }
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.clone(),
        service: service.into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Collection access is unknown or unavailable".into());
    }
    let mut random = [0u8; 16];
    SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| "Resource identity generation failed")?;
    let uid: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let href = base
        .join(&format!(
            "{}.{}",
            uid,
            if kind == "contact" { "vcf" } else { "ics" }
        ))
        .map_err(|_| "Invalid resource URL")?
        .to_string();
    let timestamp = isyncyou_connectors::dav::edit::current_task_completion_time()?;
    let raw =
        isyncyou_connectors::dav::edit::create_simple(kind, &uid, &timestamp, name, start, end)?;
    Ok(PreparedCreatePreview {
        origin,
        collection: collection.into(),
        service: service.into(),
        href,
        kind: kind.into(),
        uid,
        timestamp,
        name: name.into(),
        start: start.map(str::to_owned),
        end: end.map(str::to_owned),
        raw,
    })
}

/// Prepare a single bounded recurring task create and its exact displayed
/// occurrences; identity and timestamp are generated exactly once.
pub fn prepare_create_task_series(
    cfg: &Config,
    account: &str,
    collection: &str,
    name: &str,
    start: &str,
    due: &str,
    rule: &str,
) -> Result<(PreparedCreatePreview, Vec<String>), String> {
    let mut create = prepare_create_simple(cfg, account, collection, "task", name, None, None)?;
    let result = isyncyou_connectors::dav::edit::create_task_series(
        &create.uid,
        &create.timestamp,
        name,
        start,
        due,
        rule,
    )?;
    create.start = Some(start.into());
    create.end = Some(due.into());
    create.raw = result.raw;
    Ok((create, result.new_occurrences))
}

pub struct PreparedInvitationCreate {
    pub create: PreparedCreatePreview,
    pub organizer: String,
    pub attendees: Vec<String>,
}

/// Construct one invitation only for an address owned by this principal.
#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn prepare_invitation_create(
    cfg: &Config,
    account: &str,
    entry: &str,
    collection: &str,
    name: &str,
    start: &str,
    end: &str,
    organizer: &str,
    attendees: &[String],
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<PreparedInvitationCreate, String> {
    let mut create = prepare_create_simple(
        cfg,
        account,
        collection,
        "event",
        name,
        Some(start),
        Some(end),
    )?;
    let client = match credentials {
        Some(c) => client_with_store(cfg, account, &create.origin, c)?,
        None => client(cfg, account, &create.origin)?,
    };
    if !client
        .discover(entry)
        .map_err(|e| e.to_string())?
        .iter()
        .any(|c| c.calendar && c.url == collection)
    {
        return Err("Calendar is not currently accessible".into());
    }
    if !client
        .calendar_user_addresses(entry)
        .map_err(|e| e.to_string())?
        .iter()
        .any(|address| address == organizer)
    {
        return Err("Organizer is not an authenticated calendar identity".into());
    }
    create.raw = isyncyou_connectors::dav::edit::create_invitation(
        &create.uid,
        &create.timestamp,
        name,
        start,
        end,
        organizer,
        attendees,
    )?;
    Ok(PreparedInvitationCreate {
        create,
        organizer: organizer.into(),
        attendees: attendees.to_vec(),
    })
}

pub fn prepare_delete_preview(
    cfg: &Config,
    account: &str,
    collection: &str,
    service: &str,
    href: &str,
) -> Result<PreparedDeletePreview, String> {
    use sha2::{Digest, Sha256};
    if !matches!(service, "calendar" | "contacts") {
        return Err("Unsupported DAV service".into());
    }
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.clone(),
        service: service.into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Collection access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached resource missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let (kind, uid, name, mut projection) = if service == "calendar" {
        let (uid, name) = isyncyou_connectors::dav::edit::preview_event_delete(&object.raw)?;
        (
            "event",
            uid,
            name,
            isyncyou_connectors::dav::projection::calendar(&object.raw)?,
        )
    } else {
        let (uid, name) = isyncyou_connectors::dav::edit::preview_contact_delete(&object.raw)?;
        (
            "contact",
            uid,
            name,
            isyncyou_connectors::dav::projection::contact(&object.raw)?,
        )
    };
    let photos = if service == "contacts" {
        redact_contact_photos(&mut projection)?
    } else {
        Vec::new()
    };
    if projection.to_string().len() > 96 * 1024 || photos.len() > 64 {
        return Err("Deletion effect is too large to preview safely".into());
    }
    Ok(PreparedDeletePreview {
        origin,
        href: object.remote_id,
        expected_etag,
        kind: kind.into(),
        uid,
        name,
        source_sha256: format!("{:x}", Sha256::digest(&object.raw)),
        projection,
        photos,
    })
}

/// A preview and exact replacement based on one cached native resource version.
pub struct PreparedContactName {
    pub origin: String,
    pub old_name: String,
    pub edit: PreparedEdit,
}

pub struct PreparedContactField {
    pub origin: String,
    pub key: String,
    pub index: usize,
    pub version: String,
    pub property_name: String,
    pub parameters: Vec<(String, Vec<String>)>,
    pub value_type: String,
    pub old_value: String,
    pub new_value: String,
    pub edit: PreparedEdit,
}

pub fn prepare_contact_text_preview(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    key: &str,
    index: usize,
    value: &str,
) -> Result<PreparedContactField, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.clone(),
        service: "contacts".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Address book access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached contact missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let field =
        isyncyou_connectors::dav::edit::preview_contact_field(&object.raw, key, index, value)?;
    Ok(PreparedContactField {
        origin,
        key: key.into(),
        index,
        version: field.version,
        property_name: field.property_name,
        parameters: field.parameters,
        value_type: field.value_type,
        old_value: field.old_value,
        new_value: field.new_value,
        edit: PreparedEdit {
            href: object.remote_id,
            expected_etag,
            expected_collection_token: None,
            raw: field.raw,
        },
    })
}

pub struct PreparedContactAddition {
    pub origin: String,
    pub key: String,
    pub version: String,
    pub value_type: String,
    pub new_value: String,
    pub edit: PreparedEdit,
}

pub fn prepare_contact_add_preview(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    key: &str,
    value: &str,
    value_type: &str,
) -> Result<PreparedContactAddition, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.clone(),
        service: "contacts".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Address book access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached contact missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let field =
        isyncyou_connectors::dav::edit::preview_contact_add(&object.raw, key, value, value_type)?;
    Ok(PreparedContactAddition {
        origin,
        key: key.into(),
        version: field.version,
        value_type: field.value_type,
        new_value: field.new_value,
        edit: PreparedEdit {
            href: object.remote_id,
            expected_etag,
            expected_collection_token: None,
            raw: field.raw,
        },
    })
}

pub fn prepare_contact_remove_preview(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    key: &str,
    index: usize,
) -> Result<PreparedContactField, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.clone(),
        service: "contacts".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Address book access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached contact missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let field = isyncyou_connectors::dav::edit::preview_contact_remove(&object.raw, key, index)?;
    Ok(PreparedContactField {
        origin,
        key: key.into(),
        index,
        version: field.version,
        property_name: field.property_name,
        parameters: field.parameters,
        value_type: field.value_type,
        old_value: field.old_value,
        new_value: field.new_value,
        edit: PreparedEdit {
            href: object.remote_id,
            expected_etag,
            expected_collection_token: None,
            raw: field.raw,
        },
    })
}

pub struct PreparedContactPhoto {
    pub origin: String,
    pub uid: String,
    pub name: String,
    pub media_type: String,
    pub size: usize,
    pub old_photo_uri_sha256: Option<String>,
    pub new_photo_sha256: String,
    pub edit: PreparedEdit,
}

/// Prepare a bounded embedded image; no remote photo URL is fetched.
pub fn prepare_contact_photo(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    media_type: &str,
    photo: &[u8],
) -> Result<PreparedContactPhoto, String> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use sha2::{Digest, Sha256};
    if photo.is_empty()
        || photo.len() > 2 * 1024 * 1024
        || !match media_type {
            "image/png" => photo.starts_with(b"\x89PNG\r\n\x1a\n"),
            "image/jpeg" => photo.starts_with(b"\xff\xd8\xff"),
            _ => false,
        }
    {
        return Err("Invalid or unsupported contact photo".into());
    }
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.clone(),
        service: "contacts".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Address book access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached contact missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let (uid, name) = isyncyou_connectors::dav::edit::unique_contact_identity(&object.raw)?;
    let uri = format!("data:{media_type};base64,{}", STANDARD.encode(photo));
    let (old, raw) = isyncyou_connectors::dav::edit::contact_photo(&object.raw, &uri)?;
    Ok(PreparedContactPhoto {
        origin,
        uid,
        name,
        media_type: media_type.into(),
        size: photo.len(),
        old_photo_uri_sha256: old.map(|bytes| format!("{:x}", Sha256::digest(bytes))),
        new_photo_sha256: format!("{:x}", Sha256::digest(photo)),
        edit: PreparedEdit {
            href: object.remote_id,
            expected_etag,
            expected_collection_token: None,
            raw,
        },
    })
}

pub fn prepare_contact_name(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    new_name: &str,
) -> Result<PreparedContactName, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.clone(),
        service: "contacts".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Address book access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached contact missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let (old_name, raw) =
        isyncyou_connectors::dav::edit::edit_unique_contact_name(&object.raw, new_name)?;
    Ok(PreparedContactName {
        origin,
        old_name,
        edit: PreparedEdit {
            href: object.remote_id,
            expected_etag,
            expected_collection_token: None,
            raw,
        },
    })
}

/// Prepare an edit on one VEVENT/VTODO master or exception from the cached
/// original. No operation is dispatched; the host must bind approval to the
/// returned bytes, account, target, version, and scheduling effects.
pub fn prepare_text_edit(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    uid: &str,
    recurrence_id: Option<&str>,
    changes: &[(&str, &str)],
) -> Result<PreparedEdit, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin,
        service: "calendar".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Calendar access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached calendar resource missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let raw = isyncyou_connectors::dav::edit::edit_text(&object.raw, uid, recurrence_id, changes)?;
    Ok(PreparedEdit {
        href: object.remote_id,
        expected_etag,
        expected_collection_token: None,
        raw,
    })
}

/// Prepare one event's UTC or all-day time change against its cached ETag.
/// Callers must display the series and scheduling implications before approval.
#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn prepare_event_time(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    uid: &str,
    recurrence_id: Option<&str>,
    start: &str,
    end: &str,
    date_only: bool,
) -> Result<PreparedEdit, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin,
        service: "calendar".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Calendar access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached calendar resource missing")?;
    let raw = isyncyou_connectors::dav::edit::event_time(
        &object.raw,
        uid,
        recurrence_id,
        start,
        end,
        date_only,
    )?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    Ok(PreparedEdit {
        href: object.remote_id,
        expected_etag,
        expected_collection_token: None,
        raw,
    })
}

/// Prepare a zone-aware edit; callers must bind the zone and both civil times
/// into any explicit scheduling/series approval.
#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn prepare_event_time_zoned(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    uid: &str,
    recurrence_id: Option<&str>,
    start: &str,
    end: &str,
    tzid: &str,
) -> Result<PreparedEdit, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin,
        service: "calendar".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Calendar access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached calendar resource missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let raw = isyncyou_connectors::dav::edit::event_time_zoned(
        &object.raw,
        uid,
        recurrence_id,
        start,
        end,
        tzid,
    )?;
    Ok(PreparedEdit {
        href: object.remote_id,
        expected_etag,
        expected_collection_token: None,
        raw,
    })
}

/// Create one UTC override from an occurrence confirmed by the server's
/// expansion of the same cached resource version.
#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn prepare_event_exception(
    cfg: &Config,
    account: &str,
    entry: &str,
    collection: &str,
    href: &str,
    uid: &str,
    recurrence_id: &str,
    start: &str,
    end: &str,
    window_start: &str,
    window_end: &str,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<PreparedEdit, String> {
    let (origin, store) = configured(cfg, account)?;
    let client = match credentials {
        Some(c) => client_with_store(cfg, account, &origin, c)?,
        None => client(cfg, account, &origin)?,
    };
    if !client
        .discover(entry)
        .map_err(|e| e.to_string())?
        .iter()
        .any(|c| c.calendar && c.url == collection)
    {
        return Err("Calendar is not currently accessible".into());
    }
    let scope = ProviderScope {
        account_id: account.into(),
        origin,
        service: "calendar".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Calendar access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached calendar resource missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let expansions = client
        .expanded_events(&scope, window_start, window_end)
        .map_err(|e| e.to_string())?;
    let mut matches = expansions
        .iter()
        .filter(|item| item.href == object.remote_id);
    let expansion = matches
        .next()
        .ok_or("Occurrence is not in server expansion")?;
    if matches.next().is_some() || expansion.etag != expected_etag {
        return Err("Calendar changed during expansion".into());
    }
    let raw = isyncyou_connectors::dav::edit::create_event_exception(
        &object.raw,
        expansion.calendar_data.as_bytes(),
        uid,
        recurrence_id,
        start,
        end,
    )?;
    Ok(PreparedEdit {
        href: object.remote_id,
        expected_etag,
        expected_collection_token: None,
        raw,
    })
}

pub struct PreparedExceptionPreview {
    pub origin: String,
    pub uid: String,
    pub recurrence_id: String,
    pub summary: String,
    pub old_start: String,
    pub old_end: String,
    pub new_start: String,
    pub new_end: String,
    pub edit: PreparedEdit,
}

#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn prepare_exception_preview(
    cfg: &Config,
    account: &str,
    entry: &str,
    collection: &str,
    href: &str,
    uid: &str,
    recurrence_id: &str,
    start: &str,
    end: &str,
    window_start: &str,
    window_end: &str,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<PreparedExceptionPreview, String> {
    let origin = configured_origin(cfg, account)?;
    let client = match credentials {
        Some(c) => client_with_store(cfg, account, &origin, c)?,
        None => client(cfg, account, &origin)?,
    };
    if !client
        .discover(entry)
        .map_err(|e| e.to_string())?
        .iter()
        .any(|c| c.calendar && c.url == collection)
    {
        return Err("Calendar is not currently accessible".into());
    }
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.clone(),
        service: "calendar".into(),
        collection: collection.into(),
    };
    let object = {
        let store = crate::provider_store::open_readonly(cfg, account)?;
        if store
            .provider_accessible(&scope)
            .map_err(|e| e.to_string())?
            != Some(true)
        {
            return Err("Calendar access is unknown or unavailable".into());
        }
        store
            .provider_object(&scope, href)
            .map_err(|e| e.to_string())?
            .ok_or("Cached event missing")?
    };
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let expansions = client
        .expanded_events(&scope, window_start, window_end)
        .map_err(|e| e.to_string())?;
    let mut matches = expansions
        .iter()
        .filter(|item| item.href == object.remote_id);
    let expansion = matches
        .next()
        .ok_or("Occurrence is not in server expansion")?;
    if matches.next().is_some() || expansion.etag != expected_etag {
        return Err("Calendar changed during expansion".into());
    }
    let (summary, old_start, old_end, raw) =
        isyncyou_connectors::dav::edit::preview_event_exception(
            &object.raw,
            expansion.calendar_data.as_bytes(),
            uid,
            recurrence_id,
            start,
            end,
        )?;
    Ok(PreparedExceptionPreview {
        origin,
        uid: uid.into(),
        recurrence_id: recurrence_id.into(),
        summary,
        old_start,
        old_end,
        new_start: start.into(),
        new_end: end.into(),
        edit: PreparedEdit {
            href: object.remote_id,
            expected_etag,
            expected_collection_token: None,
            raw,
        },
    })
}

pub struct PreparedEventTimePreview {
    pub origin: String,
    pub old_start: String,
    pub old_end: String,
    pub edit: PreparedEdit,
}

pub struct PreparedZonedTimePreview {
    pub origin: String,
    pub uid: String,
    pub old_start: String,
    pub old_start_zone: Option<String>,
    pub old_end: String,
    pub old_end_zone: Option<String>,
    pub new_start: String,
    pub new_end: String,
    pub new_zone: String,
    pub edit: PreparedEdit,
}

#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn prepare_zoned_time_preview(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    uid: &str,
    start: &str,
    end: &str,
    tzid: &str,
) -> Result<PreparedZonedTimePreview, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.clone(),
        service: "calendar".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Calendar access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached event missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let (old_start, old_start_zone, old_end, old_end_zone, raw) =
        isyncyou_connectors::dav::edit::preview_event_time_zoned(
            &object.raw,
            uid,
            start,
            end,
            tzid,
        )?;
    Ok(PreparedZonedTimePreview {
        origin,
        uid: uid.into(),
        old_start,
        old_start_zone,
        old_end,
        old_end_zone,
        new_start: start.into(),
        new_end: end.into(),
        new_zone: tzid.into(),
        edit: PreparedEdit {
            href: object.remote_id,
            expected_etag,
            expected_collection_token: None,
            raw,
        },
    })
}

/// Human confirmation preview for a single event without scheduling recipients.
#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn prepare_event_time_preview(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    uid: &str,
    start: &str,
    end: &str,
    date_only: bool,
) -> Result<PreparedEventTimePreview, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.clone(),
        service: "calendar".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Calendar access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached calendar resource missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let (old_start, old_end, raw) = isyncyou_connectors::dav::edit::preview_event_time(
        &object.raw,
        uid,
        start,
        end,
        date_only,
    )?;
    Ok(PreparedEventTimePreview {
        origin,
        old_start,
        old_end,
        edit: PreparedEdit {
            href: object.remote_id,
            expected_etag,
            expected_collection_token: None,
            raw,
        },
    })
}

pub struct PreparedTextPreview {
    pub origin: String,
    pub old_value: Option<String>,
    pub new_value: String,
    pub edit: PreparedEdit,
}

/// Preview is restricted to unscheduled single occurrences; dispatch must
/// separately recheck live privileges and condition the PUT on expected_etag.
#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn prepare_text_preview(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    uid: &str,
    recurrence_id: Option<&str>,
    field: &str,
    new_value: &str,
) -> Result<PreparedTextPreview, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.clone(),
        service: "calendar".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Calendar access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached calendar resource missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let (old_value, raw) = isyncyou_connectors::dav::edit::preview_text_edit(
        &object.raw,
        uid,
        recurrence_id,
        field,
        new_value,
    )?;
    Ok(PreparedTextPreview {
        origin,
        old_value,
        new_value: new_value.into(),
        edit: PreparedEdit {
            href: object.remote_id,
            expected_etag,
            expected_collection_token: None,
            raw,
        },
    })
}

/// Prepare one contact-property edit without altering the cached source.
pub fn prepare_contact_text_edit(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    key: &str,
    index: usize,
    value: &str,
) -> Result<PreparedEdit, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin,
        service: "contacts".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Address book access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached contact missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let raw = isyncyou_connectors::dav::edit::edit_contact_text(&object.raw, key, index, value)?;
    Ok(PreparedEdit {
        href: object.remote_id,
        expected_etag,
        expected_collection_token: None,
        raw,
    })
}

/// Prepare completion or reopening of one task master or existing exception.
pub fn prepare_task_completion(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    uid: &str,
    recurrence_id: Option<&str>,
    completed_at: Option<&str>,
) -> Result<PreparedEdit, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin,
        service: "calendar".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Task collection access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached task resource missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let raw = isyncyou_connectors::dav::edit::task_completion(
        &object.raw,
        uid,
        recurrence_id,
        completed_at,
    )?;
    Ok(PreparedEdit {
        href: object.remote_id,
        expected_etag,
        expected_collection_token: None,
        raw,
    })
}

pub struct PreparedTaskCompletion {
    pub origin: String,
    pub summary: String,
    pub old_status: Option<String>,
    pub old_completed: Option<String>,
    pub old_percent: Option<String>,
    pub edit: PreparedEdit,
}

pub struct PreparedTaskOccurrenceCompletion {
    pub origin: String,
    pub uid: String,
    pub recurrence_id: String,
    pub summary: String,
    pub start: String,
    pub due: String,
    pub completed_at: Option<String>,
    pub edit: PreparedEdit,
}

#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn prepare_task_occurrence_completion_preview(
    cfg: &Config,
    account: &str,
    entry: &str,
    collection: &str,
    href: &str,
    uid: &str,
    recurrence_id: &str,
    completed_at: Option<&str>,
    window_start: &str,
    window_end: &str,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<PreparedTaskOccurrenceCompletion, String> {
    let parse = isyncyou_connectors::dav::reminders::utc_window_bound;
    let (begin, end, occurrence) = (
        parse(window_start)?,
        parse(window_end)?,
        parse(recurrence_id)?,
    );
    if end <= begin
        || end.saturating_sub(begin) > 31 * 86400
        || occurrence < begin
        || occurrence >= end
    {
        return Err("Invalid occurrence expansion window".into());
    }
    let origin = configured_origin(cfg, account)?;
    let client = match credentials {
        Some(c) => client_with_store(cfg, account, &origin, c)?,
        None => client(cfg, account, &origin)?,
    };
    if !client
        .discover(entry)
        .map_err(|e| e.to_string())?
        .iter()
        .any(|c| c.calendar && c.url == collection)
    {
        return Err("Calendar is not currently accessible".into());
    }
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.clone(),
        service: "calendar".into(),
        collection: collection.into(),
    };
    let object = {
        let store = crate::provider_store::open_readonly(cfg, account)?;
        if store
            .provider_accessible(&scope)
            .map_err(|e| e.to_string())?
            != Some(true)
        {
            return Err("Calendar access is unknown or unavailable".into());
        }
        store
            .provider_object(&scope, href)
            .map_err(|e| e.to_string())?
            .ok_or("Cached task missing")?
    };
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let expansions = client
        .expanded_tasks(&scope, window_start, window_end)
        .map_err(|e| e.to_string())?;
    let mut matches = expansions
        .iter()
        .filter(|item| item.href == object.remote_id);
    let expansion = matches
        .next()
        .ok_or("Task occurrence is not in server expansion")?;
    if matches.next().is_some() || expansion.etag != expected_etag {
        return Err("Task changed during expansion".into());
    }
    let (summary, start, due, raw) =
        isyncyou_connectors::dav::edit::preview_task_occurrence_completion(
            &object.raw,
            expansion.calendar_data.as_bytes(),
            uid,
            recurrence_id,
            completed_at,
        )?;
    Ok(PreparedTaskOccurrenceCompletion {
        origin,
        uid: uid.into(),
        recurrence_id: recurrence_id.into(),
        summary,
        start,
        due,
        completed_at: completed_at.map(str::to_owned),
        edit: PreparedEdit {
            href: object.remote_id,
            expected_etag,
            expected_collection_token: None,
            raw,
        },
    })
}

/// Capture once before confirmation and reuse unchanged for re-preparation.
pub fn current_task_completion_time() -> Result<String, String> {
    isyncyou_connectors::dav::edit::current_task_completion_time()
}

/// Prepare a confirmed single-task state change and its preview from one cache
/// version. A fixed UTC completion timestamp is supplied by the host.
pub fn prepare_task_completion_preview(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    uid: &str,
    recurrence_id: Option<&str>,
    completed_at: Option<&str>,
) -> Result<PreparedTaskCompletion, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.clone(),
        service: "calendar".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Calendar access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached task resource missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let (summary, old_status, old_completed, old_percent, raw) =
        isyncyou_connectors::dav::edit::preview_task_completion(
            &object.raw,
            uid,
            recurrence_id,
            completed_at,
        )?;
    Ok(PreparedTaskCompletion {
        origin,
        summary,
        old_status,
        old_completed,
        old_percent,
        edit: PreparedEdit {
            href: object.remote_id,
            expected_etag,
            expected_collection_token: None,
            raw,
        },
    })
}

/// Prepare priority and/or all-day or UTC due-date changes for one task.
#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn prepare_task_attributes(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    uid: &str,
    recurrence_id: Option<&str>,
    priority: Option<u8>,
    due: Option<(&str, bool)>,
) -> Result<PreparedEdit, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin,
        service: "calendar".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Task collection access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached task resource missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let raw = isyncyou_connectors::dav::edit::task_attributes(
        &object.raw,
        uid,
        recurrence_id,
        priority,
        due,
    )?;
    Ok(PreparedEdit {
        href: object.remote_id,
        expected_etag,
        expected_collection_token: None,
        raw,
    })
}

pub struct PreparedTaskAttributes {
    pub origin: String,
    pub uid: String,
    pub name: String,
    pub old_priority: Option<String>,
    pub old_due: Option<String>,
    pub old_start: Option<String>,
    pub new_priority: Option<u8>,
    pub new_due: Option<(String, bool)>,
    pub new_start: Option<String>,
    pub edit: PreparedEdit,
}

pub fn prepare_task_attributes_preview(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    uid: &str,
    priority: Option<u8>,
    due: Option<(&str, bool)>,
) -> Result<PreparedTaskAttributes, String> {
    prepare_task_attributes_with_start_preview(
        cfg, account, collection, href, uid, priority, due, None,
    )
}

#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn prepare_task_attributes_with_start_preview(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    uid: &str,
    priority: Option<u8>,
    due: Option<(&str, bool)>,
    start: Option<&str>,
) -> Result<PreparedTaskAttributes, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.clone(),
        service: "calendar".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Task collection access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached task resource missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let (name, old_priority, old_due, old_start, raw) =
        isyncyou_connectors::dav::edit::preview_task_attributes_with_start(
            &object.raw,
            uid,
            priority,
            due,
            start,
        )?;
    Ok(PreparedTaskAttributes {
        origin,
        uid: uid.into(),
        name,
        old_priority,
        old_due,
        old_start,
        new_priority: priority,
        new_due: due.map(|(value, date)| (value.into(), date)),
        new_start: start.map(str::to_owned),
        edit: PreparedEdit {
            href: object.remote_id,
            expected_etag,
            expected_collection_token: None,
            raw,
        },
    })
}

pub(crate) fn prepare_task_parent_in_store(
    store: &Store,
    scope: &ProviderScope,
    href: &str,
    uid: &str,
    parent: Option<&str>,
) -> Result<PreparedEdit, String> {
    use isyncyou_store::ProviderDavIndex;
    if store
        .provider_accessible(scope)
        .map_err(|e| e.to_string())?
        != Some(true)
        || store
            .provider_dav_index_incomplete_count(scope)
            .map_err(|e| e.to_string())?
            != 0
    {
        return Err("Task graph is unavailable or incomplete".into());
    }
    let snapshot = store
        .provider_export_snapshot(&scope.account_id, &scope.origin)
        .map_err(|e| e.to_string())?;
    let collection = snapshot
        .collections
        .iter()
        .find(|c| c.scope == *scope && c.complete && c.accessible)
        .ok_or("Task collection is not fully synchronized")?;
    let token = collection
        .cursor
        .clone()
        .filter(|v| !v.is_empty())
        .ok_or("Task collection checkpoint missing")?;
    let lookup = |id: &str| -> Result<isyncyou_store::ProviderDavIndexEntry, String> {
        let mut matches = store
            .provider_dav_uid_lookup(scope, id)
            .map_err(|e| e.to_string())?;
        if matches.len() != 1 {
            return Err("Task identity is missing or ambiguous".into());
        }
        let entry = matches.pop().ok_or("Task missing")?;
        if !matches!(&entry.index, ProviderDavIndex::Valid {kind,..} if kind == "task") {
            return Err("Parent must be a task in the same collection".into());
        }
        Ok(entry)
    };
    let child = lookup(uid)?;
    if child.remote_id != href {
        return Err("Task target no longer matches its UID".into());
    }
    let mut seen = std::collections::HashSet::new();
    seen.insert(uid.to_owned());
    let mut current = parent.map(str::to_owned);
    while let Some(id) = current {
        if seen.len() >= 1024 || !seen.insert(id.clone()) {
            return Err("Task parent would create a cycle or exceeds depth limit".into());
        }
        let entry = lookup(&id)?;
        current = match entry.index {
            ProviderDavIndex::Valid { parent_uid, .. } => parent_uid,
            ProviderDavIndex::Invalid => return Err("Invalid task graph".into()),
        };
    }
    let object = store
        .provider_object(scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached task resource missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let raw = isyncyou_connectors::dav::edit::task_parent(&object.raw, uid, parent)?;
    Ok(PreparedEdit {
        href: object.remote_id,
        expected_etag,
        expected_collection_token: Some(token),
        raw,
    })
}

/// Prepare native parentage after checking the complete, indexed task graph.
/// The returned sync token must be bound into the approved DAV write.
pub fn prepare_task_parent(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    uid: &str,
    parent: Option<&str>,
) -> Result<PreparedEdit, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin,
        service: "calendar".into(),
        collection: collection.into(),
    };
    prepare_task_parent_in_store(&store, &scope, href, uid, parent)
}

pub struct PreparedTaskParent {
    pub origin: String,
    pub uid: String,
    pub summary: String,
    pub old_parent: Option<String>,
    pub new_parent: Option<String>,
    pub edit: PreparedEdit,
}

pub struct PreparedTaskDelete {
    pub origin: String,
    pub uid: String,
    pub summary: String,
    pub parent: Option<String>,
    pub href: String,
    pub expected_etag: String,
    pub expected_collection_token: String,
}

pub(crate) fn prepare_task_delete_in_store(
    store: &Store,
    scope: &ProviderScope,
    href: &str,
    uid: &str,
) -> Result<(String, Option<String>, String, String), String> {
    use isyncyou_store::ProviderDavIndex;
    if store
        .provider_accessible(scope)
        .map_err(|e| e.to_string())?
        != Some(true)
        || store
            .provider_dav_index_incomplete_count(scope)
            .map_err(|e| e.to_string())?
            != 0
    {
        return Err("Task graph is unavailable or incomplete".into());
    }
    let snapshot = store
        .provider_export_snapshot(&scope.account_id, &scope.origin)
        .map_err(|e| e.to_string())?;
    let collection = snapshot
        .collections
        .iter()
        .find(|c| c.scope == *scope && c.complete && c.accessible)
        .ok_or("Task collection is not fully synchronized")?;
    let token = collection
        .cursor
        .clone()
        .filter(|value| !value.is_empty())
        .ok_or("Task collection checkpoint missing")?;
    let entries = store
        .provider_dav_uid_lookup(scope, uid)
        .map_err(|e| e.to_string())?;
    if entries.len() != 1
        || entries[0].remote_id != href
        || !matches!(&entries[0].index,ProviderDavIndex::Valid {kind,..} if kind == "task")
    {
        return Err("Task identity is missing or ambiguous".into());
    }
    if store
        .provider_dav_child_count(scope, uid)
        .map_err(|e| e.to_string())?
        != 0
    {
        return Err("Task has child tasks and cannot be deleted".into());
    }
    let object = store
        .provider_object(scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached task missing")?;
    let etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let (summary, parent) = isyncyou_connectors::dav::edit::preview_task_delete(&object.raw, uid)?;
    Ok((summary, parent, etag, token))
}

pub fn prepare_task_delete_preview(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    uid: &str,
) -> Result<PreparedTaskDelete, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.clone(),
        service: "calendar".into(),
        collection: collection.into(),
    };
    let (summary, parent, expected_etag, expected_collection_token) =
        prepare_task_delete_in_store(&store, &scope, href, uid)?;
    Ok(PreparedTaskDelete {
        origin,
        uid: uid.into(),
        summary,
        parent,
        href: href.into(),
        expected_etag,
        expected_collection_token,
    })
}

pub fn prepare_task_parent_preview(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    uid: &str,
    parent: Option<&str>,
) -> Result<PreparedTaskParent, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.clone(),
        service: "calendar".into(),
        collection: collection.into(),
    };
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached task missing")?;
    let (summary, old_parent, raw) =
        isyncyou_connectors::dav::edit::preview_task_parent(&object.raw, uid, parent)?;
    let edit = prepare_task_parent_in_store(&store, &scope, href, uid, parent)?;
    if raw != edit.raw {
        return Err("Task parent effect is inconsistent".into());
    }
    Ok(PreparedTaskParent {
        origin,
        uid: uid.into(),
        summary,
        old_parent,
        new_parent: parent.map(str::to_owned),
        edit,
    })
}

/// Prepare an explicit attendee response; server scheduling effects require
/// separate effect-bound confirmation and real provider verification.
#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn prepare_attendee_response(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    uid: &str,
    recurrence_id: Option<&str>,
    attendee: &str,
    response: &str,
) -> Result<PreparedEdit, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin,
        service: "calendar".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Calendar access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached event missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let raw = isyncyou_connectors::dav::edit::respond_attendee(
        &object.raw,
        uid,
        recurrence_id,
        attendee,
        response,
    )?;
    Ok(PreparedEdit {
        href: object.remote_id,
        expected_etag,
        expected_collection_token: None,
        raw,
    })
}

pub struct PreparedInvitationResponse {
    pub origin: String,
    pub organizer: String,
    pub attendee: String,
    pub old_response: String,
    pub new_response: String,
    pub edit: PreparedEdit,
}

/// Bind the preview to an address currently advertised by the authenticated
/// principal. Dispatch must repeat this check immediately before the write.
#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn prepare_invitation_response(
    cfg: &Config,
    account: &str,
    entry: &str,
    collection: &str,
    href: &str,
    uid: &str,
    recurrence_id: Option<&str>,
    attendee: &str,
    response: &str,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<PreparedInvitationResponse, String> {
    let (origin, store) = configured(cfg, account)?;
    let client = match credentials {
        Some(c) => client_with_store(cfg, account, &origin, c)?,
        None => client(cfg, account, &origin)?,
    };
    if !client
        .discover(entry)
        .map_err(|e| e.to_string())?
        .iter()
        .any(|c| c.calendar && c.url == collection)
    {
        return Err("Calendar is not currently accessible".into());
    }
    if !client
        .calendar_user_addresses(entry)
        .map_err(|e| e.to_string())?
        .iter()
        .any(|address| address == attendee)
    {
        return Err("Attendee is not an authenticated calendar identity".into());
    }
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.clone(),
        service: "calendar".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Calendar access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached invitation missing")?;
    let etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let (organizer, old_response, raw) = isyncyou_connectors::dav::edit::preview_attendee_response(
        &object.raw,
        uid,
        recurrence_id,
        attendee,
        response,
    )?;
    Ok(PreparedInvitationResponse {
        origin,
        organizer,
        attendee: attendee.into(),
        old_response,
        new_response: response.into(),
        edit: PreparedEdit {
            href: object.remote_id,
            expected_etag: etag,
            expected_collection_token: None,
            raw,
        },
    })
}

/// Prepare cancellation with an explicit advanced sequence and exact ETag.
pub fn prepare_event_cancellation(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    uid: &str,
    recurrence_id: Option<&str>,
    sequence: u32,
) -> Result<PreparedEdit, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin,
        service: "calendar".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Calendar access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached event missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let raw =
        isyncyou_connectors::dav::edit::cancel_event(&object.raw, uid, recurrence_id, sequence)?;
    Ok(PreparedEdit {
        href: object.remote_id,
        expected_etag,
        expected_collection_token: None,
        raw,
    })
}

pub struct PreparedCancellation {
    pub origin: String,
    pub uid: String,
    pub recurrence_id: Option<String>,
    pub summary: String,
    pub organizer: String,
    pub attendees: Vec<String>,
    pub old_sequence: u32,
    pub new_sequence: u32,
    pub has_alarms: bool,
    pub edit: PreparedEdit,
}

#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn prepare_cancellation_preview(
    cfg: &Config,
    account: &str,
    entry: &str,
    collection: &str,
    href: &str,
    uid: &str,
    recurrence_id: Option<&str>,
    sequence: u32,
    credentials: Option<&dyn ProviderCredentialStore>,
) -> Result<PreparedCancellation, String> {
    let (origin, store) = configured(cfg, account)?;
    let client = match credentials {
        Some(c) => client_with_store(cfg, account, &origin, c)?,
        None => client(cfg, account, &origin)?,
    };
    if !client
        .discover(entry)
        .map_err(|e| e.to_string())?
        .iter()
        .any(|c| c.calendar && c.url == collection)
    {
        return Err("Calendar is not currently accessible".into());
    }
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.clone(),
        service: "calendar".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Calendar access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached event missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let preview = isyncyou_connectors::dav::edit::preview_event_cancellation(
        &object.raw,
        uid,
        recurrence_id,
        sequence,
    )?;
    if !client
        .calendar_user_addresses(entry)
        .map_err(|e| e.to_string())?
        .iter()
        .any(|address| address == &preview.organizer)
    {
        return Err("Organizer is not an authenticated calendar identity".into());
    }
    Ok(PreparedCancellation {
        origin,
        uid: uid.into(),
        recurrence_id: recurrence_id.map(str::to_owned),
        summary: preview.summary,
        organizer: preview.organizer,
        attendees: preview.attendees,
        old_sequence: preview.old_sequence,
        new_sequence: preview.new_sequence,
        has_alarms: preview.has_alarms,
        edit: PreparedEdit {
            href: object.remote_id,
            expected_etag,
            expected_collection_token: None,
            raw: preview.raw,
        },
    })
}

/// Prepare a validated series-level recurrence change; overrides stay in the
/// original resource and the host must confirm any affected occurrences.
pub fn prepare_series_recurrence(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    uid: &str,
    rule: Option<&str>,
) -> Result<PreparedEdit, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin,
        service: "calendar".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Calendar access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached series missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let raw = isyncyou_connectors::dav::edit::series_recurrence(&object.raw, uid, rule)?;
    Ok(PreparedEdit {
        href: object.remote_id,
        expected_etag,
        expected_collection_token: None,
        raw,
    })
}

pub struct PreparedSeriesRule {
    pub origin: String,
    pub uid: String,
    pub summary: String,
    pub start: String,
    pub end: String,
    pub old_rule: String,
    pub new_rule: String,
    pub old_occurrences: Vec<String>,
    pub new_occurrences: Vec<String>,
    pub edit: PreparedEdit,
}

pub struct PreparedAlarm {
    pub origin: String,
    pub summary: String,
    pub old_trigger: Option<String>,
    pub old_description: Option<String>,
    pub new_trigger: Option<String>,
    pub new_description: Option<String>,
    pub edit: PreparedEdit,
}

#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn prepare_alarm_preview(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    uid: &str,
    recurrence_id: Option<&str>,
    index: Option<usize>,
    trigger: Option<&str>,
    description: Option<&str>,
) -> Result<PreparedAlarm, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.clone(),
        service: "calendar".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Calendar access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached calendar resource missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let p = isyncyou_connectors::dav::edit::preview_alarm(
        &object.raw,
        uid,
        recurrence_id,
        index,
        trigger,
        description,
    )?;
    Ok(PreparedAlarm {
        origin,
        summary: p.summary,
        old_trigger: p.old_trigger,
        old_description: p.old_description,
        new_trigger: p.new_trigger,
        new_description: p.new_description,
        edit: PreparedEdit {
            href: object.remote_id,
            expected_etag,
            expected_collection_token: None,
            raw: p.raw,
        },
    })
}

pub fn prepare_series_rule_preview(
    cfg: &Config,
    account: &str,
    collection: &str,
    href: &str,
    uid: &str,
    rule: &str,
) -> Result<PreparedSeriesRule, String> {
    let (origin, store) = configured(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.clone(),
        service: "calendar".into(),
        collection: collection.into(),
    };
    if store
        .provider_accessible(&scope)
        .map_err(|e| e.to_string())?
        != Some(true)
    {
        return Err("Calendar access is unknown or unavailable".into());
    }
    let object = store
        .provider_object(&scope, href)
        .map_err(|e| e.to_string())?
        .ok_or("Cached series missing")?;
    let expected_etag = object
        .etag
        .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
        .ok_or("Strong ETag required")?;
    let preview = isyncyou_connectors::dav::edit::preview_series_rule(&object.raw, uid, rule)?;
    Ok(PreparedSeriesRule {
        origin,
        uid: uid.into(),
        summary: preview.summary,
        start: preview.start,
        end: preview.end,
        old_rule: preview.old_rule,
        new_rule: preview.new_rule,
        old_occurrences: preview.old_occurrences,
        new_occurrences: preview.new_occurrences,
        edit: PreparedEdit {
            href: object.remote_id,
            expected_etag,
            expected_collection_token: None,
            raw: preview.raw,
        },
    })
}

/// Build the exact payload to be confirmed and persisted before creating a
/// recovered resource. The verified original remains with the recovery caller.
pub fn prepare_recovered_calendar(
    original: &[u8],
    uid_map: &std::collections::HashMap<String, String>,
) -> Result<Vec<u8>, String> {
    isyncyou_connectors::dav::edit::recovered_calendar(original, uid_map)
}

pub enum RecoveryImportOutcome {
    Confirmed { href: String },
    Unknown,
    Conflict,
    Rejected,
}

/// Import an approved, hash-verified original into an explicitly selected
/// personal Recovered collection. Callers must persist the approved operation
/// ID and UID mapping before invoking this seam. A previously claimed operation
/// is reconciled by readback only, never re-sent after an unknown outcome.
#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub fn import_recovered_with_store(
    cfg: &Config,
    account: &str,
    entry: &str,
    collection: &str,
    service: &str,
    operation_id: &str,
    generation: &str,
    object_id: &str,
    version: &str,
    original: &[u8],
    original_sha256: &str,
    uid_map: &std::collections::HashMap<String, String>,
    contact_uid: Option<&str>,
    credentials: &dyn ProviderCredentialStore,
) -> Result<RecoveryImportOutcome, String> {
    let (origin, store) = configured(cfg, account)?;
    crate::provider_auth::with_locked_provider_access_token(cfg, account, credentials, |token| {
        import_recovered_with_token(
            cfg,
            account,
            &origin,
            &store,
            entry,
            collection,
            service,
            operation_id,
            generation,
            object_id,
            version,
            original,
            original_sha256,
            uid_map,
            contact_uid,
            token,
        )
    })
}

/// Engine-only variant for a caller already holding the account authorization
/// lock. The token must come from that lock and never cross a platform/UI API.
#[allow(clippy::too_many_arguments, reason = "DAV operation contract")]
pub(crate) fn import_recovered_with_token(
    cfg: &Config,
    account: &str,
    origin: &str,
    store: &Store,
    entry: &str,
    collection: &str,
    service: &str,
    operation_id: &str,
    generation: &str,
    object_id: &str,
    version: &str,
    original: &[u8],
    original_sha256: &str,
    uid_map: &std::collections::HashMap<String, String>,
    contact_uid: Option<&str>,
    token: &str,
) -> Result<RecoveryImportOutcome, String> {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    if !matches!(service, "calendar" | "contacts")
        || [operation_id, generation, object_id, version]
            .iter()
            .any(|v| v.is_empty() || v.len() > 4096 || v.chars().any(char::is_control))
        || original.len() > 16 * 1024 * 1024
    {
        return Err("Invalid recovery import identity".into());
    }
    let source_digest = format!("{:x}", Sha256::digest(original));
    if source_digest != original_sha256 {
        return Err("Recovery original hash mismatch".into());
    }
    let projected = if service == "calendar" {
        if contact_uid.is_some() {
            return Err("Unexpected contact UID".into());
        }
        prepare_recovered_calendar(original, uid_map)?
    } else {
        if !uid_map.is_empty() {
            return Err("Unexpected calendar UID mapping".into());
        }
        isyncyou_connectors::dav::edit::recovered_contact(
            original,
            contact_uid.ok_or("Recovery contact UID required")?,
        )?
    };
    if !matches!(cfg.provider_accounts.get(account), Some(ProviderConfig::SelfHosted {origin: configured, ..}) if configured.as_str() == origin)
    {
        return Err("Account or provider changed".into());
    }
    let client = DavClient::new(origin, token.to_owned()).map_err(|e| e.to_string())?;
    let collections = client.discover(entry).map_err(|e| e.to_string())?;
    if !collections.iter().any(|c| {
        c.url == collection
            && c.name == "Recovered"
            && c.owned_by_current_principal == Some(true)
            && if service == "calendar" {
                c.calendar
            } else {
                c.addressbook
            }
    }) {
        return Err("Selected personal Recovered collection is unavailable".into());
    }
    let scope = ProviderScope {
        account_id: account.into(),
        origin: origin.into(),
        service: service.into(),
        collection: collection.into(),
    };
    client
        .require_private_recovery_collection(entry, &scope)
        .map_err(|_| "Recovered collection privacy is unavailable")?;
    let file = format!(
        "recovered-{:x}.{}",
        Sha256::digest(operation_id.as_bytes()),
        if service == "calendar" { "ics" } else { "vcf" }
    );
    let href = client
        .validate_target(&scope, &file)
        .map_err(|e| e.to_string())?;
    let intent = serde_json::json!({"generation":generation,"object_id":object_id,"version":version,"href":href,"original_sha256":source_digest,"original":base64::engine::general_purpose::STANDARD.encode(original),"projection":base64::engine::general_purpose::STANDARD.encode(&projected)}).to_string();
    let digest = format!("{:x}", Sha256::digest(intent.as_bytes()));
    journaled_recovery_dispatch(
        store,
        &scope,
        operation_id,
        &intent,
        &digest,
        || {
            if !client
                .permits(&scope, None, "bind")
                .map_err(|e| e.to_string())?
            {
                return Err("Recovered collection is not writable".into());
            }
            client
                .require_private_parent_atomic(&scope)
                .map_err(|_| "Atomic private recovery is unavailable".to_string())
        },
        || client.put_private_recovery(&scope, &href, projected.clone()),
        || reconcile_recovery(store, &client, &scope, operation_id, &href, &projected),
    )
}

/// Existing intents are observed only; a first dispatch follows an atomic claim.
#[allow(clippy::too_many_arguments)]
fn journaled_recovery_dispatch(
    store: &Store,
    scope: &ProviderScope,
    operation_id: &str,
    intent: &str,
    digest: &str,
    preflight: impl FnOnce() -> Result<(), String>,
    dispatch: impl FnOnce() -> Result<Option<String>, DavError>,
    readback: impl FnOnce() -> Result<RecoveryImportOutcome, String>,
) -> Result<RecoveryImportOutcome, String> {
    if let Some((previous, state, result)) = store
        .provider_mutation(scope, operation_id)
        .map_err(|e| e.to_string())?
    {
        if previous != digest {
            return Err("Recovery operation differs from recorded intent".into());
        }
        return match state.as_str() {
            "confirmed" => Ok(RecoveryImportOutcome::Confirmed {
                href: result.ok_or("Recovery result identity missing")?,
            }),
            "rejected" => Ok(RecoveryImportOutcome::Rejected),
            "conflict" => Ok(RecoveryImportOutcome::Conflict),
            _ => readback(),
        };
    }
    preflight()?;
    if !store
        .claim_provider_mutation(scope, operation_id, "dav_recovery", intent, digest)
        .map_err(|e| e.to_string())?
    {
        return Ok(RecoveryImportOutcome::Unknown);
    }
    finish_recovery_dispatch(store, scope, operation_id, dispatch(), readback)
}

fn finish_recovery_dispatch(
    store: &Store,
    scope: &ProviderScope,
    operation_id: &str,
    result: Result<Option<String>, DavError>,
    readback: impl FnOnce() -> Result<RecoveryImportOutcome, String>,
) -> Result<RecoveryImportOutcome, String> {
    match result {
        Ok(_) => readback(),
        Err(DavError::Conflict) => {
            if store
                .conflict_provider_mutation(scope, operation_id)
                .map_err(|e| e.to_string())?
            {
                Ok(RecoveryImportOutcome::Conflict)
            } else {
                Ok(RecoveryImportOutcome::Unknown)
            }
        }
        Err(DavError::Http(code)) if (400..500).contains(&code) && code != 408 && code != 429 => {
            if store
                .finish_provider_mutation(scope, operation_id, None)
                .map_err(|e| e.to_string())?
            {
                Ok(RecoveryImportOutcome::Rejected)
            } else {
                Ok(RecoveryImportOutcome::Unknown)
            }
        }
        // A timeout, throttling response, server failure or transport failure
        // is reconciled by readback only; the recorded write is never resent.
        Err(_) => readback(),
    }
}

fn reconcile_recovery(
    store: &Store,
    client: &DavClient,
    scope: &ProviderScope,
    operation_id: &str,
    href: &str,
    projected: &[u8],
) -> Result<RecoveryImportOutcome, String> {
    let current = match client.read_resource(scope, href) {
        Ok(current) => current,
        Err(_) => return Ok(RecoveryImportOutcome::Unknown),
    };
    if !recovery_readback_matches(projected, &current) {
        return Ok(RecoveryImportOutcome::Unknown);
    }
    if store
        .finish_provider_mutation(scope, operation_id, Some(href))
        .map_err(|e| e.to_string())?
    {
        Ok(RecoveryImportOutcome::Confirmed { href: href.into() })
    } else {
        Ok(RecoveryImportOutcome::Unknown)
    }
}

/// Compare exact content after only RFC line unfolding. Other server changes
/// remain unverified; an ETag is required for the native observation.
pub fn recovery_readback_matches(prepared: &[u8], current: &ProviderObject) -> bool {
    fn unfold(input: &[u8]) -> Vec<u8> {
        let mut output = Vec::with_capacity(input.len());
        let mut i = 0;
        while i < input.len() {
            if input
                .get(i..i + 3)
                .is_some_and(|v| v == b"\r\n " || v == b"\r\n\t")
            {
                i += 3;
            } else {
                output.push(input[i]);
                i += 1;
            }
        }
        output
    }
    !prepared.is_empty()
        && (prepared == current.raw.as_slice() || unfold(prepared) == unfold(&current.raw))
        && current.etag.as_ref().is_some_and(|etag| !etag.is_empty())
}

/// Domain views carry provider identity and observation metadata. Individual
/// invalid resources are explicit errors rather than silently discarded.
pub fn project_page(
    cfg: &Config,
    account: &str,
    collection: &str,
    service: &str,
    after: &str,
    limit: usize,
) -> Result<ProjectedPage, String> {
    let page = objects(cfg, account, collection, service, after, limit)?;
    let objects = page
        .objects
        .into_iter()
        .map(|object| {
            let view = if service == "calendar" {
                isyncyou_connectors::dav::projection::calendar(&object.raw)
            } else {
                isyncyou_connectors::dav::projection::contact(&object.raw)
            };
            ProjectedObject {
                remote_id: object.remote_id,
                etag: object.etag,
                view,
            }
        })
        .collect();
    Ok(ProjectedPage {
        account_id: page.account_id,
        origin: page.origin,
        accessible: page.accessible,
        observed_at: page.observed_at,
        cursor: page.cursor,
        objects,
    })
}

/// Read a bounded keyset page without presenting cached bytes as live data.
/// Use an empty `after` for the first page and the final `remote_id` for the next.
pub fn objects(
    cfg: &Config,
    account: &str,
    collection: &str,
    service: &str,
    after: &str,
    limit: usize,
) -> Result<CachedObjects, String> {
    if !matches!(service, "calendar" | "contacts") {
        return Err("Unsupported DAV service".into());
    }
    let origin = configured_origin(cfg, account)?;
    let store = crate::provider_store::open_readonly(cfg, account)?;
    let scope = ProviderScope {
        account_id: account.into(),
        origin,
        service: service.into(),
        collection: collection.into(),
    };
    cached_live_objects(&store, &scope, after, limit)
}

pub fn cached_live_objects(
    store: &Store,
    scope: &ProviderScope,
    after: &str,
    limit: usize,
) -> Result<CachedObjects, String> {
    let (observed_at, cursor, objects) = store
        .provider_accessible_dav_snapshot(scope, after, limit)
        .map_err(|e| e.to_string())?
        .ok_or("DAV collection access not established")?;
    Ok(CachedObjects {
        account_id: scope.account_id.clone(),
        origin: scope.origin.clone(),
        accessible: Some(true),
        observed_at,
        cursor,
        objects,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_readback_only_accepts_exact_rfc_line_folding() {
        let prepared = b"BEGIN:VCALENDAR\r\nRELATED-TO:parent-uid\r\nEND:VCALENDAR\r\n";
        let mut current = ProviderObject {
            remote_id: "fixture".into(),
            etag: Some("\"etag\"".into()),
            raw: b"BEGIN:VCALENDAR\r\nRELATED-TO:parent-\r\n uid\r\nEND:VCALENDAR\r\n".to_vec(),
        };
        assert!(recovery_readback_matches(prepared, &current));
        current.raw =
            b"BEGIN:VCALENDAR\r\nRELATED-TO:parent-\r\n\tuid\r\nEND:VCALENDAR\r\n".to_vec();
        assert!(recovery_readback_matches(prepared, &current));
        current.raw =
            b"BEGIN:VCALENDAR\r\nRELATED-TO:parent-\r\n  uid\r\nEND:VCALENDAR\r\n".to_vec();
        assert!(!recovery_readback_matches(prepared, &current));
        current.raw = b"BEGIN:VCALENDAR\r\nRELATED-TO:parent-\n uid\r\nEND:VCALENDAR\r\n".to_vec();
        assert!(!recovery_readback_matches(prepared, &current));
        current.raw = b"BEGIN:VCALENDAR\r\nRELATED-TO:parent-other\r\nEND:VCALENDAR\r\n".to_vec();
        assert!(!recovery_readback_matches(prepared, &current));
        current.raw = prepared.to_vec();
        current.etag = None;
        assert!(!recovery_readback_matches(prepared, &current));
    }

    #[test]
    fn recovery_continuation_reconciles_unknown_before_first_new_dispatch() {
        use std::cell::Cell;
        let store = Store::open_in_memory().unwrap();
        let scope = ProviderScope {
            account_id: "account".into(),
            origin: "https://example.test/".into(),
            service: "calendar".into(),
            collection: "https://example.test/recovered/".into(),
        };
        for op in ["parent", "child"] {
            assert!(store
                .claim_provider_mutation(&scope, op, "dav_recovery", "intent", "digest")
                .unwrap());
        }
        assert!(store
            .finish_provider_mutation(&scope, "parent", Some("parent-href"))
            .unwrap());
        let dispatches = Cell::new(0);
        let child_reads = Cell::new(0);
        let parent = journaled_recovery_dispatch(
            &store,
            &scope,
            "parent",
            "intent",
            "digest",
            || panic!("confirmed preflight"),
            || panic!("confirmed write"),
            || panic!("confirmed readback"),
        )
        .unwrap();
        assert!(matches!(parent, RecoveryImportOutcome::Confirmed { .. }));
        let child = journaled_recovery_dispatch(
            &store,
            &scope,
            "child",
            "intent",
            "digest",
            || panic!("unknown preflight"),
            || panic!("unknown must not write"),
            || {
                child_reads.set(child_reads.get() + 1);
                let current = ProviderObject {
                    remote_id: "child".into(),
                    etag: Some("\"etag\"".into()),
                    raw: b"RELATED-TO:parent-\r\n uid\r\n".to_vec(),
                };
                assert!(recovery_readback_matches(
                    b"RELATED-TO:parent-uid\r\n",
                    &current
                ));
                assert!(store
                    .finish_provider_mutation(&scope, "child", Some("child-href"))
                    .unwrap());
                Ok(RecoveryImportOutcome::Confirmed {
                    href: "child-href".into(),
                })
            },
        )
        .unwrap();
        assert!(matches!(child, RecoveryImportOutcome::Confirmed { .. }));
        assert_eq!(child_reads.get(), 1);
        let event = journaled_recovery_dispatch(
            &store,
            &scope,
            "event",
            "intent",
            "digest",
            || Ok(()),
            || {
                assert_eq!(
                    store.provider_mutation(&scope, "event").unwrap().unwrap().1,
                    "unknown"
                );
                dispatches.set(dispatches.get() + 1);
                Err(DavError::Transport("fixture response lost".into()))
            },
            || Ok(RecoveryImportOutcome::Unknown),
        )
        .unwrap();
        assert!(matches!(event, RecoveryImportOutcome::Unknown));
        let repeated = journaled_recovery_dispatch(
            &store,
            &scope,
            "event",
            "intent",
            "digest",
            || panic!("unknown preflight"),
            || panic!("unknown must not repeat"),
            || Ok(RecoveryImportOutcome::Unknown),
        )
        .unwrap();
        assert!(matches!(repeated, RecoveryImportOutcome::Unknown));
        assert_eq!(dispatches.get(), 1);
    }

    #[test]
    fn cached_dav_pages_hide_lost_access_and_restore_without_deleting_originals() {
        let store = Store::open_in_memory().unwrap();
        for service in ["calendar", "contacts"] {
            let scope = ProviderScope {
                account_id: "fixture".into(),
                origin: "https://example.test/".into(),
                service: service.into(),
                collection: format!("/{service}/"),
            };
            let original = ProviderObject {
                remote_id: format!("/{service}/one"),
                etag: Some("v1".into()),
                raw: b"private fixture bytes".to_vec(),
            };
            store
                .apply_provider_page(
                    &scope,
                    &[isyncyou_store::ProviderChange::Upsert(original.clone())],
                    "one",
                )
                .unwrap();
            assert_eq!(
                cached_live_objects(&store, &scope, "", 10).unwrap().objects,
                vec![original.clone()]
            );
            store.set_provider_accessible(&scope, false).unwrap();
            assert!(cached_live_objects(&store, &scope, "", 10).is_err());
            assert!(store
                .provider_accessible_dav_objects_page(&scope, "", 10)
                .unwrap()
                .is_empty());
            assert_eq!(
                store.provider_object(&scope, &original.remote_id).unwrap(),
                Some(original.clone())
            );
            store.set_provider_accessible(&scope, true).unwrap();
            assert_eq!(
                cached_live_objects(&store, &scope, "", 10).unwrap().objects,
                vec![original]
            );
        }
    }

    #[test]
    #[cfg(feature = "encrypted-store")]
    fn cached_dav_page_reads_while_writer_is_held_and_respects_access_loss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("provider.db");
        let key = b"synthetic dav reader key";
        let writer = Store::open_encrypted(&path, key).unwrap();
        let scope = ProviderScope {
            account_id: "fixture".into(),
            origin: "https://example.test/".into(),
            service: "calendar".into(),
            collection: "/calendar/".into(),
        };
        let object = ProviderObject {
            remote_id: "/calendar/one".into(),
            etag: Some("v1".into()),
            raw: b"fixture".to_vec(),
        };
        writer
            .apply_provider_page(
                &scope,
                &[isyncyou_store::ProviderChange::Upsert(object.clone())],
                "one",
            )
            .unwrap();
        let reader = Store::open_readonly_encrypted(&path, key).unwrap();
        assert_eq!(
            cached_live_objects(&reader, &scope, "", 10)
                .unwrap()
                .objects,
            vec![object]
        );
        writer.set_provider_accessible(&scope, false).unwrap();
        assert!(cached_live_objects(&reader, &scope, "", 10).is_err());
    }

    #[test]
    fn reminder_claim_diagnostic_exposes_only_bounded_phase() {
        struct EmptyCredentials;
        impl ProviderCredentialStore for EmptyCredentials {
            fn load(
                &self,
                _: &str,
                _: &str,
            ) -> Result<Option<crate::provider_credential::ProviderCredential>, String>
            {
                Err("private details".into())
            }
            fn replace(
                &self,
                _: &str,
                _: &str,
                _: &crate::provider_credential::ProviderCredential,
            ) -> Result<(), String> {
                Err("private details".into())
            }
            fn delete(&self, _: &str, _: &str) -> Result<(), String> {
                Err("private details".into())
            }
        }
        let cfg = Config::default();
        assert_eq!(
            claim_reminder_record_diagnostic(
                &cfg,
                "a",
                "/",
                &serde_json::json!({}),
                &EmptyCredentials
            ),
            Err("record")
        );
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let record = serde_json::json!({"account":"a", "collection":"private-collection", "href":"private-item", "uid":"u", "recurrence_id":"r", "alarm_id":"a", "trigger_at":now, "occurrence_start":"start", "occurrence_end":"end", "window_start":1, "window_end":2});
        assert_eq!(
            claim_reminder_record_diagnostic(&cfg, "a", "/", &record, &EmptyCredentials),
            Err("credentials")
        );
    }

    #[test]
    fn failed_collection_discards_its_records_and_does_not_starve_next_collection() {
        let mut plan = ReminderAccountPlan {
            records: vec![],
            unsupported_count: 0,
            completed_collections: vec![],
            failed_collections: 0,
            failure_reason: None,
        };
        finish_reminder_collection(
            &mut plan,
            "bad".into(),
            vec![serde_json::json!("stale")],
            1,
            Err("reminder_sync".into()),
        );
        finish_reminder_collection(
            &mut plan,
            "good".into(),
            vec![serde_json::json!("valid")],
            0,
            Ok(()),
        );
        assert_eq!(plan.records, vec![serde_json::json!("valid")]);
        assert_eq!(plan.completed_collections, vec!["good"]);
        assert_eq!(plan.failed_collections, 1);
        assert_eq!(plan.failure_reason.as_deref(), Some("reminder_sync"));
        assert_eq!(plan.unsupported_count, 0);
    }

    #[test]
    fn unsupported_recurring_alarm_does_not_hide_supported_reminder() {
        let valid = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//fixture//EN\r\nBEGIN:VEVENT\r\nUID:event\r\nDTSTAMP:20261001T100000Z\r\nDTSTART:20261028T100000Z\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT15M\r\nDESCRIPTION:Alarm\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let unsupported = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//fixture//EN\r\nBEGIN:VEVENT\r\nUID:series\r\nDTSTAMP:20261001T100000Z\r\nDTSTART:20261028T100000Z\r\nRRULE:FREQ=DAILY\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER;VALUE=DATE-TIME:20261028T094500Z\r\nDESCRIPTION:Alarm\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let mut plan = ReminderAccountPlan {
            records: Vec::new(),
            unsupported_count: 0,
            completed_collections: Vec::new(),
            failed_collections: 0,
            failure_reason: None,
        };
        for raw in [valid.as_slice(), unsupported.as_slice()] {
            match reminder_resource_coverage(raw) {
                ReminderResourceCoverage::Unsupported => plan.unsupported_count += 1,
                ReminderResourceCoverage::Standalone => {
                    for reminder in isyncyou_connectors::dav::reminders::plan(
                        raw, raw, None, 1793180000, 1793190000,
                    )
                    .unwrap()
                    {
                        plan.records
                            .push(serde_json::json!({"trigger_at":reminder.trigger_at}));
                    }
                }
                _ => panic!("unexpected coverage"),
            }
        }
        assert_eq!(plan.unsupported_count, 1);
        assert_eq!(plan.records.len(), 1);
        assert_eq!(plan.records[0]["trigger_at"], 1793180700);
    }

    #[test]
    fn recovery_token_seam_rejects_unconfigured_origin_before_network() {
        use sha2::{Digest, Sha256};
        let mut cfg = Config::default();
        cfg.provider_accounts.insert(
            "account".into(),
            ProviderConfig::SelfHosted {
                origin: "https://one.example.test".into(),
                client_id: "client".into(),
            },
        );
        let store = Store::open_in_memory().unwrap();
        let original = b"BEGIN:VCARD\r\nVERSION:4.0\r\nUID:old\r\nFN:Example\r\nEND:VCARD\r\n";
        let digest = format!("{:x}", Sha256::digest(original));
        let result = import_recovered_with_token(
            &cfg,
            "account",
            "https://other.example.test",
            &store,
            "/.well-known/carddav",
            "https://other.example.test/book/",
            "contacts",
            "op",
            "gen",
            "object",
            "v1",
            original,
            &digest,
            &std::collections::HashMap::new(),
            Some("replacement"),
            "unused-token",
        );
        assert!(matches!(result,Err(ref error) if error == "Account or provider changed"));
    }

    #[test]
    fn recovery_timeout_and_throttle_remain_unknown_and_readback_only() {
        let store = Store::open_in_memory().unwrap();
        let scope = ProviderScope {
            account_id: "fixture".into(),
            origin: "https://example.test".into(),
            service: "contacts".into(),
            collection: "https://example.test/book/".into(),
        };
        for (operation, code) in [("timeout", 408), ("throttle", 429)] {
            assert!(store
                .claim_provider_mutation(&scope, operation, "dav_recovery", "{}", "digest")
                .unwrap());
            let readbacks = std::cell::Cell::new(0);
            let result = finish_recovery_dispatch(
                &store,
                &scope,
                operation,
                Err(DavError::Http(code)),
                || {
                    readbacks.set(readbacks.get() + 1);
                    Ok(RecoveryImportOutcome::Unknown)
                },
            )
            .unwrap();
            assert!(matches!(result, RecoveryImportOutcome::Unknown));
            assert_eq!(readbacks.get(), 1);
            assert_eq!(
                store
                    .provider_mutation(&scope, operation)
                    .unwrap()
                    .unwrap()
                    .1,
                "unknown"
            );
        }
        assert!(store
            .claim_provider_mutation(&scope, "rejected", "dav_recovery", "{}", "digest")
            .unwrap());
        let result =
            finish_recovery_dispatch(&store, &scope, "rejected", Err(DavError::Http(403)), || {
                panic!("definitive rejection should not be read back")
            })
            .unwrap();
        assert!(matches!(result, RecoveryImportOutcome::Rejected));
        assert_eq!(
            store
                .provider_mutation(&scope, "rejected")
                .unwrap()
                .unwrap()
                .1,
            "rejected"
        );
    }

    #[test]
    fn embedded_contact_photo_rejects_remote_and_invalid_data() {
        assert!(decode_embedded_contact_photo("https://example.test/photo").is_err());
        assert!(decode_embedded_contact_photo("data:image/png;base64,QUJD").is_err());
        let uri = format!("data:image/png;base64,{}", "A".repeat(2_796_208));
        assert!(decode_embedded_contact_photo(&uri).is_err());
        let (media, bytes) =
            decode_embedded_contact_photo("data:image/png;base64,iVBORw0KGgo=").unwrap();
        assert_eq!(media, "image/png");
        assert_eq!(bytes, b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn contact_delete_projection_summarizes_photo_without_exposing_bytes() {
        let mut projection = serde_json::json!([
            "vcard",
            [
                ["fn", {}, "text", "Example"],
                ["photo", {}, "uri", "data:image/png;base64,iVBORw0KGgo="]
            ]
        ]);
        let photos = redact_contact_photos(&mut projection).unwrap();
        assert_eq!(photos.len(), 1);
        assert_eq!(photos[0].image_size, Some(8));
        assert_eq!(photos[0].value_sha256.len(), 64);
        assert!(projection.to_string().contains("Example"));
        assert!(!projection.to_string().contains("iVBORw0KGgo="));
    }

    #[test]
    fn grouped_contact_summary_indexes_match_native_field_edits() {
        let raw = b"BEGIN:VCARD\r\nVERSION:4.0\r\nUID:id\r\nFN:Example\r\nitem1.EMAIL;TYPE=work:first@example.test\r\nEMAIL;TYPE=home:second@example.test\r\nitem2.EMAIL;TYPE=other:third@example.test\r\nEND:VCARD\r\n";
        let projection = isyncyou_connectors::dav::projection::contact(raw).unwrap();
        let summary = contact_summary(&projection).unwrap();
        assert_eq!(summary.emails.len(), 3);
        for (index, expected) in [
            "first@example.test",
            "second@example.test",
            "third@example.test",
        ]
        .iter()
        .enumerate()
        {
            let field = isyncyou_connectors::dav::edit::preview_contact_field(
                raw,
                "EMAIL",
                index,
                "changed@example.test",
            )
            .unwrap();
            assert_eq!(field.old_value, *expected);
            assert_eq!(summary.emails[index].values[0].as_str(), Some(*expected));
        }
    }

    #[test]
    fn native_reminder_records_preserve_claim_identity_and_limit_size() {
        let scope = ProviderScope {
            account_id: "a".into(),
            origin: "https://example.test".into(),
            service: "calendar".into(),
            collection: "https://example.test/cal/".into(),
        };
        let identity = isyncyou_connectors::dav::reminders::Reminder {
            uid: "event".into(),
            recurrence_id: "20261025T100000".into(),
            alarm_id: "alarm".into(),
            trigger_at: 1792918800,
        };
        let tag = identity.notification_tag(&scope);
        let mut plan = ReminderPlan {
            scope,
            href: "https://example.test/cal/event.ics".into(),
            etag: "\"v1\"".into(),
            occurrence_start: "20261025T000000Z".into(),
            occurrence_end: "20261026T000000Z".into(),
            window_start: 1792886400,
            window_end: 1792972800,
            items: vec![ReminderPlanItem {
                identity,
                notification_tag: tag.clone(),
            }],
        };
        let record = plan
            .native_records(Some("Europe/Vienna"))
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            reminder_identity(&record)
                .unwrap()
                .notification_tag(&plan.scope),
            tag
        );
        assert_eq!(record["floating_tzid"], "Europe/Vienna");
        plan.href = "x".repeat(5000);
        assert!(plan.native_records(None).is_err());
    }

    #[test]
    fn contact_summary_preserves_multiple_fields_and_structured_organizations() {
        let jcard = serde_json::json!(["vcard",[["fn",{},"text","Example"],["email",{"type":"work"},"text","a@example.test"],["tel",{},"uri","tel:+123"],["org",{},"text",["Company","Team"]]]]);
        let summary = contact_summary(&jcard).unwrap();
        assert_eq!(summary.names[0].values[0], "Example");
        assert_eq!(summary.emails[0].parameters["type"], "work");
        assert_eq!(summary.phones[0].value_type, "uri");
        assert_eq!(summary.organizations[0].values[0][1], "Team");
        assert!(contact_summary(&serde_json::json!([
            "vcard",
            [["email", {}, "text", "a@example.test"]]
        ]))
        .is_err());
        assert!(contact_summary(&serde_json::json!(["vcard", [["fn", {}, "text"]]])).is_err());
    }
    use isyncyou_store::ProviderDavIndex;

    #[test]
    fn parent_preparation_rejects_cycle_and_binds_collection_checkpoint() {
        let store = Store::open_in_memory().unwrap();
        let scope = ProviderScope {
            account_id: "fixture".into(),
            origin: "https://example.test".into(),
            service: "calendar".into(),
            collection: "https://example.test/cal/".into(),
        };
        store.begin_provider_staging(&scope, "generation").unwrap();
        for (id, parent) in [("a", None), ("b", Some("a"))] {
            let href = format!("https://example.test/cal/{id}.ics");
            let relationship = parent
                .map(|p| format!("RELATED-TO:{p}\r\n"))
                .unwrap_or_default();
            let raw = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:{id}\r\nSUMMARY:Task {id}\r\n{relationship}END:VTODO\r\nEND:VCALENDAR\r\n").into_bytes();
            let object = ProviderObject {
                remote_id: href.clone(),
                etag: Some("\"v1\"".into()),
                raw,
            };
            store
                .replace_provider_staged_object(&scope, "generation", &object)
                .unwrap();
            let index = ProviderDavIndex::Valid {
                uid: id.into(),
                kind: "task".into(),
                parent_uid: parent.map(str::to_owned),
            };
            store
                .stage_provider_dav_index(&scope, "generation", &href, &index)
                .unwrap();
            store
                .stage_provider_alarm_index(
                    &scope,
                    "generation",
                    &href,
                    &isyncyou_store::ProviderAlarmIndex::Valid { has_alarm: false },
                )
                .unwrap();
        }
        store
            .finish_provider_staging(&scope, "generation", "https://example.test/token/1")
            .unwrap();
        assert!(prepare_task_parent_in_store(
            &store,
            &scope,
            "https://example.test/cal/a.ics",
            "a",
            Some("b")
        )
        .is_err());
        let edit = prepare_task_parent_in_store(
            &store,
            &scope,
            "https://example.test/cal/b.ics",
            "b",
            None,
        )
        .unwrap();
        assert_eq!(
            edit.expected_collection_token.as_deref(),
            Some("https://example.test/token/1")
        );
        assert_eq!(edit.expected_etag, "\"v1\"");
        assert!(prepare_task_delete_in_store(
            &store,
            &scope,
            "https://example.test/cal/a.ics",
            "a"
        )
        .is_err());
        let (summary, parent, etag, checkpoint) =
            prepare_task_delete_in_store(&store, &scope, "https://example.test/cal/b.ics", "b")
                .unwrap();
        assert_eq!(summary, "Task b");
        assert_eq!(parent.as_deref(), Some("a"));
        assert_eq!(etag, "\"v1\"");
        assert_eq!(checkpoint, "https://example.test/token/1");
    }
}
