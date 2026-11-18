//! Mobile OneNote refresh with network I/O outside the shared store gate.
//!
//! A retained read connection detects intervening commits. Every network result
//! is applied only after the account, credential file, database identity and
//! reader generation have been checked again under the gate.
use crate::{auth, unix_now};
use isyncyou_connectors::archive::{self, JsonFetcher};
use isyncyou_connectors::onenote;
use isyncyou_core::{config::AccountConfig, Config};
use isyncyou_graph::GraphClient;
use isyncyou_graph::Transport;
use isyncyou_store::Store;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex};
use std::time::Duration;

#[cfg(unix)]
fn file_identity(path: &Path) -> Result<(u64, u64, i64, i64), String> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(path).map_err(|_| "identity unavailable")?;
    if !m.file_type().is_file() {
        return Err("invalid identity file".into());
    }
    Ok((m.dev(), m.ino(), m.ctime(), m.ctime_nsec()))
}

struct Guard {
    account: AccountConfig,
    credential: isyncyou_graph::auth::TokenCache,
    credential_path: PathBuf,
    credential_identity: (u64, u64, i64, i64),
    db_identity: isyncyou_store::ReadonlyCacheIdentity,
    reader: Store,
    version: i64,
}

fn prepare(cfg: &Config, account: &str) -> Result<Guard, String> {
    if cfg.provider_accounts.contains_key(account) {
        return Err("provider changed".into());
    }
    let acc = cfg
        .accounts
        .iter()
        .find(|a| a.id == account)
        .cloned()
        .ok_or("account absent")?;
    let credential_path = auth::read_token_cache_path(cfg, account).ok_or("credential absent")?;
    let credential_identity = file_identity(&credential_path)?;
    let credential = isyncyou_graph::auth::TokenCache::load(&credential_path)
        .map_err(|_| "reader unavailable")?;
    if !credential.is_access_valid(unix_now().parse().unwrap_or(0))
        || file_identity(&credential_path)? != credential_identity
    {
        return Err("reader unavailable".into());
    }
    let path = acc.archive_root.join(".isyncyou-store.db");
    // Initialize/migrate while the caller holds the shared gate, then release
    // the exclusive writer lock before starting any network operation.
    drop(Store::open(&path).map_err(|_| "store unavailable")?);
    let db_identity =
        Store::readonly_cache_identity(&path).map_err(|_| "store identity unavailable")?;
    let reader = Store::open_readonly(&path).map_err(|_| "reader unavailable")?;
    let version = reader.data_version().map_err(|_| "version unavailable")?;
    Ok(Guard {
        account: acc,
        credential,
        credential_path,
        credential_identity,
        db_identity,
        reader,
        version,
    })
}

fn validate(guard: &Guard, cfg: &Config, account: &str) -> Result<Store, String> {
    if cfg.provider_accounts.contains_key(account)
        || cfg.accounts.iter().find(|a| a.id == account) != Some(&guard.account)
        || auth::read_token_cache_path(cfg, account).as_ref() != Some(&guard.credential_path)
        || file_identity(&guard.credential_path)? != guard.credential_identity
    {
        return Err("account or grant changed".into());
    }
    let credential = isyncyou_graph::auth::TokenCache::load(&guard.credential_path)
        .map_err(|_| "reader unavailable")?;
    if !credential.is_access_valid(unix_now().parse().unwrap_or(0))
        || credential.access_token != guard.credential.access_token
        || credential.refresh_token != guard.credential.refresh_token
        || credential.account_subject != guard.credential.account_subject
        || credential.expires_at != guard.credential.expires_at
    {
        return Err("grant changed".into());
    }
    let path = guard.account.archive_root.join(".isyncyou-store.db");
    validate_store(&guard.reader, guard.version, &guard.db_identity, &path)?;
    let store = Store::open(&path).map_err(|_| "store unavailable")?;
    // The process gate does not cover another process: close the window
    // between the first check and acquiring the store's exclusive lock.
    validate_store(&guard.reader, guard.version, &guard.db_identity, &path)?;
    Ok(store)
}

fn validate_store(
    reader: &Store,
    version: i64,
    identity: &isyncyou_store::ReadonlyCacheIdentity,
    path: &Path,
) -> Result<(), String> {
    if &Store::readonly_cache_identity(path).map_err(|_| "store identity unavailable")? != identity
        || reader.data_version().map_err(|_| "version unavailable")? != version
    {
        return Err("store changed during OneNote fetch".into());
    }
    Ok(())
}

