//! `isyncyou` — command-line interface for the iSyncYou engine.
//!
//! Wires the config, store and connectors into a usable tool:
//! - `check`  — validate a config file
//! - `status` — show tracked-item counts + delta cursor for an account
//! - `sync`   — run one incremental OneDrive sync for an account
//! - `backup` — index + archive M365 services (mail/calendar/contacts/todo/onenote)
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
    /// Back up M365 services: index (delta) + archive bodies to the archive root.
    Backup {
        #[arg(long, default_value = "isyncyou.toml")]
        config: PathBuf,
        #[arg(long)]
        account: String,
        /// One of mail|calendar|contacts|todo|onenote; omitted = all.
        #[arg(long)]
        service: Option<String>,
        /// Max bodies to download per service this pass (0 = all).
        #[arg(long, default_value_t = 0)]
        body_limit: usize,
        /// Calendar window start (RFC3339).
        #[arg(long, default_value = "2015-01-01T00:00:00Z")]
        cal_start: String,
        /// Calendar window end (RFC3339).
        #[arg(long, default_value = "2035-01-01T00:00:00Z")]
        cal_end: String,
        #[arg(long, env = "ISYNCYOU_TOKEN")]
        token: Option<String>,
    },
    /// Full-text search the local archive index (item names/subjects/titles).
    Search {
        #[arg(long, default_value = "isyncyou.toml")]
        config: PathBuf,
        #[arg(long)]
        account: String,
        /// FTS5 query (e.g. "invoice", "report 2026").
        #[arg(long)]
        query: String,
    },
    /// Restore one archived item back to the cloud (re-create via Graph).
    Restore {
        #[arg(long, default_value = "isyncyou.toml")]
        config: PathBuf,
        #[arg(long)]
        account: String,
        /// One of mail|calendar|contacts|todo.
        #[arg(long)]
        service: String,
        /// The archived item's `remote_id`.
        #[arg(long)]
        id: String,
        /// Graph **write** access token (Mail/Calendars/Contacts/Tasks.ReadWrite).
        #[arg(long, env = "ISYNCYOU_TOKEN")]
        token: Option<String>,
    },
}

/// The M365 backup services this CLI knows how to drive.
const SERVICES: &[&str] = &["mail", "calendar", "contacts", "todo", "onenote"];
/// Services with a restore path (OneNote pages can't be re-created via a simple POST).
const RESTORE_SERVICES: &[&str] = &["mail", "calendar", "contacts", "todo"];

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
        Command::Backup {
            config,
            account,
            service,
            body_limit,
            cal_start,
            cal_end,
            token,
        } => cmd_backup(
            &config, &account, service, body_limit, &cal_start, &cal_end, token,
        ),
        Command::Search {
            config,
            account,
            query,
        } => cmd_search(&config, &account, &query),
        Command::Restore {
            config,
            account,
            service,
            id,
            token,
        } => cmd_restore(&config, &account, &service, &id, token),
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

