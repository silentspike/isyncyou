//! Headless capture on the server. Credentials come from systemd credential files.
mod mail;

use fs2::FileExt;
use isyncyou_connectors::{dav::LoopbackDavReader, jmap::LoopbackJmapReader};
use isyncyou_exporter::{Collection, Event, Manifest, Object};
use isyncyou_store::{ProviderProtectedCapture, ProviderScope, Store};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::collections::HashSet;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::FromRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Instant;
use url::Url;

type Result<T> = std::result::Result<T, Box<dyn Error>>;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    origin: String,
    trusted_public_origin: String,
    identity_origin: String,
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

fn run(config: Config, stage: &Cell<&'static str>) -> Result<()> {
    let started = Instant::now();
    stage.set("configuration");
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
    let identity = Url::parse(&config.identity_origin)?;
    let public = Url::parse(&config.trusted_public_origin)?;
    if identity.scheme() != "https"
        || identity.host_str().is_none()
        || identity.path() != "/"
        || !identity.username().is_empty()
        || identity.password().is_some()
        || identity.query().is_some()
        || identity.fragment().is_some()
        || identity != public
    {
        return Err("invalid enrolled identity origin".into());
    }
    if config
        .archive
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
    {
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
        stage.set("staging-capacity");
        return Err("export staging limit reached".into());
    }
    stage.set("credentials");
    let token = String::from_utf8(credential("provider-token")?)?
        .trim()
        .to_owned();
    if token.is_empty() || token.chars().any(char::is_whitespace) {
        return Err("invalid provider token".into());
    }
    let key = credential("store-key")?;
    stage.set("store");
    let store = Store::open_encrypted(&config.store, &key)?;
    // The dedicated collector lock excludes other writers. Preserve captured
    // staged versions before releasing an interrupted source enumeration.
    store.recover_abandoned_provider_staging()?;
    stage.set("jmap-discovery");
    let jmap =
        LoopbackJmapReader::discover(&config.origin, token.clone(), &config.trusted_public_origin)
            .inspect_err(|error| {
                use isyncyou_connectors::jmap::JmapError;
                stage.set(match error {
                    JmapError::InvalidUrl => "jmap-invalid-url",
                    JmapError::InvalidResponse => "jmap-invalid-response",
                    JmapError::Unsupported => "jmap-unsupported",
                    JmapError::Http(_) => "jmap-http",
                    JmapError::Transport => "jmap-transport",
                    _ => "jmap-other",
                });
            })?;
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
    stage.set("mail-capture");
    mail::sync(&jmap, &store, &mail_scope, &mut budget)?;
    stage.set("mailbox-capture");
    mail::sync_mailboxes(&jmap, &store, &mailbox_scope)?;
    stage.set("dav-discovery");
    let dav = LoopbackDavReader::new_with_public_origin(
        &config.origin,
        token,
        &config.trusted_public_origin,
    )?;
    let collections = dav.discover(&config.dav_entry)?;
    if !collections.iter().any(|x| x.calendar) || !collections.iter().any(|x| x.addressbook) {
        return Err("required DAV collections unavailable".into());
    }
    let mut expected = vec![mail_scope, mailbox_scope];
    for collection in collections {
        // Historical ownership follows native DAV owner/principal evidence,
        // never mere read or shared-calendar access.
        match collection.owned_by_current_principal {
            Some(true) => {}
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
                stage.set("staging-capacity");
                return Err("DAV staging capacity unavailable".into());
            }
            budget.remaining -= 256 * 1024 * 1024;
            stage.set("dav-capture");
            dav.sync_collection(&store, &scope)?;
            expected.push(scope);
        }
    }
    if !expected.iter().any(|scope| scope.service == "calendar")
        || !expected.iter().any(|scope| scope.service == "contacts")
    {
        return Err("required owned DAV collections unavailable".into());
    }
    stage.set("source-completeness");
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
    stage.set("generation-assembly");
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
            || previous.collections.iter().any(|x| {
                x.account != account
                    || (x.origin != config.origin
                        && x.origin != identity.origin().ascii_serialization())
            })
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
                    stage.set("staging-capacity");
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
            } else {
                Vec::new()
            };
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
    let mut manifest = Manifest {
        schema: 1,
        capture_highwater: capture,
        event_highwater: after,
        collections,
        objects,
        events,
    };
    stage.set("identity-canonicalization");
    isyncyou_exporter::canonicalize_manifest_identity(
        &mut manifest,
        &config.origin,
        &config.identity_origin,
    )?;
    stage.set("generation-publication");
    let generation = isyncyou_exporter::publish_started(&config.archive, &manifest, started)?;
    println!("{{\"state\":\"captured_locally\",\"generation\":\"{generation}\"}}");
    Ok(())
}

