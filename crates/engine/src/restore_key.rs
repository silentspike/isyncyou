//! Content-derived idempotency key + per-install secret for crash-safe restore
//! (ADR-001).
//!
//! The key is `HMAC-SHA256(secret, account ‖ service ‖ source_id ‖ payload)` rendered
//! as lowercase hex. Identical content yields an identical key, so a retry after a
//! crash is recognised as "the same restore" (and the store's
//! `UNIQUE(account, idempotency_key)` rejects a duplicate). The `secret` is a
//! per-install random value kept on disk (never logged), so keys are not guessable
//! across installs.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::io::Write;
use std::path::Path;

type HmacSha256 = Hmac<Sha256>;

/// Compute the idempotency key for one restore operation. Fields are `\0`-separated
/// so distinct field boundaries cannot collide by concatenation.
pub fn idempotency_key(
    secret: &[u8],
    account: &str,
    service: &str,
    source_id: &str,
    payload: &[u8],
) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(account.as_bytes());
    mac.update(b"\0");
    mac.update(service.as_bytes());
    mac.update(b"\0");
    mac.update(source_id.as_bytes());
    mac.update(b"\0");
    mac.update(payload);
    let bytes = mac.finalize().into_bytes();
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// A synthetic, findable mail marker derived from the key: an RFC-5322 `Message-ID`
/// using the reserved `.invalid` TLD (guaranteed never to resolve), so embedding it
/// in the restored MIME and probing Graph's `internetMessageId` for it is safe.
pub fn mail_marker(key: &str) -> String {
    format!("<isyncyou-{key}@restore.invalid>")
}

/// A calendar marker derived from the key: a Microsoft Graph **transactionId** for
/// `POST /me/events`. Graph uses it for server-side de-duplication — a retry with the
/// same value returns the existing event instead of creating a second one (confirmed
/// live in `tools/live_calendar_probe.py`), so calendar crash recovery re-POSTs and
/// relies on that de-dup. Graph does **not** support a `transactionId` `$filter` query
/// (HTTP 400), so there is no probe. Kept well under Graph's 256-char transactionId
/// limit.
pub fn calendar_marker(key: &str) -> String {
    format!("isyncyou-restore-{key}")
}

/// Load the per-install restore secret from `path`, creating it (32 random bytes,
/// owner-only) if it does not exist. The secret is binary and never logged.
pub fn load_or_create_secret(path: &Path) -> Result<Vec<u8>, String> {
    match std::fs::read(path) {
        Ok(b) if b.len() >= 16 => Ok(b),
        Ok(_) => Err(format!("restore secret at {} is too short", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let secret = random_32()?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            write_owner_only(path, &secret)?;
            Ok(secret)
        }
        Err(e) => Err(format!("read restore secret {}: {e}", path.display())),
    }
}

/// 32 random bytes from the OS CSPRNG (`/dev/urandom`), no extra dependency.
fn random_32() -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; 32];
    let mut f = std::fs::File::open("/dev/urandom").map_err(|e| e.to_string())?;
    std::io::Read::read_exact(&mut f, &mut buf).map_err(|e| e.to_string())?;
    Ok(buf)
}

/// Write `data` to `path` with `0600` permissions (best-effort on non-unix).
fn write_owner_only(path: &Path, data: &[u8]) -> Result<(), String> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(|e| e.to_string())?;
    f.write_all(data).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"test-secret-0123456789abcdef";

    #[test]
    fn key_is_deterministic_and_hex() {
        let k1 = idempotency_key(SECRET, "acc", "mail", "id1", b"body");
        let k2 = idempotency_key(SECRET, "acc", "mail", "id1", b"body");
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 64); // sha256 -> 32 bytes -> 64 hex chars
        assert!(k1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn key_changes_with_every_field() {
        let base = idempotency_key(SECRET, "acc", "mail", "id1", b"body");
        assert_ne!(
            base,
            idempotency_key(SECRET, "acc2", "mail", "id1", b"body")
        );
        assert_ne!(
            base,
            idempotency_key(SECRET, "acc", "calendar", "id1", b"body")
        );
        assert_ne!(base, idempotency_key(SECRET, "acc", "mail", "id2", b"body"));
        assert_ne!(
            base,
            idempotency_key(SECRET, "acc", "mail", "id1", b"body2")
        );
        assert_ne!(
            base,
            idempotency_key(b"other-secret-xx", "acc", "mail", "id1", b"body")
        );
    }

    #[test]
    fn field_boundaries_do_not_collide() {
        // "ab|c" vs "a|bc" must differ thanks to the \0 separators.
        assert_ne!(
            idempotency_key(SECRET, "ab", "c", "x", b"p"),
            idempotency_key(SECRET, "a", "bc", "x", b"p"),
        );
    }

    #[test]
    fn mail_marker_is_a_valid_invalid_message_id() {
        let m = mail_marker("deadbeef");
        assert!(m.starts_with("<isyncyou-deadbeef@"));
        assert!(m.ends_with(".invalid>"));
    }

    #[test]
    fn calendar_marker_is_a_short_stable_transaction_id() {
        let m = calendar_marker("deadbeef");
        assert_eq!(m, "isyncyou-restore-deadbeef");
        // A 64-hex key keeps the transactionId well under Graph's 256-char limit.
        assert!(calendar_marker(&"a".repeat(64)).len() < 256);
    }

    #[test]
    fn secret_is_created_then_reused() {
        let dir = std::env::temp_dir().join(format!("isyncyou-secret-{}", std::process::id()));
        let p = dir.join("sub").join(".isyncyou-restore-secret");
        let _ = std::fs::remove_dir_all(&dir);
        let s1 = load_or_create_secret(&p).unwrap();
        assert_eq!(s1.len(), 32);
        assert!(p.exists());
        let s2 = load_or_create_secret(&p).unwrap();
        assert_eq!(s1, s2); // stable across calls
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
