//! At-rest **body-file envelope** encryption (#onedrive-mobile 0B).
//!
//! SQLCipher encrypts the store DB, but **not** the OneDrive file bodies materialised in
//! `sync_root`/`cache_root` — potentially large, sensitive files. Those get their own
//! authenticated envelope: chunked **AES-256-GCM** (via `ring`, which already cross-compiles
//! for Android arm64, unlike SQLCipher's vendored OpenSSL).
//!
//! Format (little SQLCipher, self-describing, versioned so a future scheme can coexist):
//! ```text
//! header (32 bytes):
//!   magic       [u8;4]  = b"ISYE"
//!   version     u8      = 1
//!   reserved    [u8;3]  = 0
//!   key_id      u32 BE          — which key sealed this (key rotation)
//!   chunk_size  u32 BE          — plaintext bytes per chunk
//!   plaintext_len u64 BE
//!   nonce_base  [u8;8]          — random per blob
//! body: ceil(plaintext_len / chunk_size) chunks; chunk i:
//!   nonce = nonce_base(8) || i(u32 BE)      (12 bytes, unique per chunk)
//!   aad   = header (binds version/key_id/size/nonce_base — a chunk can't be
//!           relocated to another blob or reordered)
//!   ciphertext = AES-256-GCM(plaintext_chunk) || tag(16)
//! ```
//! Rotation: re-seal with the new `key_id`; a reader picks the key by the header's id.

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// A 256-bit body key (from the Android Keystore on mobile; a test/derived key elsewhere).
pub type BodyKey = [u8; 32];

/// The envelope format version stamped into each blob (matches the item schema's
/// `encrypted_blob_version`).
pub const BODY_ENVELOPE_VERSION: u8 = 1;

const MAGIC: &[u8; 4] = b"ISYE";
const HEADER_LEN: usize = 32;
const TAG_LEN: usize = 16;
/// Default plaintext chunk size (64 KiB) — bounds per-chunk memory for large files.
pub const DEFAULT_CHUNK: u32 = 64 * 1024;

static REQUIRE_BODY_ENVELOPE: AtomicBool = AtomicBool::new(false);

/// Require all OneDrive body reads in this process to use a valid sealed envelope.
///
/// Desktop keeps plaintext pass-through compatibility. Mobile calls this only after
/// the Android Keystore-derived body key has been installed, so WebUI/engine readers
/// cannot accidentally treat a legacy plaintext file as a valid local OneDrive body.
pub fn require_body_envelope_for_process() {
    REQUIRE_BODY_ENVELOPE.store(true, Ordering::SeqCst);
}

/// Whether this process must reject plaintext body pass-through for OneDrive data.
pub fn body_envelope_required_for_process() -> bool {
    REQUIRE_BODY_ENVELOPE.load(Ordering::SeqCst)
}

#[cfg(any(test, debug_assertions))]
#[doc(hidden)]
pub fn reset_body_envelope_requirement_for_tests() {
    REQUIRE_BODY_ENVELOPE.store(false, Ordering::SeqCst);
}

/// Why an envelope could not be produced or opened. Never leaks key material.
#[derive(Debug, PartialEq, Eq)]
pub enum EnvelopeError {
    /// The blob is truncated, has a bad magic, or an unsupported version.
    Malformed(&'static str),
    /// AEAD verification failed — wrong key, or the ciphertext was tampered with.
    Decrypt,
}

impl std::fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvelopeError::Malformed(w) => write!(f, "malformed envelope: {w}"),
            EnvelopeError::Decrypt => write!(f, "envelope decryption failed"),
        }
    }
}
impl std::error::Error for EnvelopeError {}

fn build_header(key_id: u32, chunk_size: u32, plaintext_len: u64, nonce_base: [u8; 8]) -> [u8; 32] {
    let mut h = [0u8; HEADER_LEN];
    h[0..4].copy_from_slice(MAGIC);
    h[4] = BODY_ENVELOPE_VERSION;
    // h[5..8] reserved = 0
    h[8..12].copy_from_slice(&key_id.to_be_bytes());
    h[12..16].copy_from_slice(&chunk_size.to_be_bytes());
    h[16..24].copy_from_slice(&plaintext_len.to_be_bytes());
    h[24..32].copy_from_slice(&nonce_base);
    h
}

