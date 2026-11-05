//! Mobile OneNote refresh with network I/O outside the shared store gate.
//!
//! A retained read connection detects intervening commits. Every network result
//! is applied only after the account, credential file, database identity and
//! reader generation have been checked again under the gate.
use crate::{auth, unix_now};
use isyncyou_connectors::archive::{self, BytesFetcher, JsonFetcher};
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

struct ReplayBytes {
    url: String,
    bytes: Vec<u8>,
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
impl BytesFetcher for ReplayBytes {
    fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
        if url != self.url {
            return Err("OneNote archive plan changed".into());
        }
        Ok(self.bytes.clone())
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
struct NoNetwork;
impl BytesFetcher for NoNetwork {
    fn fetch_bytes(&self, _url: &str) -> Result<Vec<u8>, String> {
        Err("unexpected OneNote resource fetch".into())
    }
}

/// Two bounded continuation keys provide fair progress across refresh passes.
#[derive(Default)]
pub struct RefreshCursor {
    account: Option<AccountConfig>,
    subject: Option<String>,
    body_after: Option<String>,
    resource_after: Option<(String, String)>,
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
) -> Result<bool, String> {
    let guard = {
        let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
        prepare(&current_config()?, account)?
    };
    if cursor.account.as_ref() != Some(&guard.account)
        || cursor.subject != guard.credential.account_subject
    {
        *cursor = RefreshCursor {
            account: Some(guard.account.clone()),
            subject: guard.credential.account_subject.clone(),
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
            move || match GraphClient::new(token)
                .get_bytes_bounded(&request_url, MAX_ARCHIVE_RESPONSE_BYTES)
            {
                Ok(bytes) => Ok(Some(bytes)),
                Err(isyncyou_graph::http::UploadError::TooLarge) => Ok(None),
                Err(_) => Err("OneNote body fetch failed".to_string()),
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
        cursor.body_after = Some(item.remote_id);
        drop(store);
        drop(_g);
        if !notified {
            notify_changed();
            notified = true;
        }
        serve_target();
    }

    let mut deferred_resources = HashSet::new();
    let mut resource_wrapped = false;
    for _ in 0..50 {
        let (guard, planned) = {
            let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
            let guard = prepare(&current_config()?, account)?;
            let after = cursor
                .resource_after
                .as_ref()
                .map(|(page, url)| (page.as_str(), url.as_str()));
            let planned = archive::next_onenote_resource_after(
                &guard.reader,
                account,
                &guard.account.archive_root,
                &deferred_resources,
                after,
                true,
            )
            .map_err(|_| "resource plan failed")?;
            (guard, planned)
        };
        let Some((item, url)) = planned else {
            if cursor.resource_after.is_some() && !resource_wrapped {
                cursor.resource_after = None;
                resource_wrapped = true;
                continue;
            }
            // A previous pass may have stored the last resource but stopped
            // before publishing its manifest. Rebuild manifests locally.
            let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
            let store = validate(&guard, &current_config()?, account)?;
            archive::backup_onenote_resources_after(
                &NoNetwork,
                &store,
                account,
                &guard.account.archive_root,
                1,
                &deferred_resources,
                None,
                true,
            )
            .map_err(|_| "OneNote manifest commit failed")?;
            break;
        };
        if item.remote_id.len() > 4096 || url.len() > 4096 {
            return Err("OneNote resource cursor too large".into());
        }
        let previous = cursor.resource_after.clone();
        let token = guard.credential.access_token.clone();
        let request_url = url.clone();
        let bytes = fetch(
            move || match GraphClient::new(token)
                .get_bytes_bounded(&request_url, MAX_ARCHIVE_RESPONSE_BYTES)
            {
                Ok(bytes) => Ok(Some(bytes)),
                Err(isyncyou_graph::http::UploadError::TooLarge) => Ok(None),
                Err(_) => Err("OneNote resource fetch failed".to_string()),
            },
            &mut serve_target,
        )?;
        let Some(bytes) = bytes else {
            cursor.resource_after = Some((item.remote_id.clone(), url.clone()));
            deferred_resources.insert((item.remote_id, url));
            drop(guard);
            serve_target();
            continue;
        };
        let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
        let store = validate(&guard, &current_config()?, account)?;
        let after = previous
            .as_ref()
            .map(|(page, url)| (page.as_str(), url.as_str()));
        if archive::next_onenote_resource_after(
            &store,
            account,
            &guard.account.archive_root,
            &deferred_resources,
            after,
            true,
        )
        .map_err(|_| "resource plan failed")?
            != Some((item.clone(), url.clone()))
        {
            return Err("resource plan changed".into());
        }
        let report = archive::backup_onenote_resources_after(
            &ReplayBytes {
                url: url.clone(),
                bytes,
            },
            &store,
            account,
            &guard.account.archive_root,
            1,
            &deferred_resources,
            after,
            true,
        )
        .map_err(|_| "OneNote resource commit failed")?;
        changed |= report.resources > 0;
        cursor.resource_after = Some((item.remote_id, url));
        drop(store);
        drop(_g);
        if report.resources > 0 && !notified {
            notify_changed();
            notified = true;
        }
        serve_target();
    }
    if !deferred_bodies.is_empty() || !deferred_resources.is_empty() {
        eprintln!("isyncyou: OneNote mobile archive deferred oversized objects");
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
    Ok(changed)
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