/// Current unix time as a string, used as the deterministic tombstone stamp.
fn unix_now() -> String {
    format!(
        "{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    )
}

fn cmd_sync(config: &Path, account: &str, token: Option<String>) -> Result<(), String> {
    let cfg = load_config(config)?;
    let token = token.ok_or("no access token (pass --token or set ISYNCYOU_TOKEN)")?;
    let store = Store::open(store_path(&cfg, account)?).map_err(|e| e.to_string())?;
    let mut map = MappingTable::new();
    let mut client = isyncyou_graph::GraphClient::new(token);
    let now = unix_now();
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

#[allow(clippy::too_many_arguments)]
fn cmd_backup(
    config: &Path,
    account: &str,
    service: Option<String>,
    body_limit: usize,
    cal_start: &str,
    cal_end: &str,
    token: Option<String>,
) -> Result<(), String> {
    let cfg = load_config(config)?;
    let token = token.ok_or("no access token (pass --token or set ISYNCYOU_TOKEN)")?;
    let acc = cfg
        .accounts
        .iter()
        .find(|a| a.id == account)
        .ok_or_else(|| format!("no account '{account}' in config"))?;
    let archive_root = acc.archive_root.clone();
    let services: Vec<&str> = match &service {
        Some(s) => {
            if !SERVICES.contains(&s.as_str()) {
                return Err(format!(
                    "unknown service '{s}' (expected one of {})",
                    SERVICES.join("|")
                ));
            }
            vec![s.as_str()]
        }
        None => SERVICES.to_vec(),
    };

    std::fs::create_dir_all(&archive_root).map_err(|e| e.to_string())?;
    let store = Store::open(store_path(&cfg, account)?).map_err(|e| e.to_string())?;
    let mut client = isyncyou_graph::GraphClient::new(token);
    let now = unix_now();

    for svc in services {
        // `incremental_sync_*` needs `&mut client` (Transport polling); the body
        // archive needs `&client`. The mutable borrow ends before the shared one.
        let line = match svc {
            "mail" => {
                let r = connectors::incremental_sync_mail(&mut client, &store, account, &now)
                    .map_err(|e| e.to_string())?;
                let b = connectors::backup_message_bodies(
                    &client,
                    &store,
                    account,
                    &archive_root,
                    body_limit,
                )
                .map_err(|e| e.to_string())?;
                format!(
                    "mail: {} folders, {} indexed, {} deleted; {} .eml archived ({} bytes)",
                    r.folders, r.upserted, r.deleted, b.downloaded, b.bytes
                )
            }
            "calendar" => {
                let r = connectors::incremental_sync_calendar(
                    &mut client,
                    &store,
                    account,
                    cal_start,
                    cal_end,
                    &now,
                )
                .map_err(|e| e.to_string())?;
                let b = connectors::backup_calendar_bodies(
                    &client,
                    &store,
                    account,
                    &archive_root,
                    body_limit,
                )
                .map_err(|e| e.to_string())?;
                format!(
                    "calendar: {} calendars, {} indexed; {} json archived ({} bytes)",
                    r.calendars, r.upserted, b.archived, b.bytes
                )
            }
            "contacts" => {
                let r = connectors::incremental_sync_contacts(&mut client, &store, account, &now)
                    .map_err(|e| e.to_string())?;
                let b = connectors::backup_contacts_bodies(
                    &client,
                    &store,
                    account,
                    &archive_root,
                    body_limit,
                )
                .map_err(|e| e.to_string())?;
                format!(
                    "contacts: {} folders, {} indexed; {} json archived ({} bytes)",
                    r.folders, r.upserted, b.archived, b.bytes
                )
            }
            "todo" => {
                let r = connectors::incremental_sync_todo(&mut client, &store, account, &now)
                    .map_err(|e| e.to_string())?;
                let b = connectors::backup_todo_bodies(
                    &client,
                    &store,
                    account,
                    &archive_root,
                    body_limit,
                )
                .map_err(|e| e.to_string())?;
                format!(
                    "todo: {} lists, {} indexed; {} json archived ({} bytes)",
                    r.lists, r.upserted, b.archived, b.bytes
                )
            }
            "onenote" => {
                let r = connectors::incremental_sync_onenote(&mut client, &store, account, &now)
                    .map_err(|e| e.to_string())?;
                let b = connectors::backup_onenote_bodies(
                    &client,
                    &store,
                    account,
                    &archive_root,
                    body_limit,
                )
                .map_err(|e| e.to_string())?;
                format!(
                    "onenote: {} pages, {} indexed, {} deleted; {} html archived ({} bytes)",
                    r.pages, r.upserted, r.deleted, b.archived, b.bytes
                )
            }
            _ => unreachable!("validated against SERVICES"),
        };
        println!("{line}");
    }
    Ok(())
}

/// Run an FTS query against an account's archive index, returning the matches.
fn search_account(
    cfg: &Config,
    account: &str,
    query: &str,
) -> Result<Vec<isyncyou_store::Item>, String> {
    let store = Store::open(store_path(cfg, account)?).map_err(|e| e.to_string())?;
    store
        .search_names(account, query)
        .map_err(|e| e.to_string())
}

fn cmd_search(config: &Path, account: &str, query: &str) -> Result<(), String> {
    let cfg = load_config(config)?;
    let hits = search_account(&cfg, account, query)?;
    if hits.is_empty() {
        println!("no matches for {query:?}");
    } else {
        println!("{} match(es) for {query:?}:", hits.len());
        for h in &hits {
            println!(
                "  [{}/{}] {} ({})",
                h.service, h.item_type, h.name, h.remote_id
            );
        }
    }
    Ok(())
}

fn cmd_restore(
    config: &Path,
    account: &str,
    service: &str,
    id: &str,
    token: Option<String>,
) -> Result<(), String> {
    let cfg = load_config(config)?;
    if !RESTORE_SERVICES.contains(&service) {
        return Err(format!(
            "service '{service}' has no restore path (expected one of {})",
            RESTORE_SERVICES.join("|")
        ));
    }
    let token = token.ok_or("no access token (pass --token or set ISYNCYOU_TOKEN)")?;
    let acc = cfg
        .accounts
        .iter()
        .find(|a| a.id == account)
        .ok_or_else(|| format!("no account '{account}' in config"))?;
    let archive_root = acc.archive_root.clone();
    let store = Store::open(store_path(&cfg, account)?).map_err(|e| e.to_string())?;

    let item = store
        .get_item(account, service, id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no archived {service} item '{id}' for account '{account}'"))?;
    let rel = item
        .local_path
        .as_deref()
        .ok_or_else(|| format!("item '{id}' has no archived body yet (run backup first)"))?;
    let path = archive_root.join(rel);
    let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;

    let client = isyncyou_graph::GraphClient::new(token);
    let new_id = match service {
        "mail" => connectors::restore_message(&client, &bytes)?,
        "calendar" => {
            let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            connectors::restore_event(&client, &v)?
        }
        "contacts" => {
            let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            connectors::restore_contact(&client, &v)?
        }
        "todo" => {
            let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            let list = item
                .parent_remote_id
                .as_deref()
                .ok_or("archived task has no parent list id")?;
            connectors::restore_task(&client, list, &v)?
        }
        _ => unreachable!("validated against RESTORE_SERVICES"),
    };
    println!("restored {service} item '{id}' as '{new_id}'");
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
    fn parses_backup_with_defaults() {
        let c = parse(&["isyncyou", "backup", "--account", "primary", "--token", "T"]).unwrap();
        assert_eq!(
            c.command,
            Command::Backup {
                config: "isyncyou.toml".into(),
                account: "primary".into(),
                service: None,
                body_limit: 0,
                cal_start: "2015-01-01T00:00:00Z".into(),
                cal_end: "2035-01-01T00:00:00Z".into(),
                token: Some("T".into()),
            }
        );
    }

    #[test]
    fn parses_backup_with_service_and_limit() {
        let c = parse(&[
            "isyncyou",
            "backup",
            "--account",
            "a",
            "--service",
            "mail",
            "--body-limit",
            "50",
        ])
        .unwrap();
        match c.command {
            Command::Backup {
                service,
                body_limit,
                ..
            } => {
                assert_eq!(service.as_deref(), Some("mail"));
                assert_eq!(body_limit, 50);
            }
            other => panic!("expected Backup, got {other:?}"),
        }
    }

    #[test]
    fn backup_requires_account() {
        assert!(parse(&["isyncyou", "backup", "--token", "T"]).is_err());
    }

    #[test]
    fn parses_search() {
        let c = parse(&["isyncyou", "search", "--account", "a", "--query", "invoice"]).unwrap();
        assert_eq!(
            c.command,
            Command::Search {
                config: "isyncyou.toml".into(),
                account: "a".into(),
                query: "invoice".into(),
            }
        );
    }

    #[test]
    fn search_account_finds_matching_items() {
        let dir = std::env::temp_dir().join(format!("isyncyou-cli-se-{}", std::process::id()));
        let arch = dir.join("arch");
        std::fs::create_dir_all(&arch).unwrap();
        let p = write_config(&dir, &arch);
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            store
                .upsert_item(&isyncyou_store::Item::new(
                    "a",
                    "mail",
                    "m1",
                    "Invoice for March",
                    "message",
                ))
                .unwrap();
            store
                .upsert_item(&isyncyou_store::Item::new(
                    "a",
                    "calendar",
                    "e1",
                    "Team Standup",
                    "event",
                ))
                .unwrap();
        }
        let cfg = load_config(&p).unwrap();
        let hits = search_account(&cfg, "a", "invoice").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].remote_id, "m1");
        assert!(search_account(&cfg, "a", "nonexistentterm")
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_restore() {
        let c = parse(&[
            "isyncyou",
            "restore",
            "--account",
            "a",
            "--service",
            "mail",
            "--id",
            "M1",
            "--token",
            "T",
        ])
        .unwrap();
        assert_eq!(
            c.command,
            Command::Restore {
                config: "isyncyou.toml".into(),
                account: "a".into(),
                service: "mail".into(),
                id: "M1".into(),
                token: Some("T".into()),
            }
        );
    }

    /// Writes a minimal one-account config whose archive_root is `arch`.
    fn write_config(dir: &std::path::Path, arch: &std::path::Path) -> PathBuf {
        let p = dir.join("c.toml");
        std::fs::write(
            &p,
            format!(
                "[[accounts]]\nid=\"a\"\nusername=\"a@outlook.com\"\nsync_root=\"{}/od\"\narchive_root=\"{}\"\n",
                dir.display(),
                arch.display()
            ),
        )
        .unwrap();
        p
    }

    #[test]
    fn restore_rejects_unknown_service() {
        let dir = std::env::temp_dir().join(format!("isyncyou-cli-rs1-{}", std::process::id()));
        let arch = dir.join("arch");
        std::fs::create_dir_all(&arch).unwrap();
        let p = write_config(&dir, &arch);
        let err = cmd_restore(&p, "a", "onenote", "x", Some("T".into())).unwrap_err();
        assert!(err.contains("no restore path"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_errors_when_item_has_no_archived_body() {
        let dir = std::env::temp_dir().join(format!("isyncyou-cli-rs2-{}", std::process::id()));
        let arch = dir.join("arch");
        std::fs::create_dir_all(&arch).unwrap();
        let p = write_config(&dir, &arch);
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            store
                .upsert_item(&isyncyou_store::Item::new(
                    "a", "calendar", "e1", "Ev", "event",
                ))
                .unwrap();
        } // drop -> release the store lock before cmd_restore reopens it
        let err = cmd_restore(&p, "a", "calendar", "e1", Some("T".into())).unwrap_err();
        assert!(err.contains("no archived body"), "got: {err}");
        // a missing id is reported distinctly
        let err2 = cmd_restore(&p, "a", "calendar", "missing", Some("T".into())).unwrap_err();
        assert!(err2.contains("no archived calendar item"), "got: {err2}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cmd_backup_rejects_unknown_service() {
        // build a minimal valid config with one account
        let dir = std::env::temp_dir().join(format!("isyncyou-cli-bk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("c.toml");
        std::fs::write(
            &p,
            "[[accounts]]\nid=\"a\"\nusername=\"a@outlook.com\"\nsync_root=\"/tmp/od\"\narchive_root=\"/tmp/arch\"\n",
        )
        .unwrap();
        let err =
            cmd_backup(&p, "a", Some("bogus".into()), 0, "s", "e", Some("T".into())).unwrap_err();
        assert!(err.contains("unknown service"));
        let _ = std::fs::remove_dir_all(&dir);
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
