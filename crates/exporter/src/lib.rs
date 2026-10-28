//! Durable immutable export storage. Provider acquisition and PBS protection
//! are separate contracts; publishing here does not imply either is complete.

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Copy one bounded page of the shared provider's transactional capture outbox.
/// Returning a page is not a PBS acknowledgement or a complete generation.
/// Persist its returned high-water mark only with the corresponding durable
/// manifest; otherwise replay the page after a crash.
pub fn capture_page(
    root: &Path,
    store: &isyncyou_store::Store,
    after: i64,
    limit: usize,
) -> Result<Vec<(i64, Object)>, Box<dyn std::error::Error>> {
    let mut result = Vec::new();
    for capture in store.provider_captures(after, limit)? {
        let hash = put(root, &capture.object.raw)?;
        if hash != capture.digest {
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "outbox digest mismatch").into(),
            );
        }
        result.push((
            capture.sequence,
            Object {
                account: capture.scope.account_id,
                origin: capture.scope.origin,
                service: capture.scope.service,
                collection: capture.scope.collection,
                id: capture.object.remote_id,
                version: capture.version_key,
                digest: hash,
                size: capture.object.raw.len() as u64,
            },
        ));
    }
    Ok(result)
}

/// Read one bounded ordered page of membership changes. A removal has no
/// version key. The caller must bind these to the same consistent source view
/// as capture_page before declaring a complete generation.
pub fn event_page(
    store: &isyncyou_store::Store,
    after: i64,
    limit: usize,
) -> isyncyou_store::Result<Vec<Event>> {
    Ok(store
        .provider_events(after, limit)?
        .into_iter()
        .map(|event| Event {
            sequence: event.sequence,
            account: event.scope.account_id,
            origin: event.scope.origin,
            service: event.scope.service,
            collection: event.scope.collection,
            id: event.remote_id,
            version: event.version_key,
            cursor: event.cursor,
        })
        .collect())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub sequence: i64,
    pub account: String,
    pub origin: String,
    pub service: String,
    pub collection: String,
    pub id: String,
    pub version: Option<String>,
    pub cursor: String,
}

/// Decode the preserved JMAP metadata and exact original MIME from an envelope.
/// This does not submit or import the message.
pub fn original_mail(envelope: &[u8]) -> io::Result<(serde_json::Value, &[u8])> {
    const MAGIC: &[u8] = b"ISYNC-MIME-1\n";
    if !envelope.starts_with(MAGIC) || envelope.len() < MAGIC.len() + 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid mail envelope",
        ));
    }
    let start = MAGIC.len() + 4;
    let size = u32::from_be_bytes(
        envelope[MAGIC.len()..start]
            .try_into()
            .map_err(io::Error::other)?,
    ) as usize;
    let end = start
        .checked_add(size)
        .filter(|end| *end <= envelope.len())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid mail metadata length")
        })?;
    let metadata: serde_json::Value =
        serde_json::from_slice(&envelope[start..end]).map_err(io::Error::other)?;
    if !metadata.is_object() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid mail metadata",
        ));
    }
    Ok((metadata, &envelope[end..]))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Collection {
    pub account: String,
    pub origin: String,
    pub service: String,
    pub collection: String,
    pub cursor: String,
    pub observed_at: u64,
    pub complete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Object {
    pub account: String,
    pub origin: String,
    pub service: String,
    pub collection: String,
    pub id: String,
    pub version: String,
    pub digest: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    /// Highest capture sequence represented in this manifest, not PBS protection.
    pub capture_highwater: i64,
    /// Highest provider membership event reflected in this manifest.
    pub event_highwater: i64,
    pub collections: Vec<Collection>,
    pub objects: Vec<Object>,
    pub events: Vec<Event>,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// Store immutable bytes. Callers must enforce a configured space quota before
/// supplying data; this function never removes older or unprotected objects.
pub fn put(root: &Path, bytes: &[u8]) -> io::Result<String> {
    let blobs = root.join("blobs");
    fs::create_dir_all(&blobs)?;
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    fs::set_permissions(&blobs, fs::Permissions::from_mode(0o700))?;
    sync_dir(root)?;
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("publish.lock"))?;
    lock.lock_exclusive()?;
    let hash = digest(bytes);
    let path = blobs.join(&hash);
    if path.exists() {
        if digest(&fs::read(&path)?) != hash {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "existing blob corrupt",
            ));
        }
    } else {
        let tmp = blobs.join(".staging");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(tmp, &path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400))?;
        file.sync_all()?;
        sync_dir(&blobs)?;
    }
    Ok(hash)
}

