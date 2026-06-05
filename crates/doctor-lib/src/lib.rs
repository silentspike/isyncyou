//! `isyncyou-doctor-lib` — reusable health checks for the standalone doctor.
//!
//! The binary stays deliberately small: it parses CLI args, calls [`run_checks`],
//! prints the report, and exits non-zero only for hard failures.

use isyncyou_core::Config;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

impl Level {
    pub fn mark(self) -> &'static str {
        match self {
            Level::Ok => "[ ok ]",
            Level::Warn => "[warn]",
            Level::Fail => "[FAIL]",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub level: Level,
    pub detail: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    fn push(&mut self, name: &str, level: Level, detail: impl Into<String>) {
        self.checks.push(Check {
            name: name.into(),
            level,
            detail: detail.into(),
        });
    }

    /// Exit code severity: `Fail` if any check failed, else `Warn` if any warned.
    pub fn worst(&self) -> Level {
        self.checks
            .iter()
            .map(|c| c.level)
            .fold(Level::Ok, |a, b| match (a, b) {
                (Level::Fail, _) | (_, Level::Fail) => Level::Fail,
                (Level::Warn, _) | (_, Level::Warn) => Level::Warn,
                _ => Level::Ok,
            })
    }
}

/// Minimum free space below which disk is flagged.
const MIN_FREE: u64 = 100 * 1024 * 1024;

/// Sibling binaries the install should provide next to the doctor.
const SIBLING_BINS: &[&str] = &["isyncyou", "isyncyoud"];

/// Install-integrity check: are the sibling binaries present in `dir`?
fn check_install(dir: &Path) -> (Level, String) {
    let missing: Vec<&str> = SIBLING_BINS
        .iter()
        .copied()
        .filter(|b| !dir.join(b).exists())
        .collect();
    if missing.is_empty() {
        (Level::Ok, format!("{} present", SIBLING_BINS.join(" + ")))
    } else {
        (
            Level::Warn,
            format!("missing sibling binaries: {}", missing.join(", ")),
        )
    }
}

const TOKEN_CACHE_MAGIC: &str = "isyncyou-token-cache-encrypted-v1";
const TOKEN_CACHE_KEYRING_MAGIC: &str = "isyncyou-token-cache-keyring-v1";
const TOKEN_CACHE_SECRET_ENV: &str = "ISYNCYOU_TOKEN_CACHE_KEY";
const TOKEN_CACHE_SECRET_FILE_ENV: &str = "ISYNCYOU_TOKEN_CACHE_KEY_FILE";
const TOKEN_CACHE_SYSTEMD_CREDENTIAL: &str = "isyncyou-token-cache-key";
const SYSTEMD_CREDENTIALS_DIR_ENV: &str = "CREDENTIALS_DIRECTORY";
const STORE_KEY_ENV: &str = "ISYNCYOU_STORE_KEY";
const STORE_KEY_FILE_ENV: &str = "ISYNCYOU_STORE_KEY_FILE";
const STORE_SYSTEMD_CREDENTIAL: &str = "isyncyou-store-key";
const SQLITE_HEADER: &[u8] = b"SQLite format 3\0";

fn token_cache_secret_available() -> bool {
    if let Some(path) = std::env::var_os(TOKEN_CACHE_SECRET_FILE_ENV) {
        return secret_file_nonempty(Path::new(&path));
    }
    if let Some(dir) = std::env::var_os(SYSTEMD_CREDENTIALS_DIR_ENV) {
        let path = PathBuf::from(dir).join(TOKEN_CACHE_SYSTEMD_CREDENTIAL);
        if path.exists() {
            return secret_file_nonempty(&path);
        }
    }
    std::env::var(TOKEN_CACHE_SECRET_ENV).is_ok_and(|s| !s.trim().is_empty())
}

fn store_secret_available() -> bool {
    if let Some(path) = std::env::var_os(STORE_KEY_FILE_ENV) {
        return secret_file_nonempty(Path::new(&path));
    }
    if let Some(dir) = std::env::var_os(SYSTEMD_CREDENTIALS_DIR_ENV) {
        let path = PathBuf::from(dir).join(STORE_SYSTEMD_CREDENTIAL);
        if path.exists() {
            return secret_file_nonempty(&path);
        }
    }
    std::env::var(STORE_KEY_ENV).is_ok_and(|s| !s.trim().is_empty())
}

fn secret_file_nonempty(path: &Path) -> bool {
    std::fs::read(path).is_ok_and(|bytes| bytes.iter().any(|b| !b.is_ascii_whitespace()))
}

fn check_store(path: &Path, secret_available: bool) -> (Level, String) {
    let meta = match std::fs::metadata(path) {
        Ok(m) if m.len() > 0 => m,
        Ok(_) => return (Level::Warn, "store file is empty (never synced?)".into()),
        Err(_) => return (Level::Warn, format!("no store yet at {}", path.display())),
    };
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => return (Level::Warn, format!("cannot inspect store header: {e}")),
    };
    let plaintext_sqlite = bytes.starts_with(SQLITE_HEADER);
    match (plaintext_sqlite, secret_available) {
        (false, true) => (
            Level::Ok,
            format!(
                "{} ({} bytes, SQLCipher/non-plaintext header)",
                path.display(),
                meta.len()
            ),
        ),
        (false, false) => (
            Level::Warn,
            format!(
                "{} does not have a plaintext SQLite header; configure {STORE_KEY_FILE_ENV} or systemd credential {STORE_SYSTEMD_CREDENTIAL} before opening it",
                path.display()
            ),
        ),
        (true, true) => (
            Level::Warn,
            format!(
                "{} is a plaintext SQLite store even though a store key is configured; migrate/recreate it as SQLCipher",
                path.display()
            ),
        ),
        (true, false) => (
            Level::Warn,
            format!(
                "{} is a plaintext SQLite store; configure {STORE_KEY_FILE_ENV} or systemd credential {STORE_SYSTEMD_CREDENTIAL}",
                path.display()
            ),
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenCacheKind {
    Plain,
    Encrypted,
    Keyring,
}

fn token_cache_kind(bytes: &[u8]) -> Option<TokenCacheKind> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    match v.get("isyncyou_token_cache").and_then(|m| m.as_str()) {
        Some(TOKEN_CACHE_MAGIC) => Some(TokenCacheKind::Encrypted),
        Some(TOKEN_CACHE_KEYRING_MAGIC) => Some(TokenCacheKind::Keyring),
        _ => Some(TokenCacheKind::Plain),
    }
}

fn check_token_cache(path: &Path, secret_available: bool) -> (Level, String) {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => {
            return (
                Level::Warn,
                "no cached login token - run `isyncyou login`".into(),
            )
        }
    };
    match token_cache_kind(&bytes) {
        Some(TokenCacheKind::Keyring) => (
            Level::Ok,
            "desktop keyring login token marker cached (secret is not on disk)".into(),
        ),
        Some(TokenCacheKind::Encrypted) if secret_available => {
            (Level::Ok, "encrypted login token cached".into())
        }
        Some(TokenCacheKind::Encrypted) => (
            Level::Warn,
            "encrypted login token cached, but no token-cache secret is available".into(),
        ),
        Some(TokenCacheKind::Plain) if secret_available => (
            Level::Warn,
            "plaintext login token cached; next refresh/login should rewrite it encrypted".into(),
        ),
        Some(TokenCacheKind::Plain) => (
            Level::Warn,
            "plaintext login token cached; configure ISYNCYOU_TOKEN_CACHE_KEY_FILE or systemd credential isyncyou-token-cache-key".into(),
        ),
        None => (Level::Warn, "login token cache is not valid JSON".into()),
    }
}

pub fn run_checks(config_path: &Path) -> Report {
    let mut r = Report::default();

    // Install integrity: the CLI + daemon should sit next to the doctor.
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        let (lvl, det) = check_install(&dir);
        r.push("install", lvl, det);
    }

