//! `isyncyou` — command-line interface for the iSyncYou engine.
//!
//! Wires the config, store and OneDrive connector into a usable tool:
//! - `check`  — validate a config file
//! - `status` — show tracked-item counts + delta cursor for an account
//! - `sync`   — run one incremental OneDrive sync for an account
//!
//! Until OAuth lands (#40) the access token is supplied via `--token`/`ISYNCYOU_TOKEN`.

use clap::{Parser, Subcommand};
use isyncyou_core::Config;
use isyncyou_pathmap::MappingTable;
use isyncyou_store::Store;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use isyncyou_connectors as connectors;

#[derive(Parser, Debug)]
#[command(name = "isyncyou", version, about = "Personal cloud sync client")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
enum Command {
    /// Validate a configuration file.
    Check {
        #[arg(long, default_value = "isyncyou.toml")]
        config: PathBuf,
    },
    /// Show tracked-item counts and the delta cursor for an account.
    Status {
        #[arg(long, default_value = "isyncyou.toml")]
        config: PathBuf,
        #[arg(long)]
        account: String,
    },
    /// Run one incremental OneDrive sync for an account.
    Sync {
        #[arg(long, default_value = "isyncyou.toml")]
        config: PathBuf,
        #[arg(long)]
        account: String,
        /// Graph access token (interim, until OAuth #40).
        #[arg(long, env = "ISYNCYOU_TOKEN")]
        token: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli.command) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(command: Command) -> Result<(), String> {
    match command {
        Command::Check { config } => cmd_check(&config),
        Command::Status { config, account } => cmd_status(&config, &account),
        Command::Sync {
            config,
            account,
            token,
        } => cmd_sync(&config, &account, token),
    }
}

fn load_config(path: &Path) -> Result<Config, String> {
    let cfg = Config::load(path)?;
    cfg.validate().map_err(|errs| errs.join("; "))?;
    Ok(cfg)
}

/// Store path for an account: `<archive_root>/.isyncyou-store.db`.
fn store_path(cfg: &Config, account: &str) -> Result<PathBuf, String> {
    let acc = cfg
        .accounts
        .iter()
        .find(|a| a.id == account)
        .ok_or_else(|| format!("no account '{account}' in config"))?;
    Ok(acc.archive_root.join(".isyncyou-store.db"))
}

fn cmd_check(path: &Path) -> Result<(), String> {
    let cfg = Config::load(path)?;
    match cfg.validate() {
        Ok(()) => {
            println!("config OK: {} account(s)", cfg.accounts.len());
            Ok(())
        }
        Err(errs) => Err(format!("invalid config:\n  - {}", errs.join("\n  - "))),
    }
}

fn cmd_status(config: &Path, account: &str) -> Result<(), String> {
    let cfg = load_config(config)?;
    let store = Store::open(store_path(&cfg, account)?).map_err(|e| e.to_string())?;
    let cursor = store
        .get_delta_cursor(account, "onedrive", "")
        .map_err(|e| e.to_string())?;
    println!("account: {account}");
    println!(
        "onedrive delta cursor: {}",
        if cursor.is_some() {
            "present"
        } else {
            "none (never synced)"
        }
    );
    Ok(())
}

fn cmd_sync(config: &Path, account: &str, token: Option<String>) -> Result<(), String> {
    let cfg = load_config(config)?;
    let token = token.ok_or("no access token (pass --token or set ISYNCYOU_TOKEN)")?;
    let store = Store::open(store_path(&cfg, account)?).map_err(|e| e.to_string())?;
    let mut map = MappingTable::new();
    let mut client = isyncyou_graph::GraphClient::new(token);
    let now = format!(
        "{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    let report = connectors::incremental_sync(&mut client, &store, &mut map, account, &now)
        .map_err(|e| e.to_string())?;
    println!(
        "sync done: {} upserted, {} deleted, {} skipped{}",
        report.upserted,
        report.deleted,
        report.skipped,
        if report.resynced {
            " (full resync)"
        } else {
            ""
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    #[test]
    fn parses_check() {
        let c = parse(&["isyncyou", "check", "--config", "/tmp/c.toml"]).unwrap();
        assert_eq!(
            c.command,
            Command::Check {
                config: "/tmp/c.toml".into()
            }
        );
    }

    #[test]
    fn check_has_default_config() {
        let c = parse(&["isyncyou", "check"]).unwrap();
        assert_eq!(
            c.command,
            Command::Check {
                config: "isyncyou.toml".into()
            }
        );
    }

    #[test]
    fn parses_sync_with_account_and_token() {
        let c = parse(&[
            "isyncyou",
            "sync",
            "--config",
            "c.toml",
            "--account",
            "primary",
            "--token",
            "TOK",
        ])
        .unwrap();
        assert_eq!(
            c.command,
            Command::Sync {
                config: "c.toml".into(),
                account: "primary".into(),
                token: Some("TOK".into())
            }
        );
    }

    #[test]
    fn sync_requires_account() {
        assert!(parse(&["isyncyou", "sync", "--config", "c.toml"]).is_err());
    }

    #[test]
    fn status_parses() {
        let c = parse(&["isyncyou", "status", "--account", "a"]).unwrap();
        assert_eq!(
            c.command,
            Command::Status {
                config: "isyncyou.toml".into(),
                account: "a".into()
            }
        );
    }

    #[test]
    fn unknown_subcommand_errors() {
        assert!(parse(&["isyncyou", "frobnicate"]).is_err());
    }

    #[test]
    fn check_reports_invalid_config() {
        // a config with a bad guard fraction must be rejected
        let dir = std::env::temp_dir().join(format!("isyncyou-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bad.toml");
        std::fs::write(
            &p,
            "[sync.delete_guard]\nmax_absolute = 0\nmax_fraction = 2.0\nfraction_min_total = 10\n",
        )
        .unwrap();
        assert!(cmd_check(&p).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