/// Nonce for chunk `i`: the per-blob 8-byte base then the 4-byte big-endian chunk index.
fn chunk_nonce(nonce_base: &[u8; 8], i: u32) -> Nonce {
    let mut n = [0u8; NONCE_LEN]; // 12
    n[0..8].copy_from_slice(nonce_base);
    n[8..12].copy_from_slice(&i.to_be_bytes());
    Nonce::assume_unique_for_key(n)
}

/// The `key_id` a sealed blob was made with — lets a reader pick the right key (rotation)
/// without decrypting. `None` if the header is malformed.
pub fn blob_key_id(blob: &[u8]) -> Option<u32> {
    if blob.len() < HEADER_LEN || &blob[0..4] != MAGIC {
        return None;
    }
    Some(u32::from_be_bytes([blob[8], blob[9], blob[10], blob[11]]))
}

/// The **plaintext** byte length a sealed blob encodes in its header — lets a caller compare
/// an on-disk sealed file's *logical* size against a stored (cloud/plaintext) size without
/// decrypting. `None` if the blob is not a sealed envelope (e.g. a desktop plaintext file).
pub fn blob_plaintext_len(blob: &[u8]) -> Option<u64> {
    if blob.len() < HEADER_LEN || &blob[0..4] != MAGIC {
        return None;
    }
    Some(u64::from_be_bytes([
        blob[16], blob[17], blob[18], blob[19], blob[20], blob[21], blob[22], blob[23],
    ]))
}

/// The plaintext size of the (possibly sealed) file at `path`: the envelope header's
/// `plaintext_len` for a sealed file, or the raw file length for a plaintext file (desktop) /
/// on any read error. Reads only the 32-byte header, never the whole body. Use this to compare
/// a materialized file's logical size to a stored plaintext size regardless of sealing.
pub fn on_disk_plaintext_len(path: &Path) -> u64 {
    use std::io::Read;
    let raw_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut hdr = [0u8; HEADER_LEN];
    match std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut hdr).map(|_| ())) {
        Ok(()) => blob_plaintext_len(&hdr).unwrap_or(raw_len),
        Err(_) => raw_len,
    }
}

/// Seal `plaintext` into a versioned, authenticated envelope with `key` (identified by
/// `key_id`). The output contains no plaintext bytes.
pub fn seal(plaintext: &[u8], key: &BodyKey, key_id: u32) -> Vec<u8> {
    seal_with_chunk(plaintext, key, key_id, DEFAULT_CHUNK)
}

fn seal_with_chunk(plaintext: &[u8], key: &BodyKey, key_id: u32, chunk_size: u32) -> Vec<u8> {
    let mut nonce_base = [0u8; 8];
    SystemRandom::new()
        .fill(&mut nonce_base)
        .expect("system RNG must be available");
    let header = build_header(key_id, chunk_size, plaintext.len() as u64, nonce_base);
    let sealing =
        LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).expect("AES-256 key is 32 bytes"));
    let mut out = Vec::with_capacity(header.len() + plaintext.len() + TAG_LEN);
    out.extend_from_slice(&header);
    let cs = chunk_size as usize;
    for (i, chunk) in plaintext.chunks(cs.max(1)).enumerate() {
        let mut buf = chunk.to_vec();
        sealing
            .seal_in_place_append_tag(
                chunk_nonce(&nonce_base, i as u32),
                Aad::from(&header),
                &mut buf,
            )
            .expect("seal cannot fail for a valid key/nonce");
        out.extend_from_slice(&buf);
    }
    out
}

