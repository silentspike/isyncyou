//! `isyncyou-doctor` — standalone health/recovery checker.
//!
//! Deliberately minimal-dependency so it runs even when the daemon/GUI are
//! broken. It checks the configuration, each account's store file, and free disk
//! space, prints a report, and exits non-zero if anything is red.
//!
//! Usage: `isyncyou-doctor [--config <path>]` (default `isyncyou.toml`).

use isyncyou_core::Config;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Ok,
    Warn,
    Fail,
}

impl Level {
    fn mark(self) -> &'static str {
        match self {
            Level::Ok => "[ ok ]",
            Level::Warn => "[warn]",
            Level::Fail => "[FAIL]",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Check {
    name: String,
    level: Level,
    detail: String,
}

#[derive(Debug, Default)]
struct Report {
    checks: Vec<Check>,
}

impl Report {
    fn push(&mut self, name: &str, level: Level, detail: impl Into<String>) {
        self.checks.push(Check {
            name: name.into(),
            level,
            detail: detail.into(),
        });
    }
    /// Exit code: 0 if nothing failed, 1 otherwise.
    fn worst(&self) -> Level {
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

/// Minimum free space (bytes) below which disk is flagged.
const MIN_FREE: u64 = 100 * 1024 * 1024; // 100 MiB

fn run_checks(config_path: &Path) -> Report {
    let mut r = Report::default();

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

    for acc in &cfg.accounts {
        let db = acc.archive_root.join(".isyncyou-store.db");
        match std::fs::metadata(&db) {
            Ok(m) if m.len() > 0 => {
                r.push(
                    &format!("store[{}]", acc.id),
                    Level::Ok,
                    format!("{} ({} bytes)", db.display(), m.len()),
                );
            }
            Ok(_) => r.push(
                &format!("store[{}]", acc.id),
                Level::Warn,
                "store file is empty (never synced?)",
            ),
            Err(_) => r.push(
                &format!("store[{}]", acc.id),
                Level::Warn,
                format!("no store yet at {}", db.display()),
            ),
        }

        // free space at the archive root (or its nearest existing ancestor)
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

fn parse_config_arg(args: &[String]) -> PathBuf {
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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = parse_config_arg(&args);
    let report = run_checks(&config);

    println!("iSyncYou doctor — {}", config.display());
    for c in &report.checks {
        println!("  {} {:<14} {}", c.level.mark(), c.name, c.detail);
    }
    match report.worst() {
        Level::Fail => {
            println!("status: PROBLEMS FOUND");
            std::process::exit(1);
        }
        Level::Warn => println!("status: ok with warnings"),
        Level::Ok => println!("status: healthy"),
    }
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
        assert_eq!(r.checks.len(), 1);
        assert_eq!(r.checks[0].level, Level::Fail);
        assert_eq!(r.worst(), Level::Fail);
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
        // config ok, store missing -> warn, disk ok -> overall not Fail
        assert_ne!(r.worst(), Level::Fail);
        assert!(r
            .checks
            .iter()
            .any(|c| c.name.starts_with("store[") && c.level == Level::Warn));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