    let cfg = match Config::load(config_path) {
        Ok(c) => {
            match c.validate() {
                Ok(()) => r.push(
                    "config",
                    Level::Ok,
                    format!("{} account(s)", c.accounts.len()),
                ),
                Err(errs) => r.push("config", Level::Fail, errs.join("; ")),
            }
            c
        }
        Err(e) => {
            r.push(
                "config",
                Level::Fail,
                format!("cannot load {}: {e}", config_path.display()),
            );
            return r;
        }
    };

    let token_cache_secret_available = token_cache_secret_available();
    let store_secret_available = store_secret_available();
    for acc in &cfg.accounts {
        let db = acc.archive_root.join(".isyncyou-store.db");
        let (level, detail) = check_store(&db, store_secret_available);
        r.push(&format!("store[{}]", acc.id), level, detail);

        match available_space(&acc.archive_root) {
            Some(free) if free < MIN_FREE => {
                r.push(
                    &format!("disk[{}]", acc.id),
                    Level::Fail,
                    format!("only {} MiB free", free / 1024 / 1024),
                );
            }
            Some(free) => r.push(
                &format!("disk[{}]", acc.id),
                Level::Ok,
                format!("{} MiB free", free / 1024 / 1024),
            ),
            None => r.push(
                &format!("disk[{}]", acc.id),
                Level::Warn,
                "could not determine free space",
            ),
        }

        let read_tok = acc.archive_root.join(".isyncyou-token-read.json");
        let (level, detail) = check_token_cache(&read_tok, token_cache_secret_available);
        r.push(&format!("auth[{}]", acc.id), level, detail);
    }

