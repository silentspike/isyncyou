//! Headless capture on the server. Credentials come from systemd credential files.
mod mail;

use fs2::FileExt;
use isyncyou_connectors::{dav::LoopbackDavReader, jmap::LoopbackJmapReader};
use isyncyou_exporter::{Collection, Event, Manifest, Object};
use isyncyou_store::{ProviderScope, Store};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use url::Url;

type Result<T> = std::result::Result<T, Box<dyn Error>>;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    origin: String,
    trusted_public_origin: String,
    dav_entry: String,
    store: PathBuf,
    archive: PathBuf,
    max_staging_bytes: u64,
    minimum_free_bytes: u64,
}

fn size_tree(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err("symlinks forbidden in staging".into());
    }
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Err("unsupported staging object".into());
    }
    let mut size = 0u64;
    for entry in fs::read_dir(path)? {
        size = size.saturating_add(size_tree(&entry?.path())?);
    }
    Ok(size)
}

fn credential(name: &str) -> Result<Vec<u8>> {
    let dir = PathBuf::from(
        std::env::var_os("CREDENTIALS_DIRECTORY").ok_or("missing systemd credentials")?,
    );
    if !dir.is_absolute() {
        return Err("invalid credential directory".into());
    }
    let value = fs::read(dir.join(name))?;
    if value.is_empty() || value.len() > 8192 {
        return Err("invalid credential".into());
    }
    Ok(value)
}

