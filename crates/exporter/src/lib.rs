//! Durable immutable export storage. Provider acquisition and PBS protection
//! are separate contracts; publishing here does not imply either is complete.

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
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
        let related_blobs = if capture.scope.service == "mail" {
            mail_dependencies(&capture.object.raw)?
        } else {
            Vec::new()
        };
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
                related_blobs,
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

/// Decode only the explicitly broker-declared representation after checking
/// the exact protected payload. A raw MIME message may itself start with the
/// legacy magic, so its bytes must never be used to infer the representation.
pub fn verified_recovery_mail<'a>(
    payload: &'a [u8],
    expected_size: u64,
    expected_sha256: &str,
    encoding: &str,
) -> io::Result<&'a [u8]> {
    if expected_size != payload.len() as u64
        || expected_size > 100 * 1024 * 1024
        || expected_sha256.len() != 64
        || !expected_sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        || digest(payload) != expected_sha256
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "recovery payload integrity failure",
        ));
    }
    let mime = match encoding {
        "mime" => payload,
        "isync-mime-1" => original_mail(payload)?.1,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown recovery encoding",
            ))
        }
    };
    if mime.len() > 64 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "recovered MIME too large",
        ));
    }
    Ok(mime)
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    pub digest: String,
    pub size: u64,
}