fn release_verified(config: Config, generation: String, key: &[u8]) -> Result<()> {
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(config.archive.join("collector.lock"))?;
    lock.try_lock_exclusive()?;
    if fs::read_to_string(config.archive.join("current"))? != generation {
        return Err("protected generation is not current".into());
    }
    let data = fs::read(config.archive.join("manifests").join(&generation))?;
    if data.len() > 64 * 1024 * 1024 || format!("{:x}", Sha256::digest(&data)) != generation {
        return Err("protected manifest mismatch".into());
    }
    let mut manifest: Manifest = serde_json::from_slice(&data)?;
    if manifest.schema != 1
        || manifest.collections.is_empty()
        || manifest.collections.iter().any(|c| !c.complete)
        || manifest.objects.len() > 200_000
    {
        return Err("incomplete protected manifest".into());
    }
    let account = manifest.collections[0].account.clone();
    if manifest.collections.iter().any(|c| c.account != account)
        || manifest.objects.iter().any(|o| o.account != account)
    {
        return Err("ambiguous protected account".into());
    }
    let protected: HashSet<_> = manifest
        .objects
        .iter()
        .map(|o| {
            (
                o.account.clone(),
                o.origin.clone(),
                o.service.clone(),
                o.collection.clone(),
                o.id.clone(),
                o.version.clone(),
                o.digest.clone(),
            )
        })
        .collect();
    let store = Store::open_encrypted(&config.store, key)?;
    let mut after = 0;
    let mut candidates = Vec::new();
    loop {
        let page = store.provider_captures_for(&account, &config.origin, after, 1000)?;
        if page.is_empty() {
            break;
        }
        for capture in page {
            after = capture.sequence;
            if after > manifest.capture_highwater {
                break;
            }
            if format!("{:x}", Sha256::digest(&capture.object.raw)) != capture.digest {
                return Err("capture digest mismatch".into());
            }
            let mut canonical = Manifest {
                schema: 1,
                capture_highwater: 0,
                event_highwater: 0,
                collections: Vec::new(),
                events: Vec::new(),
                objects: vec![Object {
                    account: capture.scope.account_id.clone(),
                    origin: capture.scope.origin.clone(),
                    service: capture.scope.service.clone(),
                    collection: capture.scope.collection.clone(),
                    id: capture.object.remote_id.clone(),
                    version: capture.version_key.clone(),
                    digest: capture.digest.clone(),
                    size: capture.object.raw.len() as u64,
                    related_blobs: Vec::new(),
                }],
            };
            isyncyou_exporter::canonicalize_manifest_identity(
                &mut canonical,
                &config.origin,
                &config.identity_origin,
            )?;
            let o = &canonical.objects[0];
            if protected.contains(&(
                o.account.clone(),
                o.origin.clone(),
                o.service.clone(),
                o.collection.clone(),
                o.id.clone(),
                o.version.clone(),
                o.digest.clone(),
            )) {
                candidates.push(ProviderProtectedCapture {
                    scope: capture.scope,
                    remote_id: capture.object.remote_id,
                    version_key: capture.version_key,
                    digest: capture.digest,
                });
            }
        }
        if after >= manifest.capture_highwater {
            break;
        }
    }
    let released = store.release_protected_captures(
        &account,
        &config.origin,
        manifest.capture_highwater,
        &candidates,
    )?;
    let count = released.len();
    let mut compacted = HashSet::new();
    for item in released {
        let mut canonical = Manifest {
            schema: 1,
            capture_highwater: 0,
            event_highwater: 0,
            collections: Vec::new(),
            events: Vec::new(),
            objects: vec![Object {
                account: item.scope.account_id,
                origin: item.scope.origin,
                service: item.scope.service,
                collection: item.scope.collection,
                id: item.remote_id,
                version: item.version_key,
                digest: item.digest,
                size: 0,
                related_blobs: Vec::new(),
            }],
        };
        isyncyou_exporter::canonicalize_manifest_identity(
            &mut canonical,
            &config.origin,
            &config.identity_origin,
        )?;
        let o = &canonical.objects[0];
        compacted.insert((
            o.account.clone(),
            o.origin.clone(),
            o.service.clone(),
            o.collection.clone(),
            o.id.clone(),
            o.version.clone(),
            o.digest.clone(),
        ));
    }
    if !compacted.is_empty() {
        manifest.objects.retain(|o| {
            !compacted.contains(&(
                o.account.clone(),
                o.origin.clone(),
                o.service.clone(),
                o.collection.clone(),
                o.id.clone(),
                o.version.clone(),
                o.digest.clone(),
            ))
        });
        // Keep the latest membership event for every object. Only events for
        // versions just proven protected and superseded may be omitted.
        let mut latest = std::collections::HashMap::new();
        for e in &manifest.events {
            let key = (
                e.account.clone(),
                e.origin.clone(),
                e.service.clone(),
                e.collection.clone(),
                e.id.clone(),
            );
            latest
                .entry(key)
                .and_modify(|v: &mut i64| *v = (*v).max(e.sequence))
                .or_insert(e.sequence);
        }
        let compacted_versions: HashSet<_> = compacted
            .iter()
            .map(|o| {
                (
                    o.0.clone(),
                    o.1.clone(),
                    o.2.clone(),
                    o.3.clone(),
                    o.4.clone(),
                    o.5.clone(),
                )
            })
            .collect();
        manifest.events.retain(|e| {
            let key = (
                e.account.clone(),
                e.origin.clone(),
                e.service.clone(),
                e.collection.clone(),
                e.id.clone(),
            );
            if latest.get(&key) == Some(&e.sequence) {
                return true;
            }
            !e.version.as_ref().is_some_and(|version| {
                compacted_versions.contains(&(
                    e.account.clone(),
                    e.origin.clone(),
                    e.service.clone(),
                    e.collection.clone(),
                    e.id.clone(),
                    version.clone(),
                ))
            })
        });
        isyncyou_exporter::publish_with_external_lock(&config.archive, &manifest, Instant::now())?;
    }
    println!("{{\"state\":\"released\",\"count\":{count}}}");
    Ok(())
}