    r
}

/// Free space at `path`, walking up to the nearest existing ancestor.
fn available_space(path: &Path) -> Option<u64> {
    let mut p = path;
    loop {
        if p.exists() {
            return fs2::available_space(p).ok();
        }
        p = p.parent()?;
    }
}

pub fn parse_config_arg(args: &[String]) -> PathBuf {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--config" {
            if let Some(v) = it.next() {
                return PathBuf::from(v);
            }
        } else if let Some(v) = a.strip_prefix("--config=") {
            return PathBuf::from(v);
        }
    }
    PathBuf::from("isyncyou.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config_arg_variants() {
        assert_eq!(
            parse_config_arg(&["--config".into(), "a.toml".into()]),
            PathBuf::from("a.toml")
        );
        assert_eq!(
            parse_config_arg(&["--config=b.toml".into()]),
            PathBuf::from("b.toml")
        );
        assert_eq!(parse_config_arg(&[]), PathBuf::from("isyncyou.toml"));
    }

    #[test]
    fn missing_config_is_fail() {
        let r = run_checks(Path::new("/nonexistent/iSyncYou-doctor-xyz.toml"));
        assert!(r
            .checks
            .iter()
            .any(|c| c.name == "config" && c.level == Level::Fail));
        assert_eq!(r.worst(), Level::Fail);
    }

    #[test]
    fn check_install_present_vs_missing() {
        let dir = std::env::temp_dir().join(format!("doctor-inst-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(check_install(&dir).0, Level::Warn);
        for b in SIBLING_BINS {
            std::fs::write(dir.join(b), b"x").unwrap();
        }
        assert_eq!(check_install(&dir).0, Level::Ok);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_login_token_is_an_auth_warning() {
        let dir = std::env::temp_dir().join(format!("doctor-auth-{}", std::process::id()));
        let arch = dir.join("archive");
        std::fs::create_dir_all(&arch).unwrap();
        let p = dir.join("c.toml");
        std::fs::write(
            &p,
            format!(
                "[[accounts]]\nid=\"a\"\nusername=\"a@x\"\nsync_root=\"{}/od\"\narchive_root=\"{}\"\n",
                dir.display(),
                arch.display()
            ),
        )
        .unwrap();
        let r = run_checks(&p);
        assert!(r
            .checks
            .iter()
            .any(|c| c.name == "auth[a]" && c.level == Level::Warn));
        assert_ne!(r.worst(), Level::Fail);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plaintext_login_token_is_an_auth_warning_without_secret() {
        let dir =
            std::env::temp_dir().join(format!("doctor-auth-plaintext-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(".isyncyou-token-read.json");
        std::fs::write(
            &p,
            br#"{"access_token":"AT","refresh_token":"RT","expires_at":1}"#,
        )
        .unwrap();

        let (level, detail) = check_token_cache(&p, false);
        assert_eq!(level, Level::Warn);
        assert!(detail.contains("plaintext login token cached"));
        assert!(detail.contains("ISYNCYOU_TOKEN_CACHE_KEY_FILE"));

        let (level, detail) = check_token_cache(&p, true);
        assert_eq!(level, Level::Warn);
        assert!(detail.contains("next refresh/login should rewrite"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn encrypted_login_token_with_secret_is_ok() {
        let dir =
            std::env::temp_dir().join(format!("doctor-auth-encrypted-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(".isyncyou-token-read.json");
        std::fs::write(
            &p,
            br#"{"isyncyou_token_cache":"isyncyou-token-cache-encrypted-v1","ciphertext_hex":"00"}"#,
        )
        .unwrap();

        let (level, detail) = check_token_cache(&p, true);
        assert_eq!(level, Level::Ok);
        assert_eq!(detail, "encrypted login token cached");

        let (level, detail) = check_token_cache(&p, false);
        assert_eq!(level, Level::Warn);
        assert!(detail.contains("no token-cache secret"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keyring_login_token_marker_is_ok_without_plaintext_secret() {
        let dir = std::env::temp_dir().join(format!("doctor-auth-keyring-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(".isyncyou-token-read.json");
        std::fs::write(
            &p,
            br#"{"isyncyou_token_cache":"isyncyou-token-cache-keyring-v1","keyring_service":"org.silentspike.isyncyou.token-cache","keyring_user":"/tmp/token.json"}"#,
        )
        .unwrap();

        let (level, detail) = check_token_cache(&p, false);
        assert_eq!(level, Level::Ok);
        assert!(detail.contains("desktop keyring"));
        assert!(detail.contains("not on disk"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plaintext_store_is_a_warning_until_sqlcipher_keyed() {
        let dir =
            std::env::temp_dir().join(format!("doctor-store-plaintext-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(".isyncyou-store.db");
        let mut bytes = SQLITE_HEADER.to_vec();
        bytes.extend_from_slice(b"rest of sqlite file");
        std::fs::write(&p, bytes).unwrap();

        let (level, detail) = check_store(&p, false);
        assert_eq!(level, Level::Warn);
        assert!(detail.contains("plaintext SQLite store"));
        assert!(detail.contains("ISYNCYOU_STORE_KEY_FILE"));

        let (level, detail) = check_store(&p, true);
        assert_eq!(level, Level::Warn);
        assert!(detail.contains("migrate/recreate"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_plaintext_store_is_ok_only_when_store_key_is_available() {
        let dir =
            std::env::temp_dir().join(format!("doctor-store-encrypted-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(".isyncyou-store.db");
        std::fs::write(&p, b"\x8c\x13not-a-plain-sqlite-header").unwrap();

        let (level, detail) = check_store(&p, true);
        assert_eq!(level, Level::Ok);
        assert!(detail.contains("SQLCipher"));

        let (level, detail) = check_store(&p, false);
        assert_eq!(level, Level::Warn);
        assert!(detail.contains("does not have a plaintext SQLite header"));
        assert!(detail.contains("isyncyou-store-key"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_config_is_fail() {
        let dir = std::env::temp_dir().join(format!("doctor-test-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("c.toml");
        std::fs::write(
            &p,
            "[sync.delete_guard]\nmax_absolute=0\nmax_fraction=9.0\nfraction_min_total=10\n",
        )
        .unwrap();
        let r = run_checks(&p);
        assert_eq!(r.worst(), Level::Fail);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn healthy_config_with_missing_store_is_warn_not_fail() {
        let dir = std::env::temp_dir().join(format!("doctor-test-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("c.toml");
        let arch = dir.join("archive");
        std::fs::create_dir_all(&arch).unwrap();
        std::fs::write(
            &p,
            format!(
                "[[accounts]]\nid=\"a\"\nusername=\"a@x\"\nsync_root=\"{}/od\"\narchive_root=\"{}\"\n",
                dir.display(),
                arch.display()
            ),
        )
        .unwrap();
        let r = run_checks(&p);
        assert_ne!(r.worst(), Level::Fail);
        assert!(r
            .checks
            .iter()
            .any(|c| c.name.starts_with("store[") && c.level == Level::Warn));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