/// Return the immutable original MIME reference for a metadata version. Legacy
/// envelopes contain their own MIME and therefore have no related blob.
pub fn mail_dependencies(raw: &[u8]) -> io::Result<Vec<BlobRef>> {
    if raw.starts_with(b"ISYNC-MIME-1\n") {
        original_mail(raw)?;
        return Ok(Vec::new());
    }
    if raw.len() > 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mail metadata too large",
        ));
    }
    let record: serde_json::Value = serde_json::from_slice(raw).map_err(io::Error::other)?;
    if record.get("schema").and_then(serde_json::Value::as_str) != Some("isyncyou.mail-metadata.v1")
        || !record
            .get("native")
            .is_some_and(serde_json::Value::is_object)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid mail metadata",
        ));
    }
    let digest = record
        .get("mime_digest")
        .and_then(serde_json::Value::as_str)
        .filter(|x| {
            x.len() == 64
                && x.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid MIME digest"))?;
    let size = record
        .get("mime_size")
        .and_then(serde_json::Value::as_u64)
        .filter(|x| *x <= 64 * 1024 * 1024)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid MIME size"))?;
    Ok(vec![BlobRef {
        digest: digest.to_owned(),
        size,
    }])
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
    #[serde(default)]
    pub related_blobs: Vec<BlobRef>,
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

/// Rebase only trusted transport-origin DAV metadata to the enrolled public
/// identity. Provider payload blobs are never parsed or modified here.
pub fn canonicalize_manifest_identity(
    manifest: &mut Manifest,
    transport: &str,
    identity: &str,
) -> io::Result<()> {
    use url::Url;
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid capture identity origin",
        )
    };
    let transport = Url::parse(transport).map_err(|_| invalid())?;
    let identity = Url::parse(identity).map_err(|_| invalid())?;
    let ip = transport
        .host_str()
        .and_then(|s| s.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok())
        .ok_or_else(invalid)?;
    if transport.scheme() != "http"
        || !ip.is_loopback()
        || transport.path() != "/"
        || identity.scheme() != "https"
        || identity.host_str().is_none()
        || identity.path() != "/"
        || [&transport, &identity].iter().any(|u| {
            !u.username().is_empty()
                || u.password().is_some()
                || u.query().is_some()
                || u.fragment().is_some()
        })
    {
        return Err(invalid());
    }
    fn href(value: &str, transport: &Url, identity: &Url, collection: bool) -> io::Result<String> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid DAV capture metadata");
        let raw_lower = value.to_ascii_lowercase();
        if value.contains('\\')
            || raw_lower.contains("/../")
            || raw_lower.contains("/./")
            || ["%2f", "%5c", "%2e", "%25"]
                .iter()
                .any(|bad| raw_lower.contains(bad))
        {
            return Err(invalid());
        }
        let url = Url::parse(value).map_err(|_| invalid())?;
        let lower = url.path().to_ascii_lowercase();
        if (url.origin() != transport.origin() && url.origin() != identity.origin())
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.path().starts_with('/')
            || (collection && !url.path().ends_with('/'))
            || ["%2f", "%5c", "%2e", "%25"]
                .iter()
                .any(|bad| lower.contains(bad))
        {
            return Err(invalid());
        }
        let mapped = identity.join(url.path()).map_err(|_| invalid())?;
        if mapped.origin() != identity.origin() {
            return Err(invalid());
        }
        Ok(mapped.to_string())
    }
    fn fields(
        origin: &mut String,
        service: &str,
        collection: &mut String,
        id: Option<&mut String>,
        transport: &Url,
        identity: &Url,
    ) -> io::Result<()> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid capture scope");
        let declared = Url::parse(origin).map_err(|_| invalid())?;
        if (declared.origin() != transport.origin() && declared.origin() != identity.origin())
            || declared.path() != "/"
            || !declared.username().is_empty()
            || declared.password().is_some()
            || declared.query().is_some()
            || declared.fragment().is_some()
        {
            return Err(invalid());
        }
        if matches!(service, "calendar" | "contacts") {
            *collection = href(collection, transport, identity, true)?;
            if let Some(id) = id {
                let mapped = href(id, transport, identity, false)?;
                let c = Url::parse(collection).map_err(|_| invalid())?;
                let i = Url::parse(&mapped).map_err(|_| invalid())?;
                if !i.path().starts_with(c.path()) || i.path() == c.path() {
                    return Err(invalid());
                }
                *id = mapped;
            }
        } else if !matches!(service, "mail" | "mailbox") {
            return Err(invalid());
        }
        *origin = identity.origin().ascii_serialization();
        Ok(())
    }
    for c in &mut manifest.collections {
        fields(
            &mut c.origin,
            &c.service,
            &mut c.collection,
            None,
            &transport,
            &identity,
        )?;
    }
    for o in &mut manifest.objects {
        fields(
            &mut o.origin,
            &o.service,
            &mut o.collection,
            Some(&mut o.id),
            &transport,
            &identity,
        )?;
        if matches!(o.service.as_str(), "calendar" | "contacts" | "mail") {
            isyncyou_store::capture_identity(
                &o.account,
                &o.origin,
                &o.service,
                &o.collection,
                &o.id,
            )
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid shared capture identity",
                )
            })?;
        }
    }
    for e in &mut manifest.events {
        fields(
            &mut e.origin,
            &e.service,
            &mut e.collection,
            Some(&mut e.id),
            &transport,
            &identity,
        )?;
    }
    Ok(())
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
    if root
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "archive path is a symlink",
        ));
    }
    let blobs = root.join("blobs");
    if blobs
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "blob directory is a symlink",
        ));
    }
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
        if !fs::symlink_metadata(&path)?.is_file() || digest(&fs::read(&path)?) != hash {
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

#[derive(Serialize)]
struct DomainSummary {
    collections: u64,
    complete: u64,
    oldest_at: u64,
    objects: u64,
    object_bytes: u64,
}

// This optional projection is not part of the manifest or PBS protection ABI.
// A missing projection is unknown; publication must remain independent of it.
fn write_summary(
    root: &Path,
    generation: &str,
    manifest: &Manifest,
    baseline_generation: Option<&str>,
    previous: &HashSet<(String, u64)>,
) -> io::Result<()> {
    let mut domains: BTreeMap<&str, DomainSummary> = BTreeMap::new();
    for c in &manifest.collections {
        if !matches!(
            c.service.as_str(),
            "calendar" | "contacts" | "mail" | "mailbox"
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown summary domain",
            ));
        }
        let d = domains.entry(c.service.as_str()).or_insert(DomainSummary {
            collections: 0,
            complete: 0,
            oldest_at: c.observed_at,
            objects: 0,
            object_bytes: 0,
        });
        d.collections = d
            .collections
            .checked_add(1)
            .ok_or_else(|| io::Error::other("summary overflow"))?;
        d.complete = d
            .complete
            .checked_add(u64::from(c.complete))
            .ok_or_else(|| io::Error::other("summary overflow"))?;
        d.oldest_at = d.oldest_at.min(c.observed_at);
    }
    for object in &manifest.objects {
        let d = domains
            .get_mut(object.service.as_str())
            .ok_or_else(|| io::Error::other("summary domain missing"))?;
        d.objects = d
            .objects
            .checked_add(1)
            .ok_or_else(|| io::Error::other("summary overflow"))?;
        d.object_bytes = d
            .object_bytes
            .checked_add(object.size)
            .ok_or_else(|| io::Error::other("summary overflow"))?;
    }
    // These are unique referenced export blobs, not PBS transfer or disk bytes.
    let mut payloads = HashSet::new();
    for object in &manifest.objects {
        payloads.insert((object.digest.clone(), object.size));
        payloads.extend(
            object
                .related_blobs
                .iter()
                .map(|b| (b.digest.clone(), b.size)),
        );
    }
    let payload_bytes = payloads
        .iter()
        .try_fold(0_u64, |total, (_, size)| total.checked_add(*size))
        .ok_or_else(|| io::Error::other("summary overflow"))?;
    let changed_payload_bytes = match baseline_generation {
        Some(_) => Some(
            payloads
                .difference(previous)
                .try_fold(0_u64, |total, (_, size)| total.checked_add(*size))
                .ok_or_else(|| io::Error::other("summary overflow"))?,
        ),
        None => None,
    };
    let bytes = serde_json::to_vec(
        &serde_json::json!({"schema": 1, "generation": generation, "domains": domains,
                            "payload_bytes": payload_bytes, "baseline_generation": baseline_generation,
                            "changed_payload_bytes": changed_payload_bytes}),
    )
    .map_err(io::Error::other)?;
    if bytes.len() > 4096 {
        return Err(io::Error::other("summary too large"));
    }
    let dir = root.join("summaries");
    if dir
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        return Err(io::Error::other("summary directory symlink"));
    }
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    sync_dir(root)?;
    let path = dir.join(generation);
    // A generation can recur. Its original provenance must not be rewritten
    // relative to a later predecessor; an invalid existing sidecar is unknown.
    match fs::symlink_metadata(&path) {
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
        Err(e) => return Err(e),
    }
    let tmp = dir.join(".staging");
    match fs::remove_file(&tmp) {
        Ok(()) => (),
        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
        Err(e) => return Err(e),
    }
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(&bytes)?;
    f.sync_all()?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o400))?;
    f.sync_all()?;
    fs::rename(&tmp, &path)?;
    sync_dir(&dir)
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
    let mut baseline_generation = None;
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
        baseline_generation = Some(generation);
        for o in old.objects {
            previous.insert((o.digest, o.size));
            previous.extend(o.related_blobs.into_iter().map(|b| (b.digest, b.size)));
        }
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
        if object.service == "mail" {
            let bytes = fs::read(root.join("blobs").join(&object.digest))?;
            if mail_dependencies(&bytes)? != object.related_blobs {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "mail dependency mismatch",
                ));
            }
        }
        if object.related_blobs.len() > 64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "too many related blobs",
            ));
        }
        let refs = std::iter::once((&object.digest, object.size))
            .chain(object.related_blobs.iter().map(|b| (&b.digest, b.size)));
        for (hash, size) in refs {
            if hash.len() != 64
                || !hash
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid digest",
                ));
            }
            let path = root.join("blobs").join(hash);
            let metadata = fs::symlink_metadata(&path)?;
            if !metadata.is_file()
                || metadata.len() != size
                || (!previous.contains(&(hash.clone(), size)) && digest(&fs::read(path)?) != *hash)
            {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "blob mismatch"));
            }
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
    if manifests
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "manifest directory is a symlink",
        ));
    }
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
    // Observability is optional and cannot hold back a valid recovery generation.
    let _ = write_summary(
        root,
        &generation,
        manifest,
        baseline_generation.as_deref(),
        &previous,
    );
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
    fn loopback_capture_metadata_rebases_to_public_identity_without_touching_blob() {
        let local = "http://127.0.0.1:8080/";
        let public = "https://example.test/";
        let raw = b"BEGIN:VCALENDAR\r\nURL:http://127.0.0.1:8080/private\r\nEND:VCALENDAR\r\n";
        let dir = tempfile::tempdir().unwrap();
        let hash = put(dir.path(), raw).unwrap();
        let collection = "http://127.0.0.1:8080/dav/user%40example.test/calendar/";
        let href = "http://127.0.0.1:8080/dav/user%40example.test/calendar/item.ics";
        let mut manifest = Manifest {
            schema: 1,
            capture_highwater: 1,
            event_highwater: 1,
            collections: vec![Collection {
                account: "a".into(),
                origin: local.into(),
                service: "calendar".into(),
                collection: collection.into(),
                cursor: "cursor".into(),
                observed_at: 1,
                complete: true,
            }],
            objects: vec![Object {
                account: "a".into(),
                origin: local.into(),
                service: "calendar".into(),
                collection: collection.into(),
                id: href.into(),
                version: "v".into(),
                digest: hash.clone(),
                size: raw.len() as u64,
                related_blobs: vec![],
            }],
            events: vec![Event {
                sequence: 1,
                account: "a".into(),
                origin: local.into(),
                service: "calendar".into(),
                collection: collection.into(),
                id: href.into(),
                version: Some("v".into()),
                cursor: "cursor".into(),
            }],
        };
        canonicalize_manifest_identity(&mut manifest, local, public).unwrap();
        assert_eq!(manifest.objects[0].origin, "https://example.test");
        assert_eq!(
            manifest.objects[0].collection,
            "https://example.test/dav/user%40example.test/calendar/"
        );
        assert_eq!(
            manifest.objects[0].id,
            "https://example.test/dav/user%40example.test/calendar/item.ics"
        );
        assert_eq!(manifest.events[0].id, manifest.objects[0].id);
        let selected = &manifest.objects[0];
        assert_eq!(
            isyncyou_store::capture_identity(
                &selected.account,
                &selected.origin,
                &selected.service,
                &selected.collection,
                &selected.id
            )
            .unwrap(),
            "5e300bfd02f7dc22d17982f8614a1317650cb6416eaff0ce63c849a4882241ac"
        );
        assert_eq!(
            std::fs::read(dir.path().join("blobs").join(&hash)).unwrap(),
            raw
        );
        canonicalize_manifest_identity(&mut manifest, local, public).unwrap();
        manifest.objects[0].id = "https://evil.test/dav/calendar/item.ics".into();
        assert!(canonicalize_manifest_identity(&mut manifest, local, public).is_err());
        manifest.objects[0].id = "https://example.test/dav/calendar/%2Fescape".into();
        assert!(canonicalize_manifest_identity(&mut manifest, local, public).is_err());
    }

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
                service: "calendar".into(),
                collection: "inbox".into(),
                cursor: "c1".into(),
                observed_at: 100,
                complete: true,
            }],
            objects: vec![Object {
                account: "a".into(),
                origin: "https://example.test".into(),
                service: "calendar".into(),
                collection: "inbox".into(),
                id: "i".into(),
                version: "v".into(),
                digest: hash.clone(),
                size: 8,
                related_blobs: Vec::new(),
            }],
            events: vec![],
        };
        let old = publish(root, &manifest).unwrap();
        let summary: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("summaries").join(&old)).unwrap()).unwrap();
        assert_eq!(summary["generation"], old);
        assert_eq!(summary["payload_bytes"], 8);
        assert!(summary["baseline_generation"].is_null());
        assert!(summary["changed_payload_bytes"].is_null());
        assert_eq!(summary["domains"]["calendar"]["collections"], 1);
        assert_eq!(summary["domains"]["calendar"]["complete"], 1);
        assert_eq!(summary["domains"]["calendar"]["oldest_at"], 100);
        assert_eq!(summary["domains"]["calendar"]["objects"], 1);
        assert_eq!(summary["domains"]["calendar"]["object_bytes"], 8);
        assert!(!summary.to_string().contains("https://example.test"));
        // A broken optional sidecar must never block a valid generation.
        let other = tempfile::tempdir().unwrap();
        put(other.path(), b"original").unwrap();
        std::os::unix::fs::symlink("/nonexistent", other.path().join("summaries")).unwrap();
        let published = publish(other.path(), &manifest).unwrap();
        assert_eq!(
            fs::read_to_string(other.path().join("current")).unwrap(),
            published
        );
        assert!(!other.path().join("summaries").join(published).exists());
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

    #[test]
    fn recovery_mail_requires_integrity_and_explicit_encoding() {
        let mime = b"ISYNC-MIME-1\nthis is ordinary MIME data";
        let hash = digest(mime);
        assert_eq!(
            verified_recovery_mail(mime, mime.len() as u64, &hash, "mime").unwrap(),
            mime
        );
        assert!(verified_recovery_mail(mime, mime.len() as u64, &hash, "isync-mime-1").is_err());
        assert!(verified_recovery_mail(mime, mime.len() as u64, &hash, "unknown").is_err());
        assert!(verified_recovery_mail(mime, mime.len() as u64, &"f".repeat(64), "mime").is_err());
        let mut envelope = b"ISYNC-MIME-1\n".to_vec();
        envelope.extend_from_slice(&2u32.to_be_bytes());
        envelope.extend_from_slice(b"{}");
        envelope.extend_from_slice(mime);
        assert_eq!(
            verified_recovery_mail(
                &envelope,
                envelope.len() as u64,
                &digest(&envelope),
                "isync-mime-1"
            )
            .unwrap(),
            mime
        );
    }

    #[test]
    fn metadata_versions_share_one_immutable_original_and_missing_dependency_blocks_publish() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mime = put(root, b"same original MIME").unwrap();
        let a_data = serde_json::to_vec(&serde_json::json!({"schema":"isyncyou.mail-metadata.v1","native":{"flags":[]},"mime_digest":mime,"mime_size":18})).unwrap();
        let b_data = serde_json::to_vec(&serde_json::json!({"schema":"isyncyou.mail-metadata.v1","native":{"flags":["seen"]},"mime_digest":mime,"mime_size":18})).unwrap();
        let a = put(root, &a_data).unwrap();
        let b = put(root, &b_data).unwrap();
        let make = |digest: String, size: u64, version: &str| Object {
            account: "a".into(),
            origin: "http://127.0.0.1/".into(),
            service: "mail".into(),
            collection: "a".into(),
            id: "one".into(),
            version: version.into(),
            digest,
            size,
            related_blobs: vec![BlobRef {
                digest: mime.clone(),
                size: 18,
            }],
        };
        let mut manifest = Manifest {
            schema: 1,
            capture_highwater: 1,
            event_highwater: 0,
            collections: vec![Collection {
                account: "a".into(),
                origin: "http://127.0.0.1/".into(),
                service: "mail".into(),
                collection: "a".into(),
                cursor: "state".into(),
                observed_at: 100,
                complete: true,
            }],
            objects: vec![make(a, a_data.len() as u64, "one")],
            events: vec![],
        };
        let baseline = publish(root, &manifest).unwrap();
        let original_summary = fs::read(root.join("summaries").join(&baseline)).unwrap();
        manifest.objects.push(make(b, b_data.len() as u64, "two"));
        let current = publish(root, &manifest).unwrap();
        let summary: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("summaries").join(current)).unwrap())
                .unwrap();
        assert_eq!(summary["baseline_generation"], baseline);
        assert_eq!(summary["changed_payload_bytes"], b_data.len() as u64);
        assert_eq!(
            summary["payload_bytes"],
            (a_data.len() + b_data.len()) as u64 + 18
        );
        let removed = manifest.objects.pop().unwrap();
        assert_eq!(publish(root, &manifest).unwrap(), baseline);
        assert_eq!(
            fs::read(root.join("summaries").join(&baseline)).unwrap(),
            original_summary
        );
        manifest.objects.push(removed);
        manifest.objects[1].related_blobs[0].digest = "f".repeat(64);
        assert!(publish(root, &manifest).is_err());
    }
}