/// Open an envelope sealed by [`seal`] with the matching `key`. Fails (never panics) on a
/// malformed blob or a wrong/tampered key.
pub fn open(blob: &[u8], key: &BodyKey) -> Result<Vec<u8>, EnvelopeError> {
    if blob.len() < HEADER_LEN {
        return Err(EnvelopeError::Malformed("short header"));
    }
    let header: [u8; HEADER_LEN] = blob[0..HEADER_LEN].try_into().unwrap();
    if &header[0..4] != MAGIC {
        return Err(EnvelopeError::Malformed("bad magic"));
    }
    if header[4] != BODY_ENVELOPE_VERSION {
        return Err(EnvelopeError::Malformed("unsupported version"));
    }
    let chunk_size = u32::from_be_bytes([header[12], header[13], header[14], header[15]]) as usize;
    let plaintext_len = u64::from_be_bytes(header[16..24].try_into().unwrap()) as usize;
    let mut nonce_base = [0u8; 8];
    nonce_base.copy_from_slice(&header[24..32]);
    if chunk_size == 0 {
        return Err(EnvelopeError::Malformed("zero chunk size"));
    }

    let opening =
        LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).expect("AES-256 key is 32 bytes"));
    let mut out = Vec::with_capacity(plaintext_len);
    let mut pos = HEADER_LEN;
    let mut i: u32 = 0;
    while out.len() < plaintext_len {
        let this_pt = (plaintext_len - out.len()).min(chunk_size);
        let ct_len = this_pt + TAG_LEN;
        let end = pos
            .checked_add(ct_len)
            .ok_or(EnvelopeError::Malformed("overflow"))?;
        if end > blob.len() {
            return Err(EnvelopeError::Malformed("truncated body"));
        }
        let mut buf = blob[pos..end].to_vec();
        let pt = opening
            .open_in_place(chunk_nonce(&nonce_base, i), Aad::from(&header), &mut buf)
            .map_err(|_| EnvelopeError::Decrypt)?;
        out.extend_from_slice(pt);
        pos = end;
        i += 1;
    }
    if out.len() != plaintext_len {
        return Err(EnvelopeError::Malformed("length mismatch"));
    }
    Ok(out)
}

// ---------------------------------------------------------------- process key registry
// The active body key (from the Android Keystore on mobile) plus any older keys retained
// for reading blobs sealed before a rotation. Set once at startup; a rotation prepends a
// new active key and keeps the old ones for decrypt. No key material is ever logged.

struct KeyRegistry {
    active: Option<(u32, BodyKey)>,
    older: Vec<(u32, BodyKey)>,
}
static KEYS: OnceLock<Mutex<KeyRegistry>> = OnceLock::new();
fn keys() -> &'static Mutex<KeyRegistry> {
    KEYS.get_or_init(|| {
        Mutex::new(KeyRegistry {
            active: None,
            older: Vec::new(),
        })
    })
}

/// Install the active body key (`key_id`, 32 bytes) — called once at startup after the
/// platform unwraps it (Keystore on mobile). A later call rotates: the previous active key
/// is retained so pre-rotation blobs still decrypt. No-op key material never leaves here.
pub fn set_body_key(key_id: u32, key: BodyKey) {
    let mut reg = keys().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(prev) = reg.active.take() {
        if prev.0 != key_id {
            reg.older.push(prev);
        }
    }
    reg.active = Some((key_id, key));
}

/// The active key id, or `None` when no key is installed (desktop plaintext / pre-unwrap).
pub fn active_key_id() -> Option<u32> {
    keys()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .active
        .map(|(id, _)| id)
}

fn key_for_id(key_id: u32) -> Option<BodyKey> {
    let reg = keys().lock().unwrap_or_else(|e| e.into_inner());
    if let Some((id, k)) = reg.active {
        if id == key_id {
            return Some(k);
        }
    }
    reg.older
        .iter()
        .find(|(id, _)| *id == key_id)
        .map(|(_, k)| *k)
}

/// Return the bytes to write to a body file: **sealed** when a body key is active, else the
/// plaintext unchanged (desktop / pre-unwrap). Lets a caller keep its own atomic
/// temp-file+rename while getting encryption-at-rest — the temp file then holds ciphertext,
/// so no plaintext temp survives. Read the file back with [`read_body`].
pub fn seal_for_disk(plaintext: &[u8]) -> Vec<u8> {
    match keys().lock().unwrap_or_else(|e| e.into_inner()).active {
        Some((key_id, key)) => seal(plaintext, &key, key_id),
        None => plaintext.to_vec(),
    }
}

/// Write `plaintext` to `final_path` **atomically** (temp file + rename) and **sealed** when
/// a body key is active — so a large file is never left partly written and no plaintext temp
/// file survives. With no active key (desktop) it writes plaintext, preserving today's
/// behaviour. The parent directory must already exist.
pub fn write_body_atomic(final_path: &Path, plaintext: &[u8]) -> std::io::Result<()> {
    let bytes = match keys().lock().unwrap_or_else(|e| e.into_inner()).active {
        Some((key_id, key)) => seal(plaintext, &key, key_id),
        None => plaintext.to_vec(),
    };
    let tmp = tmp_sibling(final_path);
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, final_path)
}