/// Publish a full manifest atomically. Incomplete collection observations cannot
/// become the current generation. The generation ID is the manifest digest.
pub fn publish(root: &Path, manifest: &Manifest) -> io::Result<String> {
    if manifest.schema != 1
        || manifest.capture_highwater < 0
        || manifest.event_highwater < 0
        || manifest.collections.is_empty()
        || manifest.collections.iter().any(|c| {
            !c.complete
                || c.observed_at == 0
                || c.account.is_empty()
                || c.origin.is_empty()
                || c.service.is_empty()
                || c.collection.is_empty()
                || c.cursor.is_empty()
        })
        || manifest.objects.iter().any(|o| {
            o.id.is_empty()
                || o.version.is_empty()
                || !manifest.collections.iter().any(|c| {
                    c.account == o.account
                        && c.origin == o.origin
                        && c.service == o.service
                        && c.collection == o.collection
                })
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "incomplete generation",
        ));
    }
    fs::create_dir_all(root)?;
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("publish.lock"))?;
    lock.lock_exclusive()?;
    let current = root.join("current");
    let mut previous = HashSet::new();
    if current.exists() {
        let generation = fs::read_to_string(current)?;
        if generation.len() != 64
            || !generation
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid previous generation",
            ));
        }
        let bytes = fs::read(root.join("manifests").join(&generation))?;
        if digest(&bytes) != generation {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid previous manifest",
            ));
        }
        let old: Manifest = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        previous.extend(old.objects.into_iter().map(|o| (o.digest, o.size)));
    }
    for object in &manifest.objects {
        if object.digest.len() != 64
            || !object
                .digest
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid digest",
            ));
        }
        let path = root.join("blobs").join(&object.digest);
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file()
            || metadata.len() != object.size
            || (!previous.contains(&(object.digest.clone(), object.size))
                && digest(&fs::read(path)?) != object.digest)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "blob mismatch"));
        }
    }
    let data = serde_json::to_vec(manifest).map_err(io::Error::other)?;
    if data.len() > 64 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "manifest too large",
        ));
    }
    let generation = digest(&data);
    let manifests = root.join("manifests");
    fs::create_dir_all(&manifests)?;
    fs::set_permissions(&manifests, fs::Permissions::from_mode(0o700))?;
    sync_dir(root)?;
    let manifest_path = manifests.join(&generation);
    if manifest_path.exists() {
        if fs::read(&manifest_path)? != data {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "manifest mismatch",
            ));
        }
    } else {
        let tmp = manifests.join(".staging");
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&data)?;
        f.sync_all()?;
        fs::rename(tmp, &manifest_path)?;
        fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o400))?;
        f.sync_all()?;
        sync_dir(&manifests)?;
    }
    let tmp = root.join("current.tmp");
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(generation.as_bytes())?;
    f.sync_all()?;
    fs::rename(tmp, root.join("current"))?;
    sync_dir(root)?;
    Ok(generation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_outbox_versions_are_copied_without_altering_originals() {
        use isyncyou_store::{ProviderChange, ProviderObject, ProviderScope, Store};
        let store = Store::open_in_memory().unwrap();
        let scope = ProviderScope {
            account_id: "a".into(),
            origin: "https://example.test".into(),
            service: "caldav".into(),
            collection: "c".into(),
        };
        for (version, raw) in [("1", b"first".as_slice()), ("2", b"second".as_slice())] {
            store
                .apply_provider_page(
                    &scope,
                    &[ProviderChange::Upsert(ProviderObject {
                        remote_id: "event".into(),
                        etag: Some(version.into()),
                        raw: raw.to_vec(),
                    })],
                    version,
                )
                .unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let first = capture_page(dir.path(), &store, 0, 1).unwrap();
        assert_eq!(first.len(), 1);
        let second = capture_page(dir.path(), &store, first[0].0, 10).unwrap();
        assert!(!second.is_empty());
        assert_eq!(
            fs::read(dir.path().join("blobs").join(&first[0].1.digest)).unwrap(),
            b"first"
        );
        assert_eq!(
            fs::read(
                dir.path()
                    .join("blobs")
                    .join(&second.last().unwrap().1.digest)
            )
            .unwrap(),
            b"second"
        );
        assert_eq!(
            store.provider_object(&scope, "event").unwrap().unwrap().raw,
            b"second"
        );
        store
            .apply_provider_page(&scope, &[ProviderChange::Destroyed("event".into())], "3")
            .unwrap();
        let events = event_page(&store, 0, 10).unwrap();
        assert_eq!(events.len(), 3);
        assert!(events[0].version.is_some());
        assert!(events[2].version.is_none());
    }

    #[test]
    fn incomplete_and_corrupt_generations_never_replace_current() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let hash = put(root, b"original").unwrap();
        let mut manifest = Manifest {
            schema: 1,
            capture_highwater: 0,
            event_highwater: 0,
            collections: vec![Collection {
                account: "a".into(),
                origin: "https://example.test".into(),
                service: "mail".into(),
                collection: "inbox".into(),
                cursor: "c1".into(),
                observed_at: 100,
                complete: true,
            }],
            objects: vec![Object {
                account: "a".into(),
                origin: "https://example.test".into(),
                service: "mail".into(),
                collection: "inbox".into(),
                id: "i".into(),
                version: "v".into(),
                digest: hash.clone(),
                size: 8,
            }],
            events: vec![],
        };
        let old = publish(root, &manifest).unwrap();
        manifest.collections[0].complete = false;
        assert!(publish(root, &manifest).is_err());
        assert_eq!(fs::read_to_string(root.join("current")).unwrap(), old);
        manifest.collections[0].complete = true;
        fs::set_permissions(
            root.join("blobs").join(&hash),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        fs::write(root.join("blobs").join(hash), b"corrupt").unwrap();
        assert!(publish(root, &manifest).is_err());
        assert_eq!(fs::read_to_string(root.join("current")).unwrap(), old);
    }

    #[test]
    fn mail_envelope_returns_exact_mime_and_rejects_truncation() {
        let metadata = br#"{"id":"one","mailboxIds":{"inbox":true}}"#;
        let mime = b"Subject: original\r\n\r\nbody\x00";
        let mut data = b"ISYNC-MIME-1\n".to_vec();
        data.extend_from_slice(&(metadata.len() as u32).to_be_bytes());
        data.extend_from_slice(metadata);
        data.extend_from_slice(mime);
        let (native, original) = original_mail(&data).unwrap();
        assert_eq!(native["id"], "one");
        assert_eq!(original, mime);
        assert!(original_mail(b"ISYNC-MIME-1\n\xff\xff\xff\xff").is_err());
    }
}