fn run(config: Config) -> Result<()> {
    let origin = Url::parse(&config.origin)?;
    let loopback = match origin.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    if origin.scheme() != "http"
        || !loopback
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
        || !config.store.is_absolute()
        || !config.archive.is_absolute()
        || config.max_staging_bytes == 0
        || config.minimum_free_bytes < 256 * 1024 * 1024
    {
        return Err("invalid private exporter configuration".into());
    }
    if config.archive.symlink_metadata().is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err("archive path is a symlink".into());
    }
    fs::create_dir_all(&config.archive)?;
    fs::set_permissions(&config.archive, fs::Permissions::from_mode(0o700))?;
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(config.archive.join("collector.lock"))?;
    lock.try_lock_exclusive()?;
    let mut used = size_tree(&config.archive)?;
    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", config.store.display()));
        if path.exists() {
            used = used.saturating_add(size_tree(&path)?);
        }
    }
    if used >= config.max_staging_bytes {
        return Err("export staging limit reached".into());
    }
    let token = String::from_utf8(credential("provider-token")?)?
        .trim()
        .to_owned();
    if token.is_empty() || token.chars().any(char::is_whitespace) {
        return Err("invalid provider token".into());
    }
    let key = credential("store-key")?;
    let store = Store::open_encrypted(&config.store, &key)?;
    // The dedicated collector lock excludes other writers. Preserve captured
    // staged versions before releasing an interrupted source enumeration.
    store.recover_abandoned_provider_staging()?;
    let jmap = LoopbackJmapReader::discover(&config.origin, token.clone(), &config.trusted_public_origin)?;
    let account = jmap.account_id().to_owned();
    let mail_scope = ProviderScope {
        account_id: account.clone(),
        origin: config.origin.clone(),
        service: "mail".into(),
        collection: account.clone(),
    };
    let mailbox_scope = ProviderScope {
        service: "mailbox".into(),
        ..mail_scope.clone()
    };
    let mut budget = mail::Budget {
        path: &config.archive,
        remaining: config.max_staging_bytes - used,
        reserve: config.minimum_free_bytes,
    };
    mail::sync(&jmap, &store, &mail_scope, &mut budget)?;
    mail::sync_mailboxes(&jmap, &store, &mailbox_scope)?;
    let dav = LoopbackDavReader::new(&config.origin, token)?;
    let collections = dav.discover(&config.dav_entry)?;
    if !collections.iter().any(|x| x.calendar) || !collections.iter().any(|x| x.addressbook) {
        return Err("required DAV collections unavailable".into());
    }
    let mut expected = vec![mail_scope, mailbox_scope];
    for collection in collections {
        // Historical ownership follows native DAV owner/principal evidence,
        // never mere read or shared-calendar access.
        match collection.owned_by_current_principal {
            Some(true) => {},
            Some(false) => continue,
            None => return Err("DAV collection ownership not verified".into()),
        }
        for (matches, service) in [
            (collection.calendar, "calendar"),
            (collection.addressbook, "contacts"),
        ] {
            if !matches {
                continue;
            }
            let scope = ProviderScope {
                account_id: account.clone(),
                origin: config.origin.clone(),
                service: service.into(),
                collection: collection.url.clone(),
            };
            // DAV's full snapshot is currently capped by the shared connector;
            // reserve conservatively before invoking it and fail under pressure.
            if budget.remaining < 256 * 1024 * 1024
                || fs2::available_space(&config.archive)?
                    < config.minimum_free_bytes.saturating_add(256 * 1024 * 1024)
            {
                return Err("DAV staging capacity unavailable".into());
            }
            budget.remaining -= 256 * 1024 * 1024;
            dav.sync_collection(&store, &scope)?;
            expected.push(scope);
        }
    }
    if !expected.iter().any(|scope| scope.service == "calendar") ||
       !expected.iter().any(|scope| scope.service == "contacts") {
        return Err("required owned DAV collections unavailable".into());
    }
    let snapshot = store.provider_export_snapshot(&account, &config.origin)?;
    if snapshot.collections.len() != expected.len()
        || snapshot.collections.iter().any(|c| {
            !expected.contains(&c.scope)
                || !c.complete
                || !c.accessible
                || c.cursor.is_none()
                || c.observed_at.unwrap_or(0) <= 0
        })
    {
        return Err("source completeness unavailable".into());
    }
    let collections = snapshot
        .collections
        .iter()
        .map(|c| Collection {
            account: c.scope.account_id.clone(),
            origin: c.scope.origin.clone(),
            service: c.scope.service.clone(),
            collection: c.scope.collection.clone(),
            cursor: c.cursor.clone().unwrap_or_default(),
            observed_at: c.observed_at.unwrap_or_default() as u64,
            complete: true,
        })
        .collect();
    let current = config.archive.join("current");
    let (mut objects, mut events, mut capture, mut after) = if current.exists() {
        let generation = fs::read_to_string(&current)?;
        if generation.len() != 64
            || !generation
                .bytes()
                .all(|x| x.is_ascii_hexdigit() && !x.is_ascii_uppercase())
        {
            return Err("invalid previous generation".into());
        }
        let data = fs::read(config.archive.join("manifests").join(&generation))?;
        if format!("{:x}", Sha256::digest(&data)) != generation {
            return Err("previous manifest corrupt".into());
        }
        let previous: Manifest = serde_json::from_slice(&data)?;
        if previous.schema != 1
            || previous
                .collections
                .iter()
                .any(|x| x.account != account || x.origin != config.origin)
            || previous.capture_highwater > snapshot.capture_highwater
            || previous.event_highwater > snapshot.event_highwater
        {
            return Err("previous manifest belongs to another source".into());
        }
        (
            previous.objects,
            previous.events,
            previous.capture_highwater,
            previous.event_highwater,
        )
    } else {
        (Vec::new(), Vec::new(), 0, 0)
    };
    loop {
        let page = store.provider_captures_for(&account, &config.origin, capture, 16)?;
        if page.is_empty() {
            break;
        }
        for item in page {
            if item.sequence > snapshot.capture_highwater {
                return Err("source changed during export".into());
            }
            let path = config.archive.join("blobs").join(&item.digest);
            if !path.exists() {
                let size = item.object.raw.len() as u64;
                if size > budget.remaining
                    || fs2::available_space(&config.archive)?
                        < size.saturating_add(config.minimum_free_bytes)
                {
                    return Err("export staging capacity unavailable".into());
                }
                budget.remaining -= size;
            }
            let digest = isyncyou_exporter::put(&config.archive, &item.object.raw)?;
            if digest != item.digest {
                return Err("source digest mismatch".into());
            }
            let related_blobs = if item.scope.service == "mail" {
                isyncyou_exporter::mail_dependencies(&item.object.raw)?
            } else { Vec::new() };
            capture = item.sequence;
            objects.push(Object {
                account: item.scope.account_id,
                origin: item.scope.origin,
                service: item.scope.service,
                collection: item.scope.collection,
                id: item.object.remote_id,
                version: item.version_key,
                digest,
                size: item.object.raw.len() as u64,
                related_blobs,
            });
            if objects.len() > 200_000 {
                return Err("export manifest limit reached".into());
            }
        }
    }
    if capture != snapshot.capture_highwater {
        return Err("capture checkpoint incomplete".into());
    }
    loop {
        let page = store.provider_events_for(&account, &config.origin, after, 1000)?;
        if page.is_empty() {
            break;
        }
        for item in page {
            if item.sequence > snapshot.event_highwater {
                return Err("source changed during export".into());
            }
            after = item.sequence;
            events.push(Event {
                sequence: item.sequence,
                account: item.scope.account_id,
                origin: item.scope.origin,
                service: item.scope.service,
                collection: item.scope.collection,
                id: item.remote_id,
                version: item.version_key,
                cursor: item.cursor,
            });
            if events.len() > 400_000 {
                return Err("export event limit reached".into());
            }
        }
    }
    if after != snapshot.event_highwater
        || store.provider_export_snapshot(&account, &config.origin)? != snapshot
    {
        return Err("source checkpoint changed".into());
    }
    let manifest = Manifest {
        schema: 1,
        capture_highwater: capture,
        event_highwater: after,
        collections,
        objects,
        events,
    };
    let generation = isyncyou_exporter::publish(&config.archive, &manifest)?;
    println!("{{\"state\":\"captured_locally\",\"generation\":\"{generation}\"}}");
    Ok(())
}

fn main() {
    let result = (|| -> Result<()> {
        let args: Vec<_> = std::env::args_os().collect();
        if args.len() != 2 {
            return Err("expected one private config path".into());
        }
        let path = PathBuf::from(&args[1]);
        let metadata = fs::metadata(&path)?;
        if !path.is_absolute() || metadata.permissions().mode() & 0o077 != 0 {
            return Err("private config required".into());
        }
        run(serde_json::from_slice(&fs::read(path)?)?)
    })();
    if result.is_err() {
        // Native errors may contain provider URLs or object identities.
        eprintln!("export failed; inspect private diagnostics");
        std::process::exit(1);
    }
}