/// Read a body file written by [`write_body_atomic`]. If the file is a sealed envelope it is
/// opened with the matching key (by the header's `key_id`, honouring rotation); a plaintext
/// file (no magic — e.g. from before encryption, or desktop) is returned as-is. Fails
/// (never returns plaintext) if a sealed blob's key is missing or verification fails.
pub fn read_body(path: &Path) -> std::io::Result<Vec<u8>> {
    let raw = std::fs::read(path)?;
    match blob_key_id(&raw) {
        Some(key_id) => {
            let key = key_for_id(key_id).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, "no body key for blob")
            })?;
            open(&raw, &key).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        }
        None => Ok(raw), // plaintext (pre-encryption / desktop)
    }
}

/// Read a body file only if it is a valid sealed envelope. Unlike [`read_body`], this
/// rejects plaintext pass-through and is therefore the Android/mobile availability check.
pub fn read_sealed_body_required(path: &Path) -> std::io::Result<Vec<u8>> {
    let raw = std::fs::read(path)?;
    let Some(key_id) = blob_key_id(&raw) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "body is not a sealed envelope",
        ));
    };
    let key = key_for_id(key_id).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, "no body key for blob")
    })?;
    open(&raw, &key).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Verify that a body file is sealed and decryptable without exposing its bytes to callers.
pub fn probe_sealed_body_required(path: &Path) -> std::io::Result<()> {
    read_sealed_body_required(path).map(|_| ())
}