fn fetch<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
    serve_target: &mut impl FnMut(),
) -> Result<T, String> {
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Timeout) => serve_target(),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("OneNote network worker failed".into())
            }
        }
    }
}

const MAX_LIST_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_ARCHIVE_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
struct BoundedGraph(GraphClient);
impl Transport for BoundedGraph {
    fn get(&mut self, url: &str) -> isyncyou_graph::client::Response {
        match self
            .0
            .get_bytes_bounded(url, MAX_LIST_RESPONSE_BYTES)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        {
            Some(value) => isyncyou_graph::client::Response::ok(value),
            None => isyncyou_graph::client::Response::status(503),
        }
    }
}
struct ReplayJson(HashMap<String, Value>);
impl JsonFetcher for ReplayJson {
    fn fetch_json(&self, url: &str) -> Result<Value, String> {
        self.0
            .get(url)
            .cloned()
            .ok_or_else(|| "OneNote hierarchy plan changed".into())
    }
}
/// Two bounded continuation keys provide fair progress across refresh passes.
#[derive(Default)]
pub struct RefreshCursor {
    account: Option<AccountConfig>,
    subject: Option<String>,
    credential_identity: Option<(u64, u64, i64, i64)>,
    body_after: Option<String>,
    resource_page: Option<String>,
    resource_url: Option<String>,
    resource_scan_finished: bool,
    manifest_after: Option<String>,
    manifest_failed: bool,
    verification: Option<VerificationCursor>,
}

struct VerificationCursor {
    page_generation: [u8; 32],
    offset: usize,
}

fn next_resource_page(
    pages: &[isyncyou_store::Item],
    current: Option<&str>,
) -> Option<(isyncyou_store::Item, Option<String>)> {
    let index = pages
        .iter()
        .position(|page| current.is_none_or(|id| page.remote_id.as_str() >= id))?;
    Some((
        pages[index].clone(),
        pages.get(index + 1).map(|item| item.remote_id.clone()),
    ))
}