fn main() {
    let stage = Cell::new("configuration");
    let result = (|| -> Result<()> {
        let args: Vec<_> = std::env::args_os().collect();
        if args.len() == 7 && args[1] == "--release-protected-fds" {
            return release_inherited(&args[2..]);
        }
        if args.len() != 2 {
            return Err("expected one private config path".into());
        }
        let path = PathBuf::from(&args[1]);
        let metadata = fs::symlink_metadata(&path)?;
        let mode = metadata.permissions().mode() & 0o777;
        let systemd_credential = std::env::var_os("CREDENTIALS_DIRECTORY")
            .map(PathBuf::from)
            .filter(|directory| directory.is_absolute())
            .is_some_and(|directory| path == directory.join("exporter-config"));
        let private_file = mode & 0o077 == 0;
        let systemd_file = systemd_credential && metadata.uid() == 0 && mode == 0o440;
        if !path.is_absolute() || !metadata.file_type().is_file() || !(private_file || systemd_file)
        {
            return Err("private config required".into());
        }
        run(serde_json::from_slice(&fs::read(path)?)?, &stage)
    })();
    if result.is_err() {
        // Native errors may contain provider URLs or object identities.
        eprintln!("export failed at {}", stage.get());
        std::process::exit(1);
    }
}

// The authenticated fixed gateway opens the credentials and holds both flock
// guards before spawning this process. Inherited open file descriptions retain
// those guards if the gateway or SSH connection disappears.
fn release_inherited(args: &[std::ffi::OsString]) -> Result<()> {
    if args.len() != 5 {
        return Err("invalid release descriptors".into());
    }
    let numbers: Vec<i32> = args
        .iter()
        .map(|arg| {
            arg.to_str()
                .ok_or("invalid descriptor")?
                .parse::<i32>()
                .map_err(|_| "invalid descriptor")
        })
        .collect::<std::result::Result<_, _>>()?;
    if numbers.iter().any(|n| *n < 3)
        || numbers.iter().collect::<HashSet<_>>().len() != numbers.len()
    {
        return Err("invalid release descriptors".into());
    }
    // SAFETY: each distinct descriptor was inherited by this process; each is
    // owned by exactly one File and remains open through the complete release.
    let mut files: Vec<File> = numbers
        .into_iter()
        .map(|fd| unsafe { File::from_raw_fd(fd) })
        .collect();
    for file in &files[..3] {
        let m = file.metadata()?;
        if !m.is_file() || m.uid() != 0 || m.permissions().mode() & 0o077 != 0 || m.len() > 65536 {
            return Err("unsafe inherited credential".into());
        }
    }
    for file in &files[3..] {
        if !file.metadata()?.is_file() {
            return Err("invalid inherited guard".into());
        }
        file.try_lock_exclusive()?;
    }
    let mut config = Vec::new();
    let mut key = Vec::new();
    let mut authority = Vec::new();
    files[0].read_to_end(&mut config)?;
    files[1].read_to_end(&mut key)?;
    files[2].read_to_end(&mut authority)?;
    if key.is_empty() || key.len() > 8192 {
        return Err("invalid inherited key".into());
    }
    let generation = String::from_utf8(authority)?;
    if generation.len() != 64
        || !generation
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invalid protected generation".into());
    }
    release_verified(serde_json::from_slice(&config)?, generation, &key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_release_compacts_new_manifest_and_retains_original() {
        use isyncyou_store::{ProviderChange, ProviderObject};
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("archive");
        fs::create_dir(&archive).unwrap();
        let db = dir.path().join("store.db");
        let key = b"release-test-key";
        let store = Store::open_encrypted(&db, key).unwrap();
        let scope = ProviderScope {
            account_id: "a".into(),
            origin: "http://127.0.0.1:18080/".into(),
            service: "mailbox".into(),
            collection: "inbox".into(),
        };
        for value in [b"old".as_slice(), b"new".as_slice()] {
            let object = ProviderObject {
                remote_id: "id".into(),
                etag: Some(String::from_utf8_lossy(value).into()),
                raw: value.to_vec(),
            };
            store
                .apply_provider_bundle(&[(&scope, &[ProviderChange::Upsert(object)], "cursor")])
                .unwrap();
        }
        let snapshot = store.provider_export_snapshot("a", &scope.origin).unwrap();
        let captures = store
            .provider_captures_for("a", &scope.origin, 0, 10)
            .unwrap();
        let mut objects = Vec::new();
        for capture in &captures {
            let hash = isyncyou_exporter::put(&archive, &capture.object.raw).unwrap();
            objects.push(Object {
                account: "a".into(),
                origin: scope.origin.clone(),
                service: "mailbox".into(),
                collection: "inbox".into(),
                id: "id".into(),
                version: capture.version_key.clone(),
                digest: hash,
                size: capture.object.raw.len() as u64,
                related_blobs: Vec::new(),
            });
        }
        let events = store
            .provider_events_for("a", &scope.origin, 0, 10)
            .unwrap()
            .into_iter()
            .map(|e| Event {
                sequence: e.sequence,
                account: "a".into(),
                origin: scope.origin.clone(),
                service: "mailbox".into(),
                collection: "inbox".into(),
                id: "id".into(),
                version: e.version_key,
                cursor: e.cursor,
            })
            .collect();
        let mut manifest = Manifest {
            schema: 1,
            capture_highwater: snapshot.capture_highwater,
            event_highwater: snapshot.event_highwater,
            collections: vec![Collection {
                account: "a".into(),
                origin: scope.origin.clone(),
                service: "mailbox".into(),
                collection: "inbox".into(),
                cursor: "cursor".into(),
                observed_at: 1,
                complete: true,
            }],
            objects,
            events,
        };
        isyncyou_exporter::canonicalize_manifest_identity(
            &mut manifest,
            &scope.origin,
            "https://example.test/",
        )
        .unwrap();
        let generation = isyncyou_exporter::publish(&archive, &manifest).unwrap();
        let original = fs::read(archive.join("manifests").join(&generation)).unwrap();
        drop(store);
        let config = Config {
            origin: scope.origin.clone(),
            trusted_public_origin: "https://example.test/".into(),
            identity_origin: "https://example.test/".into(),
            dav_entry: "/".into(),
            store: db.clone(),
            archive: archive.clone(),
            max_staging_bytes: 100000000,
            minimum_free_bytes: 268435456,
        };
        release_verified(config, generation.clone(), key).unwrap();
        assert_eq!(
            fs::read(archive.join("manifests").join(&generation)).unwrap(),
            original
        );
        let current = fs::read_to_string(archive.join("current")).unwrap();
        assert_ne!(current, generation);
        let compact: Manifest =
            serde_json::from_slice(&fs::read(archive.join("manifests").join(current)).unwrap())
                .unwrap();
        assert_eq!(compact.objects.len(), 1);
        assert_eq!(compact.events.len(), 1);
        assert_eq!(compact.capture_highwater, snapshot.capture_highwater);
        let store = Store::open_encrypted(&db, key).unwrap();
        assert_eq!(
            store
                .provider_captures_for("a", &scope.origin, 0, 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .provider_export_snapshot("a", &scope.origin)
                .unwrap()
                .capture_highwater,
            snapshot.capture_highwater
        );
    }

    #[test]
    fn wp16_review_gap_staging_pressure_is_explicit_and_preserves_retained_data() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("archive");
        fs::create_dir(&archive).unwrap();
        let retained = archive.join("retained");
        fs::write(&retained, b"retained version").unwrap();
        let stage = Cell::new("initial");
        let result = run(
            Config {
                origin: "http://127.0.0.1:18080/".into(),
                trusted_public_origin: "https://example.test/".into(),
                identity_origin: "https://example.test/".into(),
                dav_entry: "/.well-known/caldav".into(),
                store: dir.path().join("store.db"),
                archive,
                max_staging_bytes: 1,
                minimum_free_bytes: 256 * 1024 * 1024,
            },
            &stage,
        );
        assert!(result.is_err());
        assert_eq!(stage.get(), "staging-capacity");
        assert_eq!(fs::read(retained).unwrap(), b"retained version");
    }
}