/// A temp sibling path in the SAME directory (so the rename is atomic on one filesystem).
fn tmp_sibling(final_path: &Path) -> std::path::PathBuf {
    let mut name = final_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".isytmp");
    final_path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: BodyKey = [7u8; 32];

    #[test]
    fn on_disk_plaintext_len_reads_the_header_not_the_sealed_size() {
        // A sealed blob is larger than its plaintext; the header encodes the plaintext length so
        // a caller can compare a materialized file's logical size without decrypting (#655).
        let sealed = seal(b"hello world", &KEY, 1);
        assert!(
            sealed.len() > 11,
            "sealed blob carries header + tag overhead"
        );
        assert_eq!(blob_plaintext_len(&sealed), Some(11));
        assert_eq!(blob_plaintext_len(b"not an envelope"), None);

        let dir = tempfile::tempdir().unwrap();
        let sp = dir.path().join("sealed.bin");
        std::fs::write(&sp, &sealed).unwrap();
        assert_eq!(
            on_disk_plaintext_len(&sp),
            11,
            "sealed file → plaintext length"
        );
        let pp = dir.path().join("plain.bin");
        std::fs::write(&pp, b"raw plaintext here").unwrap();
        assert_eq!(
            on_disk_plaintext_len(&pp),
            18,
            "plaintext file → raw length"
        );
        assert_eq!(on_disk_plaintext_len(&dir.path().join("missing")), 0);
    }

    #[test]
    fn roundtrips_empty_small_and_multichunk() {
        for len in [
            0usize,
            1,
            100,
            DEFAULT_CHUNK as usize,
            DEFAULT_CHUNK as usize + 1,
            200_000,
        ] {
            let pt: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let blob = seal(&pt, &KEY, 1);
            assert_eq!(open(&blob, &KEY).unwrap(), pt, "roundtrip len={len}");
        }
    }

    #[test]
    fn ciphertext_contains_no_plaintext_sentinel() {
        // The acceptance property: a raw scan of the blob must not find the plaintext.
        let sentinel = b"TOP-SECRET-SENTINEL-onedrive-body-42";
        let mut pt = Vec::new();
        for _ in 0..1000 {
            pt.extend_from_slice(sentinel);
        }
        let blob = seal(&pt, &KEY, 9);
        assert!(
            blob.windows(sentinel.len()).all(|w| w != sentinel),
            "sealed blob must not contain the plaintext sentinel"
        );
        // ...but it decrypts back to exactly the plaintext.
        assert_eq!(open(&blob, &KEY).unwrap(), pt);
    }

    #[test]
    fn wrong_key_and_tamper_fail_closed() {
        let pt = b"the quick brown fox".repeat(50);
        let blob = seal(&pt, &KEY, 1);
        // Wrong key → Decrypt error, never plaintext.
        let wrong: BodyKey = [8u8; 32];
        assert_eq!(open(&blob, &wrong), Err(EnvelopeError::Decrypt));
        // Flip a ciphertext byte → AEAD rejects it.
        let mut tampered = blob.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert_eq!(open(&tampered, &KEY), Err(EnvelopeError::Decrypt));
    }

    #[test]
    fn key_id_is_readable_without_the_key_for_rotation() {
        let blob = seal(b"x", &KEY, 0xABCD);
        assert_eq!(blob_key_id(&blob), Some(0xABCD));
        assert_eq!(blob_key_id(b"nope"), None);
    }

    #[test]
    fn malformed_blobs_error_not_panic() {
        assert!(matches!(open(b"", &KEY), Err(EnvelopeError::Malformed(_))));
        assert!(matches!(
            open(b"short", &KEY),
            Err(EnvelopeError::Malformed(_))
        ));
        let mut blob = seal(b"hello world", &KEY, 1);
        blob.truncate(HEADER_LEN + 3); // header ok, body truncated
        assert!(matches!(
            open(&blob, &KEY),
            Err(EnvelopeError::Malformed(_))
        ));
    }

    // One test owns the process-global key registry (so parallel tests don't race it).
    #[test]
    fn body_io_seals_on_disk_reads_plaintext_passthrough_and_rotates() {
        let dir = tempfile::tempdir().unwrap();
        set_body_key(1, KEY);
        assert_eq!(active_key_id(), Some(1));

        // A written body is sealed on disk: magic present, plaintext absent, no temp left.
        let path = dir.path().join("body.bin");
        let pt = b"sensitive-onedrive-body-CONTENT-x".repeat(200);
        write_body_atomic(&path, &pt).unwrap();
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(
            blob_key_id(&on_disk),
            Some(1),
            "on-disk is a sealed envelope"
        );
        assert!(
            on_disk.windows(7).all(|w| w != b"CONTENT"),
            "no plaintext on disk"
        );
        assert!(
            !path.with_file_name("body.bin.isytmp").exists(),
            "no leftover temp file"
        );
        assert_eq!(read_body(&path).unwrap(), pt, "read_body decrypts back");

        // A plaintext file (no magic) passes through unchanged even with a key active.
        let plain = dir.path().join("plain.txt");
        std::fs::write(&plain, b"i am plaintext").unwrap();
        assert_eq!(read_body(&plain).unwrap(), b"i am plaintext");

        // A blob whose key isn't registered → fail closed (never plaintext).
        let orphan = dir.path().join("orphan.bin");
        std::fs::write(&orphan, seal(b"secret", &[9u8; 32], 777)).unwrap();
        assert_eq!(
            read_body(&orphan).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );

        // Rotation: a new active key still lets the old (key_id 1) blob decrypt.
        set_body_key(2, [2u8; 32]);
        assert_eq!(active_key_id(), Some(2));
        assert_eq!(
            read_body(&path).unwrap(),
            pt,
            "pre-rotation blob still decrypts"
        );
    }

    #[test]
    fn sealed_body_required_rejects_plaintext_and_invalid_envelopes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sealed.bin");
        let plaintext = b"android-body-sentinel".repeat(32);
        let key_id = 42_4242;
        set_body_key(key_id, KEY);
        write_body_atomic(&path, &plaintext).unwrap();

        assert_eq!(read_sealed_body_required(&path).unwrap(), plaintext);
        probe_sealed_body_required(&path).unwrap();

        let plain = dir.path().join("plain.txt");
        std::fs::write(&plain, b"raw android-body-sentinel").unwrap();
        assert_eq!(
            read_body(&plain).unwrap(),
            b"raw android-body-sentinel",
            "desktop plaintext pass-through stays unchanged"
        );
        assert_eq!(
            read_sealed_body_required(&plain).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData,
            "mobile strict helper must reject plaintext"
        );

        let orphan = dir.path().join("orphan.bin");
        std::fs::write(&orphan, seal(b"secret", &[9u8; 32], 99_999)).unwrap();
        assert_eq!(
            read_sealed_body_required(&orphan).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied,
            "unknown key id must fail closed"
        );

        let tampered = dir.path().join("tampered.bin");
        let mut blob = std::fs::read(&path).unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0x01;
        std::fs::write(&tampered, blob).unwrap();
        assert_eq!(
            read_sealed_body_required(&tampered).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData,
            "AEAD tampering must fail closed"
        );
    }
}