fn verification_range(
    cursor: &mut Option<VerificationCursor>,
    pages: &[isyncyou_store::Item],
) -> (usize, usize) {
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    for page in pages {
        for value in [
            Some(page.remote_id.as_str()),
            page.local_path.as_deref(),
            page.etag.as_deref(),
            page.body_etag.as_deref(),
            page.remote_mtime.as_deref(),
            page.body_sha256.as_deref(),
        ] {
            match value {
                Some(value) => {
                    digest.update(&[1]);
                    digest.update(&(value.len() as u64).to_be_bytes());
                    digest.update(value.as_bytes());
                }
                None => digest.update(&[0]),
            }
        }
    }
    let page_generation: [u8; 32] = digest.finish().as_ref().try_into().expect("SHA-256 length");
    if cursor
        .as_ref()
        .is_none_or(|v| v.page_generation != page_generation)
    {
        *cursor = Some(VerificationCursor {
            page_generation,
            offset: 0,
        });
    }
    let start = cursor.as_ref().unwrap().offset.min(pages.len());
    (start, start.saturating_add(8).min(pages.len()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshOutcome {
    pub changed: bool,
    pub partial: bool,
}

fn persist_refresh_state(root: &Path, partial: bool) -> Result<(), String> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let state = if partial { "partial" } else { "complete" };
    let bytes = serde_json::to_vec(&serde_json::json!({ "state": state }))
        .map_err(|_| "OneNote refresh state unavailable")?;
    let target = root.join(archive::ONENOTE_MOBILE_REFRESH_STATE_FILE);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = root.join(format!(
        ".onenote-refresh-{}-{nonce}-{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut created = false;
    let result = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        created = true;
        file.write_all(&isyncyou_core::envelope::seal_for_disk(&bytes))?;
        file.sync_all()?;
        std::fs::rename(&temporary, &target)?;
        std::fs::File::open(root)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() && created {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|_| "OneNote refresh state unavailable".into())
}

fn bounded_archive_result(
    result: Result<Vec<u8>, isyncyou_graph::http::UploadError>,
    failure: &'static str,
) -> Result<Option<Vec<u8>>, String> {
    match result {
        Ok(bytes) => Ok(Some(bytes)),
        Err(isyncyou_graph::http::UploadError::TooLarge) => Ok(None),
        Err(_) => Err(failure.into()),
    }
}

/// Every stage opens the writer only after its network work has completed.
/// Fetches are bounded to one body/resource at a time, preserving the existing
/// 50-item pass limits and existing archive implementations.
pub fn refresh(
    gate: &Mutex<()>,
    account: &str,
    cursor: &mut RefreshCursor,
    mut current_config: impl FnMut() -> Result<Config, String>,
    mut serve_target: impl FnMut(),
    mut notify_changed: impl FnMut(),
) -> Result<RefreshOutcome, String> {
    let guard = {
        let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
        prepare(&current_config()?, account)?
    };
    // Persist before fetching or replacing any content. An interruption leaves
    // an honest partial state rather than the preceding pass's success.
    persist_refresh_state(&guard.account.archive_root, true)?;
    if cursor.account.as_ref() != Some(&guard.account)
        || cursor.subject != guard.credential.account_subject
        || cursor.credential_identity != Some(guard.credential_identity)
    {
        *cursor = RefreshCursor {
            account: Some(guard.account.clone()),
            subject: guard.credential.account_subject.clone(),
            credential_identity: Some(guard.credential_identity),
            ..Default::default()
        };
    }
    let token = guard.credential.access_token.clone();
    let snapshot = fetch(
        move || {
            onenote::fetch_onenote_page_snapshot_bounded(&mut BoundedGraph(GraphClient::new(token)))
                .map_err(|_| "OneNote page fetch failed".to_string())
        },
        &mut serve_target,
    )?;
    let mut changed = {
        let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
        let store = validate(&guard, &current_config()?, account)?;
        let report = onenote::reconcile_onenote_page_snapshot(
            snapshot,
            &store,
            account,
            &unix_now(),
            Some(&guard.account.archive_root),
        )
        .map_err(|_| "OneNote page reconciliation failed")?;
        report.upserted + report.deleted > 0
    };
    drop(guard);
    let mut notified = changed;
    if notified {
        notify_changed();
    }

    let mut deferred_bodies = HashSet::new();
    let mut body_wrapped = false;
    let mut bodies_complete = false;
    for _ in 0..50 {
        let (guard, item) = {
            let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
            let guard = prepare(&current_config()?, account)?;
            let item = archive::next_onenote_body_after(
                &guard.reader,
                account,
                &deferred_bodies,
                cursor.body_after.as_deref(),
            )
            .map_err(|_| "body plan failed")?;
            (guard, item)
        };
        let Some(item) = item else {
            if cursor.body_after.is_some() && !body_wrapped {
                cursor.body_after = None;
                body_wrapped = true;
                continue;
            }
            bodies_complete = true;
            break;
        };
        if item.remote_id.len() > 4096 {
            return Err("OneNote body cursor too large".into());
        }
        let previous = cursor.body_after.clone();
        let url = format!("/me/onenote/pages/{}/content", item.remote_id);
        let token = guard.credential.access_token.clone();
        let request_url = url.clone();
        let bytes = fetch(
            move || {
                bounded_archive_result(
                    GraphClient::new(token)
                        .get_bytes_bounded(&request_url, MAX_ARCHIVE_RESPONSE_BYTES),
                    "OneNote body fetch failed",
                )
            },
            &mut serve_target,
        )?;
        let Some(bytes) = bytes else {
            cursor.body_after = Some(item.remote_id.clone());
            deferred_bodies.insert(item.remote_id);
            drop(guard);
            serve_target();
            continue;
        };
        let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
        let store = validate(&guard, &current_config()?, account)?;
        if archive::next_onenote_body_after(&store, account, &deferred_bodies, previous.as_deref())
            .map_err(|_| "body plan failed")?
            .as_ref()
            != Some(&item)
        {
            return Err("body plan changed".into());
        }
        archive::commit_onenote_body(&store, account, &item, &guard.account.archive_root, &bytes)
            .map_err(|_| "OneNote body commit failed")?;
        changed = true;
        cursor.verification = None;
        cursor.manifest_after = None;
        cursor.manifest_failed = false;
        cursor.body_after = Some(item.remote_id);
        drop(store);
        drop(_g);
        if !notified {
            notify_changed();
            notified = true;
        }
        serve_target();
    }

    let mut resources_complete = false;
    for _ in 0..50 {
        let guard = {
            let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
            prepare(&current_config()?, account)?
        };
        let pages = guard
            .reader
            .items_by_type(account, "onenote", "page")
            .map_err(|_| "OneNote resource plan failed")?;
        if !cursor.resource_scan_finished {
            let current = next_resource_page(&pages, cursor.resource_page.as_deref());
            if let Some((page, next)) = current {
                let root = guard.account.archive_root.clone();
                let worker_page = page.clone();
                let after = if cursor.resource_page.as_deref() == Some(page.remote_id.as_str()) {
                    cursor.resource_url.clone()
                } else {
                    None
                };
                let result = fetch(
                    move || {
                        Ok(
                            archive::plan_onenote_resource(&root, &worker_page, after.as_deref())
                                .ok(),
                        )
                    },
                    &mut serve_target,
                )?;
                if result.is_none() {
                    cursor.manifest_failed = true;
                }
                let Some(plan) = result.flatten() else {
                    cursor.resource_page = next;
                    cursor.resource_url = None;
                    if cursor.resource_page.is_none() {
                        cursor.resource_scan_finished = true;
                    }
                    continue;
                };
                if page.remote_id.len() > 4096 || plan.url.len() > 4096 {
                    return Err("OneNote resource cursor too large".into());
                }
                let token = guard.credential.access_token.clone();
                let request_url = plan.url.clone();
                let bytes = fetch(
                    move || {
                        bounded_archive_result(
                            GraphClient::new(token)
                                .get_bytes_bounded(&request_url, MAX_ARCHIVE_RESPONSE_BYTES),
                            "OneNote resource fetch failed",
                        )
                    },
                    &mut serve_target,
                )?;
                let Some(bytes) = bytes else {
                    cursor.resource_page = Some(page.remote_id);
                    cursor.resource_url = Some(plan.url);
                    cursor.manifest_failed = true;
                    continue;
                };
                let plan_url = plan.url.clone();
                let prepared = fetch(
                    move || {
                        archive::prepare_planned_onenote_resource(plan, &bytes)
                            .map_err(|_| "OneNote resource preparation failed".to_string())
                    },
                    &mut serve_target,
                )?;
                let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
                let store = validate(&guard, &current_config()?, account)?;
                if store
                    .get_item(account, "onenote", &page.remote_id)
                    .map_err(|_| "OneNote resource plan failed")?
                    .as_ref()
                    != Some(&page)
                {
                    return Err("OneNote resource plan changed".into());
                }
                archive::commit_planned_onenote_resource(prepared)
                    .map_err(|_| "OneNote resource commit failed")?;
                changed = true;
                cursor.verification = None;
                cursor.manifest_after = None;
                cursor.resource_page = Some(page.remote_id);
                cursor.resource_url = Some(plan_url);
                drop(store);
                drop(_g);
                if !notified {
                    notify_changed();
                    notified = true;
                }
                serve_target();
                continue;
            }
            cursor.resource_scan_finished = true;
        }
        // Prepare one page outside the global gate, then validate the original
        // account/store and every file identity before publishing its manifest.
        let page = pages.into_iter().find(|page| {
            cursor
                .manifest_after
                .as_deref()
                .is_none_or(|after| page.remote_id.as_str() > after)
        });
        let Some(page) = page else {
            resources_complete = !cursor.manifest_failed;
            cursor.resource_page = None;
            cursor.resource_url = None;
            cursor.resource_scan_finished = false;
            cursor.manifest_after = None;
            cursor.manifest_failed = false;
            break;
        };
        let root = guard.account.archive_root.clone();
        let worker_page = page.clone();
        let prepared = fetch(
            move || Ok(archive::prepare_onenote_manifest(&root, &worker_page).ok()),
            &mut serve_target,
        )?;
        let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
        let store = validate(&guard, &current_config()?, account)?;
        if store
            .get_item(account, "onenote", &page.remote_id)
            .map_err(|_| "OneNote manifest plan failed")?
            .as_ref()
            != Some(&page)
        {
            return Err("OneNote manifest plan changed".into());
        }
        match prepared {
            Some(Some(manifest)) => archive::publish_onenote_manifest(manifest)
                .map_err(|_| "OneNote manifest commit failed")?,
            Some(None) if page.local_path.is_some() => {}
            _ => cursor.manifest_failed = true,
        }
        cursor.manifest_after = Some(page.remote_id);
        break;
    }
    if !deferred_bodies.is_empty() || cursor.manifest_failed {
        eprintln!("isyncyou: OneNote mobile archive deferred oversized or unavailable objects");
    }

    let guard = {
        let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
        prepare(&current_config()?, account)?
    };
    let token = guard.credential.access_token.clone();
    let hierarchy = fetch(
        move || {
            let client = GraphClient::new(token);
            let mut values = HashMap::new();
            for url in [
                "/me/onenote/notebooks?$top=100",
                "/me/onenote/sectionGroups?$top=100",
                "/me/onenote/sections?$top=100",
            ] {
                let bytes = client
                    .get_bytes_bounded(url, MAX_LIST_RESPONSE_BYTES)
                    .map_err(|_| "OneNote hierarchy fetch failed")?;
                let value =
                    serde_json::from_slice(&bytes).map_err(|_| "OneNote hierarchy JSON invalid")?;
                values.insert(url.to_string(), value);
            }
            Ok(values)
        },
        &mut serve_target,
    )?;
    let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
    let store = validate(&guard, &current_config()?, account)?;
    let report = onenote::backup_onenote_hierarchy(
        &ReplayJson(hierarchy),
        &store,
        account,
        &guard.account.archive_root,
    )
    .map_err(|_| "OneNote hierarchy commit failed")?;
    changed |= report.notebooks + report.section_groups + report.sections > 0;
    let mut partial = !bodies_complete || !resources_complete || !deferred_bodies.is_empty();
    drop(store);
    drop(_g);
    let mut verify_db = None;
    if !partial {
        let db_path = guard.account.archive_root.join(".isyncyou-store.db");
        let identity = Store::readonly_cache_identity(&db_path)
            .map_err(|_| "OneNote archive state unavailable")?;
        let reader =
            Store::open_readonly(&db_path).map_err(|_| "OneNote archive state unavailable")?;
        let version = reader
            .data_version()
            .map_err(|_| "OneNote archive state unavailable")?;
        let pages = reader
            .items_by_type(account, "onenote", "page")
            .map_err(|_| "OneNote archive state unavailable")?;
        validate_store(&reader, version, &identity, &db_path)?;
        let (offset, end) = verification_range(&mut cursor.verification, &pages);
        let batch = pages[offset..end].to_vec();
        let root = guard.account.archive_root.clone();
        // File reads and decryption run outside the global gate, with at most
        // eight bounded pages per pass; targeted requests continue meanwhile.
        let complete = fetch(
            move || {
                Ok(batch
                    .iter()
                    .all(|page| archive::onenote_page_generation_complete(&root, page)))
            },
            &mut serve_target,
        )?;
        validate_store(&reader, version, &identity, &db_path)?;
        let verification = cursor.verification.as_mut().unwrap();
        verification.offset = end;
        partial = !complete || end < pages.len();
        if !complete {
            cursor.verification = None;
        }
        verify_db = Some((reader, identity, version, db_path));
    } else {
        cursor.verification = None;
    }
    let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
    let current = prepare(&current_config()?, account)?;
    if current.account != guard.account
        || current.credential_identity != guard.credential_identity
        || current.credential.access_token != guard.credential.access_token
        || current.credential.refresh_token != guard.credential.refresh_token
        || current.credential.account_subject != guard.credential.account_subject
    {
        cursor.verification = None;
        return Err("account or grant changed".into());
    }
    if let Some((reader, identity, version, path)) = verify_db.as_ref() {
        validate_store(reader, *version, identity, path)?;
    }
    persist_refresh_state(&guard.account.archive_root, partial)?;
    if !partial {
        cursor.verification = None;
    }
    Ok(RefreshOutcome { changed, partial })
}

#[cfg(test)]
mod tests {
    use super::*;
    use isyncyou_store::Item;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    #[test]
    fn wp16_onenote_resource_page_cursor_reaches_beyond_fifty_and_missing_page() {
        let mut pages: Vec<_> = (0..52)
            .map(|n| Item::new("a", "onenote", format!("p{n:03}"), "page", "page"))
            .collect();
        let mut cursor = None;
        for index in 0..50 {
            let (page, next) = next_resource_page(&pages, cursor.as_deref()).unwrap();
            assert_eq!(page.remote_id, format!("p{index:03}"));
            cursor = next;
        }
        pages.retain(|page| page.remote_id != "p050");
        let (page, next) = next_resource_page(&pages, cursor.as_deref()).unwrap();
        assert_eq!(page.remote_id, "p051");
        assert!(next.is_none());
        pages.insert(0, Item::new("a", "onenote", "p-early", "new", "page"));
        assert_eq!(
            next_resource_page(&pages, None).unwrap().0.remote_id,
            "p-early"
        );
    }

    #[test]
    fn wp16_onenote_resource_cursor_resumes_after_deferred_and_early_insert() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let url = "https://graph.microsoft.com/v1.0/me/onenote/resources/r/content";
        let html = format!("<img src=\"{url}\">");
        let mut pages = Vec::new();
        for n in 0..52 {
            let id = format!("p{n:03}");
            let mut page = Item::new("a", "onenote", &id, "page", "page");
            page.local_path = Some(format!("{id}.html"));
            if n != 5 {
                std::fs::write(root.join(page.local_path.as_ref().unwrap()), &html).unwrap();
            }
            pages.push(page);
        }
        let mut cursor = None;
        let mut deferred = false;
        for _ in 0..50 {
            let (page, next) = next_resource_page(&pages, cursor.as_deref()).unwrap();
            match archive::plan_onenote_resource(root, &page, None) {
                Ok(Some(plan)) => {
                    let part = archive::prepare_planned_onenote_resource(plan, b"data").unwrap();
                    archive::commit_planned_onenote_resource(part).unwrap();
                }
                Err(_) => deferred = true,
                Ok(None) => panic!("fixture page unexpectedly has no resource"),
            }
            cursor = next;
        }
        assert!(deferred);
        assert_eq!(cursor.as_deref(), Some("p050"));
        assert!(archive::plan_onenote_resource(root, &pages[50], None)
            .unwrap()
            .is_some());
        std::fs::write(root.join("p005.html"), &html).unwrap();
        let mut early = Item::new("a", "onenote", "p-early", "page", "page");
        early.local_path = Some("early.html".into());
        std::fs::write(root.join("early.html"), &html).unwrap();
        pages.insert(0, early);
        let (first, _) = next_resource_page(&pages, None).unwrap();
        assert_eq!(first.remote_id, "p-early");
        assert!(archive::plan_onenote_resource(root, &first, None)
            .unwrap()
            .is_some());
        assert!(archive::plan_onenote_resource(root, &pages[6], None)
            .unwrap()
            .is_some());
    }

    #[test]
    fn wp16_onenote_verification_progress_survives_hierarchy_updates() {
        let store = Store::open_in_memory().unwrap();
        for n in 0..9 {
            store
                .upsert_item(&Item::new("a", "onenote", format!("p{n}"), "page", "page"))
                .unwrap();
        }
        store
            .upsert_item(&Item::new("a", "onenote", "book", "book", "notebook"))
            .unwrap();
        let pages = store.items_by_type("a", "onenote", "page").unwrap();
        let mut cursor = None;
        assert_eq!(verification_range(&mut cursor, &pages), (0, 8));
        cursor.as_mut().unwrap().offset = 8;
        store
            .upsert_item(&Item::new("a", "onenote", "book", "updated", "notebook"))
            .unwrap();
        let pages = store.items_by_type("a", "onenote", "page").unwrap();
        assert_eq!(verification_range(&mut cursor, &pages), (8, 9));
        let mut changed = Item::new("a", "onenote", "p0", "changed", "page");
        changed.remote_mtime = Some("new-generation".into());
        store.upsert_item(&changed).unwrap();
        let pages = store.items_by_type("a", "onenote", "page").unwrap();
        assert_eq!(verification_range(&mut cursor, &pages), (0, 8));
    }

    #[test]
    fn wp16_onenote_partial_survives_oversize_and_fetch_failure() {
        use isyncyou_graph::http::UploadError;
        let dir = tempfile::tempdir().unwrap();
        persist_refresh_state(dir.path(), false).unwrap();
        // Start a new pass; both a deferred object and an interrupted fetch
        // must leave the durable state partial until a complete pass finishes.
        persist_refresh_state(dir.path(), true).unwrap();
        assert!(bounded_archive_result(Err(UploadError::TooLarge), "failed")
            .unwrap()
            .is_none());
        assert!(bounded_archive_result(Err(UploadError::Incomplete), "failed").is_err());
        let raw = isyncyou_core::envelope::read_body(
            &dir.path().join(archive::ONENOTE_MOBILE_REFRESH_STATE_FILE),
        )
        .unwrap();
        let status: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(status["state"], "partial");
        persist_refresh_state(dir.path(), false).unwrap();
        let raw = isyncyou_core::envelope::read_body(
            &dir.path().join(archive::ONENOTE_MOBILE_REFRESH_STATE_FILE),
        )
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&raw).unwrap()["state"],
            "complete"
        );
        assert!(persist_refresh_state(&dir.path().join("missing-directory"), true).is_err());
    }

    #[test]
    fn wp16_onenote_partial_survives_process_restart() {
        const MARKER: &str = "ISYNCYOU_ONENOTE_STATUS_TEST_ROOT";
        if let Some(root) = std::env::var_os(MARKER) {
            let raw = isyncyou_core::envelope::read_body(
                &PathBuf::from(root).join(archive::ONENOTE_MOBILE_REFRESH_STATE_FILE),
            )
            .unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&raw).unwrap()["state"],
                "partial"
            );
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        persist_refresh_state(dir.path(), true).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("onenote_mobile_refresh::tests::wp16_onenote_partial_survives_process_restart")
            .arg("--test-threads=1")
            .env(MARKER, dir.path())
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn store_change_during_fetch_rejects_commit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        drop(Store::open(&path).unwrap());
        let identity = Store::readonly_cache_identity(&path).unwrap();
        let reader = Store::open_readonly(&path).unwrap();
        let version = reader.data_version().unwrap();
        assert!(validate_store(&reader, version, &identity, &path).is_ok());
        let writer = Store::open(&path).unwrap();
        writer
            .upsert_item(&Item::new("a", "onenote", "new", "new", "page"))
            .unwrap();
        drop(writer);
        assert!(validate_store(&reader, version, &identity, &path).is_err());
    }

    #[test]
    fn credential_file_replacement_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, b"original").unwrap();
        let before = file_identity(&path).unwrap();
        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, b"original").unwrap();
        std::fs::rename(replacement, &path).unwrap();
        assert_ne!(file_identity(&path).unwrap(), before);
        std::fs::remove_file(&path).unwrap();
        assert!(file_identity(&path).is_err());
    }

    #[test]
    fn signout_and_account_replacement_abort_a_prepared_commit() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("archive");
        std::fs::create_dir_all(&archive).unwrap();
        let mut cfg = Config {
            accounts: vec![AccountConfig {
                id: "a".into(),
                username: "a@example.test".into(),
                sync_root: dir.path().join("sync"),
                archive_root: archive,
                cache_root: dir.path().join("cache"),
                mount_point: None,
            }],
            ..Default::default()
        };
        let path = auth::read_token_cache_path(&cfg, "a").unwrap();
        let token = isyncyou_graph::auth::TokenCache {
            access_token: "test-only-access".into(),
            refresh_token: Some("test-only-refresh".into()),
            expires_at: u64::MAX,
            account_subject: Some("test-subject".into()),
        };
        token.save(&path).unwrap();
        let guard = prepare(&cfg, "a").unwrap();
        assert!(validate(&guard, &cfg, "a").is_ok());
        cfg.accounts[0].username = "different@example.test".into();
        assert!(validate(&guard, &cfg, "a").is_err());
        cfg.accounts[0].username = "a@example.test".into();
        auth::sign_out_reader(&cfg, "a").unwrap();
        assert!(validate(&guard, &cfg, "a").is_err());
        token.save(&path).unwrap();
        assert!(
            validate(&guard, &cfg, "a").is_err(),
            "a recreated grant must not resume a stale snapshot"
        );
    }

    #[test]
    fn waiting_network_worker_serves_target_before_response() {
        let target_ran = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&target_ran);
        let worker = Arc::clone(&target_ran);
        let result = fetch(
            move || {
                let start = std::time::Instant::now();
                while !worker.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(2) {
                    std::thread::yield_now();
                }
                Ok(worker.load(Ordering::SeqCst))
            },
            &mut || {
                observed.store(true, Ordering::SeqCst);
            },
        )
        .unwrap();
        assert!(result);
    }
}
