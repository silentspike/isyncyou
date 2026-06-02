//! `isyncyou` — command-line interface for the iSyncYou engine.
//!
//! Wires the config, store and connectors into a usable tool:
//! - `init`    — scaffold a starter config (template or a validated account)
//! - `check`   — validate a config file
//! - `verify`  — check an account's store + archive integrity
//! - `status`  — show tracked-item counts + delta cursor for an account
//! - `sync`    — run one incremental OneDrive sync for an account
//! - `backup`  — index + archive M365 services (mail/calendar/contacts/todo/onenote)
//! - `search`  — full-text search the archive (item names + indexed mail bodies)
//! - `restore` — re-create an archived item in the cloud
//! - `export`  — export archived events/contacts to .ics / .vcf
//! - `migrate` — move an account's archive directory
//! - `serve`   — serve the local web UI
//! - `login`   — device-code sign-in; caches the account token for later commands
//!
//! Token resolution: `--token`/`ISYNCYOU_TOKEN` wins; otherwise the per-account
//! cached token (from `login`) is loaded and auto-refreshed.

use clap::{Parser, Subcommand};
use isyncyou_core::{AccountConfig, Config};
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
    /// Scaffold a starter configuration file.
    Init {
        #[arg(long, default_value = "isyncyou.toml")]
        config: PathBuf,
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        sync_root: Option<PathBuf>,
        #[arg(long)]
        archive_root: Option<PathBuf>,
        /// Overwrite an existing config file.
        #[arg(long)]
        force: bool,
    },
    /// Validate a configuration file.
    Check {
        #[arg(long, default_value = "isyncyou.toml")]
        config: PathBuf,
    },
    /// Check an account's store + archive integrity (schema, missing/empty bodies).
    Verify {
        #[arg(long, default_value = "isyncyou.toml")]
        config: PathBuf,
        #[arg(long)]
        account: String,
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
        /// Account id; omit and pass --all-accounts to back up every account.
        #[arg(long)]
        account: Option<String>,
        /// Back up every configured account (mutually exclusive with --account).
        #[arg(long)]
        all_accounts: bool,
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
    /// Full-text search the local archive (item names/subjects/titles + mail bodies).
    Search {
        #[arg(long, default_value = "isyncyou.toml")]
        config: PathBuf,
        /// Account id; omit and pass --all-accounts to search every account.
        #[arg(long)]
        account: Option<String>,
        /// Search every configured account (mutually exclusive with --account).
        #[arg(long)]
        all_accounts: bool,
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
    /// Export archived items to interchange files (.ics / .vcf).
    Export {
        #[arg(long, default_value = "isyncyou.toml")]
        config: PathBuf,
        #[arg(long)]
        account: String,
        /// calendar (-> .ics) or contacts (-> .vcf).
        #[arg(long)]
        service: String,
        /// Output directory (created if missing).
        #[arg(long)]
        out: PathBuf,
    },
    /// Move an account's archive directory to a new location (no re-download).
    Migrate {
        #[arg(long, default_value = "isyncyou.toml")]
        config: PathBuf,
        #[arg(long)]
        account: String,
        /// Destination archive root (must be empty or not yet exist).
        #[arg(long)]
        new_archive_root: PathBuf,
    },
    /// Serve the local web UI (open the printed URL in your browser).
    Serve {
        #[arg(long, default_value = "isyncyou.toml")]
        config: PathBuf,
        /// Address to bind (localhost only by default).
        #[arg(long, default_value = "127.0.0.1:8765")]
        bind: String,
    },
    /// Sign in (device-code) and cache the account's token for later commands.
    Login {
        #[arg(long, default_value = "isyncyou.toml")]
        config: PathBuf,
        #[arg(long)]
        account: String,
        /// Sign in for write operations (restore) instead of read-only backup.
        #[arg(long)]
        write: bool,
    },
}

/// The M365 backup services this CLI knows how to drive.
const SERVICES: &[&str] = &["mail", "calendar", "contacts", "todo", "onenote"];
/// Services with a restore path (OneNote pages can't be re-created via a simple POST).
const RESTORE_SERVICES: &[&str] = &["mail", "calendar", "contacts", "todo"];

// Public client app registrations + scopes for the test/personal accounts.
const READ_CLIENT: &str = "cee80dd9-c13e-4dbb-9d4c-73eb4987d447";
const WRITE_CLIENT: &str = "a90d9140-3a62-46d0-907b-f2b7b61a573a";
const READ_SCOPES: &[&str] = &[
    "Files.Read",
    "Mail.Read",
    "Calendars.Read",
    "Contacts.Read",
    "Tasks.Read",
    "Notes.Read",
    "offline_access",
];
const WRITE_SCOPES: &[&str] = &[
    "Files.ReadWrite",
    "Mail.ReadWrite",
    "Mail.Send",
    "Calendars.ReadWrite",
    "Contacts.ReadWrite",
    "Tasks.ReadWrite",
    "offline_access",
];
const READ_CACHE: &str = ".isyncyou-token-read.json";
const WRITE_CACHE: &str = ".isyncyou-token-write.json";

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli.command) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(command: Command) -> Result<(), String> {
    match command {
        Command::Init {
            config,
            account,
            username,
            sync_root,
            archive_root,
            force,
        } => cmd_init(&config, account, username, sync_root, archive_root, force),
        Command::Check { config } => cmd_check(&config),
        Command::Verify { config, account } => cmd_verify(&config, &account),
        Command::Status { config, account } => cmd_status(&config, &account),
        Command::Sync {
            config,
            account,
            token,
        } => cmd_sync(&config, &account, token),
        Command::Backup {
            config,
            account,
            all_accounts,
            service,
            body_limit,
            cal_start,
            cal_end,
            token,
        } => cmd_backup(
            &config,
            account,
            all_accounts,
            service,
            body_limit,
            &cal_start,
            &cal_end,
            token,
        ),
        Command::Search {
            config,
            account,
            all_accounts,
            query,
        } => cmd_search(&config, account, all_accounts, &query),
        Command::Restore {
            config,
            account,
            service,
            id,
            token,
        } => cmd_restore(&config, &account, &service, &id, token),
        Command::Export {
            config,
            account,
            service,
            out,
        } => cmd_export(&config, &account, &service, &out),
        Command::Migrate {
            config,
            account,
            new_archive_root,
        } => cmd_migrate(&config, &account, &new_archive_root),
        Command::Serve { config, bind } => cmd_serve(&config, &bind),
        Command::Login {
            config,
            account,
            write,
        } => cmd_login(&config, &account, write),
    }
}

/// Account's token-cache path for the read or write app.
fn token_cache_path(cfg: &Config, account: &str, write: bool) -> Result<PathBuf, String> {
    let acc = cfg
        .accounts
        .iter()
        .find(|a| a.id == account)
        .ok_or_else(|| format!("no account '{account}' in config"))?;
    Ok(acc
        .archive_root
        .join(if write { WRITE_CACHE } else { READ_CACHE }))
}

/// Resolve an access token: an explicit `--token`/`ISYNCYOU_TOKEN` wins; otherwise
/// the per-account cached token is loaded and auto-refreshed (run `login` first).
fn resolve_token(
    cfg: &Config,
    account: &str,
    token: Option<String>,
    write: bool,
) -> Result<String, String> {
    if let Some(t) = token {
        return Ok(t);
    }
    let cache = token_cache_path(cfg, account, write)?;
    if !cache.exists() {
        let kind = if write { " --write" } else { "" };
        return Err(format!(
            "no access token: pass --token / set ISYNCYOU_TOKEN, or run `isyncyou login --account {account}{kind}`"
        ));
    }
    let (client, scopes) = if write {
        (WRITE_CLIENT, WRITE_SCOPES)
    } else {
        (READ_CLIENT, READ_SCOPES)
    };
    let now = unix_now().parse::<u64>().unwrap_or(0);
    isyncyou_graph::auth::flow::ensure_access_token(&cache, client, scopes, now)
}

fn cmd_login(config: &Path, account: &str, write: bool) -> Result<(), String> {
    let cfg = load_config(config)?;
    let cache = token_cache_path(&cfg, account, write)?;
    if let Some(parent) = cache.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let (client, scopes) = if write {
        (WRITE_CLIENT, WRITE_SCOPES)
    } else {
        (READ_CLIENT, READ_SCOPES)
    };
    let now = unix_now().parse::<u64>().unwrap_or(0);
    let tokens = isyncyou_graph::auth::flow::device_code_login(client, scopes, now, |dc| {
        eprintln!(
            "To sign in, open {} and enter code: {}",
            dc.verification_uri, dc.user_code
        );
        eprintln!("{}", dc.message);
    })?;
    tokens.save(&cache).map_err(|e| e.to_string())?;
    println!(
        "login OK for '{account}' ({}); token cached at {}",
        if write { "write" } else { "read" },
        cache.display()
    );
    Ok(())
}

fn cmd_serve(config: &Path, bind: &str) -> Result<(), String> {
    let cfg = load_config(config)?;
    let router = isyncyou_webui::Router::new(cfg);
    isyncyou_webui::serve(bind, router).map_err(|e| format!("serve: {e}"))
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

/// Documented starter template (shared with the release archive's sample).
const CONFIG_TEMPLATE: &str = include_str!("../../../packaging/isyncyou.toml.sample");

fn cmd_init(
    config: &Path,
    account: Option<String>,
    username: Option<String>,
    sync_root: Option<PathBuf>,
    archive_root: Option<PathBuf>,
    force: bool,
) -> Result<(), String> {
    if config.exists() && !force {
        return Err(format!(
            "{} already exists (use --force to overwrite)",
            config.display()
        ));
    }
    match (account, username, sync_root, archive_root) {
        (Some(id), Some(user), Some(sr), Some(ar)) => {
            let cfg = Config {
                accounts: vec![AccountConfig {
                    id,
                    username: user,
                    sync_root: sr,
                    archive_root: ar,
                }],
                ..Default::default()
            };
            cfg.validate()
                .map_err(|errs| format!("invalid config: {}", errs.join("; ")))?;
            cfg.save(config)?;
            println!(
                "wrote config for account '{}' to {}",
                cfg.accounts[0].id,
                config.display()
            );
        }
        _ => {
            std::fs::write(config, CONFIG_TEMPLATE).map_err(|e| e.to_string())?;
            println!(
                "wrote a starter template to {} — edit it, then run `isyncyou check`",
                config.display()
            );
        }
    }
    Ok(())
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

/// All services a status report covers.
const STATUS_SERVICES: &[&str] = &[
    "onedrive", "mail", "calendar", "contacts", "todo", "onenote", "shared",
];

/// Per-service counts for an account's archive.
struct AccountStatus {
    /// `(service, item_count, with_archived_body)` for non-empty services.
    services: Vec<(String, usize, usize)>,
    onedrive_cursor: bool,
}

fn account_status(cfg: &Config, account: &str) -> Result<AccountStatus, String> {
    let store = Store::open(store_path(cfg, account)?).map_err(|e| e.to_string())?;
    let mut services = Vec::new();
    for &svc in STATUS_SERVICES {
        let items = store
            .items_by_service(account, svc)
            .map_err(|e| e.to_string())?;
        if !items.is_empty() {
            let archived = items.iter().filter(|i| i.local_path.is_some()).count();
            services.push((svc.to_string(), items.len(), archived));
        }
    }
    let onedrive_cursor = store
        .get_delta_cursor(account, "onedrive", "")
        .map_err(|e| e.to_string())?
        .is_some();
    Ok(AccountStatus {
        services,
        onedrive_cursor,
    })
}

/// Outcome of a store/archive integrity check.
struct VerifyReport {
    schema_ok: bool,
    items: usize,
    with_body: usize,
    missing_body: usize,
    empty_body: usize,
}

impl VerifyReport {
    fn healthy(&self) -> bool {
        self.schema_ok && self.missing_body == 0 && self.empty_body == 0
    }
}

fn verify_account(cfg: &Config, account: &str) -> Result<VerifyReport, String> {
    let acc = cfg
        .accounts
        .iter()
        .find(|a| a.id == account)
        .ok_or_else(|| format!("no account '{account}' in config"))?;
    let archive_root = acc.archive_root.clone();
    let store = Store::open(store_path(cfg, account)?).map_err(|e| e.to_string())?;
    let schema_ok =
        store.schema_version().map_err(|e| e.to_string())? == isyncyou_store::SCHEMA_VERSION;
    let mut r = VerifyReport {
        schema_ok,
        items: 0,
        with_body: 0,
        missing_body: 0,
        empty_body: 0,
    };
    for &svc in STATUS_SERVICES {
        for it in store
            .items_by_service(account, svc)
            .map_err(|e| e.to_string())?
        {
            r.items += 1;
            if let Some(rel) = it.local_path {
                r.with_body += 1;
                match std::fs::metadata(archive_root.join(&rel)) {
                    Ok(m) if m.len() == 0 => r.empty_body += 1,
                    Ok(_) => {}
                    Err(_) => r.missing_body += 1,
                }
            }
        }
    }
    Ok(r)
}

fn cmd_verify(config: &Path, account: &str) -> Result<(), String> {
    let cfg = load_config(config)?;
    let r = verify_account(&cfg, account)?;
    println!("account: {account}");
    println!("  schema: {}", if r.schema_ok { "ok" } else { "OUTDATED" });
    println!(
        "  {} item(s), {} with archived body; {} missing, {} empty",
        r.items, r.with_body, r.missing_body, r.empty_body
    );
    if r.healthy() {
        println!("verify OK");
        Ok(())
    } else {
        Err(format!(
            "integrity problems: {} missing + {} empty body file(s){}",
            r.missing_body,
            r.empty_body,
            if r.schema_ok { "" } else { ", schema outdated" }
        ))
    }
}

fn cmd_status(config: &Path, account: &str) -> Result<(), String> {
    let cfg = load_config(config)?;
    let st = account_status(&cfg, account)?;
    println!("account: {account}");
    if st.services.is_empty() {
        println!("  (nothing tracked yet — run `isyncyou backup`)");
    } else {
        for (svc, items, archived) in &st.services {
            println!("  {svc}: {items} item(s), {archived} with archived body");
        }
    }
    println!(
        "onedrive delta cursor: {}",
        if st.onedrive_cursor {
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
    let token = resolve_token(&cfg, account, token, false)?;
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

    // Materialize the ingested changes to disk: write new/changed files into the
    // account's sync root (the remote→local half that makes files actually appear).
    let sync_root = cfg
        .accounts
        .iter()
        .find(|a| a.id == account)
        .map(|a| a.sync_root.clone())
        .ok_or_else(|| format!("no account '{account}' in config"))?;
    let mat = connectors::materialize_downloads(&store, &client, account, &sync_root)
        .map_err(|e| e.to_string())?;
    println!(
        "materialized: {} downloaded, {} folders, {} failed -> {}",
        mat.downloaded,
        mat.dirs_created,
        mat.failed,
        sync_root.display()
    );
    Ok(())
}

/// Resolve which accounts a run targets (shared by backup + search).
fn select_accounts(
    cfg: &Config,
    account: Option<&str>,
    all_accounts: bool,
) -> Result<Vec<String>, String> {
    match (all_accounts, account) {
        (true, Some(_)) => Err("use either --account or --all-accounts, not both".into()),
        (true, None) => {
            if cfg.accounts.is_empty() {
                Err("no accounts configured".into())
            } else {
                Ok(cfg.accounts.iter().map(|a| a.id.clone()).collect())
            }
        }
        (false, Some(a)) => Ok(vec![a.to_string()]),
        (false, None) => Err("specify --account <id> or --all-accounts".into()),
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_backup(
    config: &Path,
    account: Option<String>,
    all_accounts: bool,
    service: Option<String>,
    body_limit: usize,
    cal_start: &str,
    cal_end: &str,
    token: Option<String>,
) -> Result<(), String> {
    let cfg = load_config(config)?;
    // Validate the service filter once, up front (a bad --service is a user error).
    if let Some(s) = &service {
        if !SERVICES.contains(&s.as_str()) {
            return Err(format!(
                "unknown service '{s}' (expected one of {})",
                SERVICES.join("|")
            ));
        }
    }
    let targets = select_accounts(&cfg, account.as_deref(), all_accounts)?;
    let multi = targets.len() > 1;
    let mut failures = Vec::new();
    for acc in &targets {
        if multi {
            println!("== account {acc} ==");
        }
        if let Err(e) = backup_one_account(
            &cfg,
            acc,
            service.clone(),
            body_limit,
            cal_start,
            cal_end,
            token.clone(),
        ) {
            eprintln!("account {acc}: error: {e}");
            failures.push(acc.clone());
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "backup failed for {} account(s): {}",
            failures.len(),
            failures.join(", ")
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn backup_one_account(
    cfg: &Config,
    account: &str,
    service: Option<String>,
    body_limit: usize,
    cal_start: &str,
    cal_end: &str,
    token: Option<String>,
) -> Result<(), String> {
    let token = resolve_token(cfg, account, token, false)?;
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
    let store = Store::open(store_path(cfg, account)?).map_err(|e| e.to_string())?;
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
                // Index body text for full-text search when enabled (privacy/space opt-in).
                let indexed = if cfg.sync.body_index {
                    connectors::index_mail_bodies(&store, account, &archive_root, 0)
                        .map_err(|e| e.to_string())?
                } else {
                    0
                };
                format!(
                    "mail: {} folders, {} indexed, {} deleted; {} .eml archived ({} bytes); {} bodies FTS-indexed",
                    r.folders, r.upserted, r.deleted, b.downloaded, b.bytes, indexed
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
                let ph = connectors::backup_contact_photos(
                    &client,
                    &store,
                    account,
                    &archive_root,
                    body_limit,
                )
                .map_err(|e| e.to_string())?;
                format!(
                    "contacts: {} folders, {} indexed; {} json archived ({} bytes); {} photos ({} without)",
                    r.folders, r.upserted, b.archived, b.bytes, ph.downloaded, ph.skipped
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
    // names (subjects/titles/filenames) ...
    let mut hits = store
        .search_names(account, query)
        .map_err(|e| e.to_string())?;
    let mut seen: std::collections::HashSet<(String, String)> = hits
        .iter()
        .map(|i| (i.service.clone(), i.remote_id.clone()))
        .collect();
    // ... merged with indexed bodies (e.g. mail text), de-duplicated.
    for (service, remote_id) in store
        .search_bodies(account, query)
        .map_err(|e| e.to_string())?
    {
        if seen.insert((service.clone(), remote_id.clone())) {
            if let Some(it) = store
                .get_item(account, &service, &remote_id)
                .map_err(|e| e.to_string())?
            {
                hits.push(it);
            }
        }
    }
    Ok(hits)
}

fn cmd_search(
    config: &Path,
    account: Option<String>,
    all_accounts: bool,
    query: &str,
) -> Result<(), String> {
    let cfg = load_config(config)?;
    let targets = select_accounts(&cfg, account.as_deref(), all_accounts)?;
    let multi = targets.len() > 1;
    let mut total = 0usize;
    for acc in &targets {
        match search_account(&cfg, acc, query) {
            Ok(hits) => {
                if !hits.is_empty() {
                    if multi {
                        println!("== account {acc} ==");
                    }
                    for h in &hits {
                        println!(
                            "  [{}/{}] {} ({})",
                            h.service, h.item_type, h.name, h.remote_id
                        );
                    }
                    total += hits.len();
                }
            }
            // an account that was never backed up (no store) is just skipped
            Err(e) => eprintln!("account {acc}: not searchable: {e}"),
        }
    }
    if total == 0 {
        println!("no matches for {query:?}");
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
    let token = resolve_token(&cfg, account, token, true)?;
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

/// Recursively copy `src` into `dst` (used as the cross-filesystem fallback).
fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let to = dst.join(entry.file_name());
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Move `old` to `new`: a same-filesystem rename, else copy + remove.
fn move_dir(old: &Path, new: &Path) -> Result<(), String> {
    if let Some(parent) = new.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // An empty destination dir is fine; remove it so rename can take the name.
    if new.exists() {
        std::fs::remove_dir(new).map_err(|e| {
            format!(
                "destination {} must be an empty directory: {e}",
                new.display()
            )
        })?;
    }
    if std::fs::rename(old, new).is_ok() {
        return Ok(());
    }
    // Cross-device or other rename failure: copy then remove the original.
    copy_dir_all(old, new)?;
    std::fs::remove_dir_all(old).map_err(|e| e.to_string())
}

/// Filesystem-safe filename from a Graph id (which may contain `/`, `=`, …).
fn safe_filename(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn cmd_export(config: &Path, account: &str, service: &str, out: &Path) -> Result<(), String> {
    let cfg = load_config(config)?;
    let acc = cfg
        .accounts
        .iter()
        .find(|a| a.id == account)
        .ok_or_else(|| format!("no account '{account}' in config"))?;
    let archive_root = acc.archive_root.clone();
    let (item_type, ext): (&str, &str) = match service {
        "calendar" => ("event", "ics"),
        "contacts" => ("contact", "vcf"),
        other => return Err(format!("export supports calendar|contacts, not '{other}'")),
    };
    let convert: fn(&serde_json::Value) -> String = match service {
        "calendar" => connectors::event_to_ics,
        "contacts" => connectors::contact_to_vcard,
        _ => unreachable!(),
    };

    let store = Store::open(store_path(&cfg, account)?).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let mut n = 0usize;
    let mut skipped = 0usize;
    for item in store
        .items_by_type(account, service, item_type)
        .map_err(|e| e.to_string())?
    {
        let rel = match item.local_path.as_deref() {
            Some(p) => p,
            None => {
                skipped += 1;
                continue;
            }
        };
        let bytes = std::fs::read(archive_root.join(rel)).map_err(|e| e.to_string())?;
        let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        let text = convert(&v);
        let fname = format!("{}.{ext}", safe_filename(&item.remote_id));
        std::fs::write(out.join(fname), text).map_err(|e| e.to_string())?;
        n += 1;
    }
    println!(
        "exported {n} {service} item(s) to {} ({skipped} without an archived body skipped)",
        out.display()
    );
    Ok(())
}

fn cmd_migrate(config: &Path, account: &str, new_root: &Path) -> Result<(), String> {
    let mut cfg = load_config(config)?;
    let old = cfg
        .accounts
        .iter()
        .find(|a| a.id == account)
        .ok_or_else(|| format!("no account '{account}' in config"))?
        .archive_root
        .clone();
    let new = new_root.to_path_buf();

    if new == old {
        return Err("new archive root equals the current one".into());
    }
    if new.starts_with(&old) {
        return Err("new archive root must not be inside the current one".into());
    }
    if !old.exists() {
        return Err(format!(
            "current archive root {} does not exist",
            old.display()
        ));
    }
    if new.exists() && new.read_dir().map_err(|e| e.to_string())?.next().is_some() {
        return Err(format!("destination {} is not empty", new.display()));
    }

    move_dir(&old, &new)?;

    // local_path is relative to archive_root, so only the config needs updating.
    for a in &mut cfg.accounts {
        if a.id == account {
            a.archive_root = new.clone();
        }
    }
    cfg.save(config)?;
    println!(
        "migrated account '{account}' archive: {} -> {}",
        old.display(),
        new.display()
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
    fn init_writes_validated_account_config() {
        let dir = std::env::temp_dir().join(format!("isyncyou-cli-init-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("isyncyou.toml");
        cmd_init(
            &p,
            Some("a".into()),
            Some("a@outlook.com".into()),
            Some(dir.join("od")),
            Some(dir.join("arch")),
            false,
        )
        .unwrap();
        let cfg = load_config(&p).unwrap(); // parses + validates
        assert_eq!(cfg.accounts[0].id, "a");
        // refuses overwrite without --force, allows with
        assert!(cmd_init(&p, None, None, None, None, false)
            .unwrap_err()
            .contains("already exists"));
        cmd_init(&p, None, None, None, None, true).unwrap(); // template, forced
                                                             // a template-initialised config is itself valid
        load_config(&p).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn init_rejects_invalid_account() {
        let dir = std::env::temp_dir().join(format!("isyncyou-cli-init2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("isyncyou.toml");
        // sync_root == archive_root is invalid -> nothing is written
        let err = cmd_init(
            &p,
            Some("a".into()),
            Some("a@o".into()),
            Some(dir.join("same")),
            Some(dir.join("same")),
            false,
        )
        .unwrap_err();
        assert!(err.contains("invalid config"), "got: {err}");
        assert!(!p.exists(), "invalid config must not be written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_account_flags_missing_and_empty_bodies() {
        let dir = std::env::temp_dir().join(format!("isyncyou-cli-vf-{}", std::process::id()));
        let arch = dir.join("arch");
        std::fs::create_dir_all(arch.join("mail/aa/bb")).unwrap();
        let p = write_config(&dir, &arch);
        // a good body file
        std::fs::write(arch.join("mail/aa/bb/ok.eml"), b"From: a\r\n\r\nhi").unwrap();
        std::fs::write(arch.join("mail/aa/bb/empty.eml"), b"").unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut ok = isyncyou_store::Item::new("a", "mail", "ok", "OK", "message");
            ok.local_path = Some("mail/aa/bb/ok.eml".into());
            store.upsert_item(&ok).unwrap();
            let mut miss = isyncyou_store::Item::new("a", "mail", "miss", "Missing", "message");
            miss.local_path = Some("mail/aa/bb/gone.eml".into()); // file does not exist
            store.upsert_item(&miss).unwrap();
            let mut emp = isyncyou_store::Item::new("a", "mail", "emp", "Empty", "message");
            emp.local_path = Some("mail/aa/bb/empty.eml".into());
            store.upsert_item(&emp).unwrap();
            // an item without a body is fine
            store
                .upsert_item(&isyncyou_store::Item::new(
                    "a", "mail", "nob", "NoBody", "message",
                ))
                .unwrap();
        }
        let cfg = load_config(&p).unwrap();
        let r = verify_account(&cfg, "a").unwrap();
        assert!(r.schema_ok);
        assert_eq!(r.items, 4);
        assert_eq!(r.with_body, 3);
        assert_eq!(r.missing_body, 1);
        assert_eq!(r.empty_body, 1);
        assert!(!r.healthy());
        // cmd_verify surfaces the problem as an error
        assert!(cmd_verify(&p, "a").unwrap_err().contains("missing"));
        let _ = std::fs::remove_dir_all(&dir);
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
    fn account_status_counts_per_service() {
        let dir = std::env::temp_dir().join(format!("isyncyou-cli-st-{}", std::process::id()));
        let arch = dir.join("arch");
        std::fs::create_dir_all(&arch).unwrap();
        let p = write_config(&dir, &arch);
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut m1 = isyncyou_store::Item::new("a", "mail", "m1", "Hi", "message");
            m1.local_path = Some("mail/aa/bb/x.eml".into());
            store.upsert_item(&m1).unwrap();
            store
                .upsert_item(&isyncyou_store::Item::new(
                    "a", "mail", "m2", "Yo", "message",
                ))
                .unwrap();
            store
                .upsert_item(&isyncyou_store::Item::new(
                    "a", "calendar", "e1", "Ev", "event",
                ))
                .unwrap();
            store.set_delta_cursor("a", "onedrive", "", "CUR").unwrap();
        }
        let cfg = load_config(&p).unwrap();
        let st = account_status(&cfg, "a").unwrap();
        let mail = st.services.iter().find(|(s, ..)| s == "mail").unwrap();
        assert_eq!((mail.1, mail.2), (2, 1)); // 2 messages, 1 archived
        assert!(st
            .services
            .iter()
            .any(|(s, n, _)| s == "calendar" && *n == 1));
        // empty services are omitted
        assert!(!st.services.iter().any(|(s, ..)| s == "todo"));
        assert!(st.onedrive_cursor);
        let _ = std::fs::remove_dir_all(&dir);
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
                account: Some("primary".into()),
                all_accounts: false,
                service: None,
                body_limit: 0,
                cal_start: "2015-01-01T00:00:00Z".into(),
                cal_end: "2035-01-01T00:00:00Z".into(),
                token: Some("T".into()),
            }
        );
    }

    #[test]
    fn select_accounts_resolution() {
        let mut cfg = Config::default();
        cfg.accounts.push(AccountConfig {
            id: "a".into(),
            username: "a@o".into(),
            sync_root: "/a/od".into(),
            archive_root: "/a/ar".into(),
        });
        cfg.accounts.push(AccountConfig {
            id: "b".into(),
            username: "b@o".into(),
            sync_root: "/b/od".into(),
            archive_root: "/b/ar".into(),
        });
        // single account
        assert_eq!(select_accounts(&cfg, Some("a"), false).unwrap(), vec!["a"]);
        // all accounts
        assert_eq!(
            select_accounts(&cfg, None, true).unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        // both -> error; neither -> error
        assert!(select_accounts(&cfg, Some("a"), true)
            .unwrap_err()
            .contains("not both"));
        assert!(select_accounts(&cfg, None, false)
            .unwrap_err()
            .contains("--account"));
        // all-accounts with empty config -> error
        assert!(select_accounts(&Config::default(), None, true)
            .unwrap_err()
            .contains("no accounts"));
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
    fn backup_without_account_or_all_errors_at_runtime() {
        // parses (account is optional) but cmd_backup requires a target selector
        let dir = std::env::temp_dir().join(format!("isyncyou-cli-bkreq-{}", std::process::id()));
        let arch = dir.join("arch");
        std::fs::create_dir_all(&arch).unwrap();
        let p = write_config(&dir, &arch);
        let err = cmd_backup(&p, None, false, None, 0, "s", "e", Some("T".into())).unwrap_err();
        assert!(
            err.contains("--account") && err.contains("--all-accounts"),
            "got: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_search() {
        let c = parse(&["isyncyou", "search", "--account", "a", "--query", "invoice"]).unwrap();
        assert_eq!(
            c.command,
            Command::Search {
                config: "isyncyou.toml".into(),
                account: Some("a".into()),
                all_accounts: false,
                query: "invoice".into(),
            }
        );
    }

    #[test]
    fn cmd_search_across_accounts() {
        // two accounts, each with a distinct item; --all-accounts searches both.
        let dir = std::env::temp_dir().join(format!("isyncyou-cli-xacc-{}", std::process::id()));
        let a1 = dir.join("a1");
        let a2 = dir.join("a2");
        std::fs::create_dir_all(&a1).unwrap();
        std::fs::create_dir_all(&a2).unwrap();
        let p = dir.join("c.toml");
        std::fs::write(
            &p,
            format!(
                "[[accounts]]\nid=\"one\"\nusername=\"one@o\"\nsync_root=\"{d}/o1\"\narchive_root=\"{a1}\"\n\
                 [[accounts]]\nid=\"two\"\nusername=\"two@o\"\nsync_root=\"{d}/o2\"\narchive_root=\"{a2}\"\n",
                d = dir.display(),
                a1 = a1.display(),
                a2 = a2.display(),
            ),
        )
        .unwrap();
        {
            let s1 = Store::open(a1.join(".isyncyou-store.db")).unwrap();
            s1.upsert_item(&isyncyou_store::Item::new(
                "one",
                "mail",
                "m1",
                "Invoice for one",
                "message",
            ))
            .unwrap();
            let s2 = Store::open(a2.join(".isyncyou-store.db")).unwrap();
            s2.upsert_item(&isyncyou_store::Item::new(
                "two",
                "mail",
                "m9",
                "Invoice for two",
                "message",
            ))
            .unwrap();
        }
        let cfg = load_config(&p).unwrap();
        // each account finds only its own
        assert_eq!(search_account(&cfg, "one", "invoice").unwrap().len(), 1);
        assert_eq!(search_account(&cfg, "two", "invoice").unwrap().len(), 1);
        // the selector resolves both for --all-accounts (the cross-account run)
        assert_eq!(
            select_accounts(&cfg, None, true).unwrap(),
            vec!["one".to_string(), "two".to_string()]
        );
        // cmd_search over all accounts completes without error
        cmd_search(&p, None, true, "invoice").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
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
    fn parses_login() {
        let c = parse(&["isyncyou", "login", "--account", "a", "--write"]).unwrap();
        assert_eq!(
            c.command,
            Command::Login {
                config: "isyncyou.toml".into(),
                account: "a".into(),
                write: true,
            }
        );
        let c2 = parse(&["isyncyou", "login", "--account", "a"]).unwrap();
        match c2.command {
            Command::Login { write, .. } => assert!(!write),
            other => panic!("expected Login, got {other:?}"),
        }
    }

    #[test]
    fn resolve_token_prefers_explicit_else_requires_login() {
        let dir = std::env::temp_dir().join(format!("isyncyou-cli-tok-{}", std::process::id()));
        let arch = dir.join("arch");
        std::fs::create_dir_all(&arch).unwrap();
        let p = write_config(&dir, &arch);
        let cfg = load_config(&p).unwrap();
        // explicit token wins, no cache needed
        assert_eq!(
            resolve_token(&cfg, "a", Some("TOK".into()), false).unwrap(),
            "TOK"
        );
        // no token + no cached login -> a clear error pointing at `login`
        let err = resolve_token(&cfg, "a", None, false).unwrap_err();
        assert!(err.contains("isyncyou login"), "got: {err}");
        // write variant points at --write
        let werr = resolve_token(&cfg, "a", None, true).unwrap_err();
        assert!(werr.contains("--write"), "got: {werr}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_account_includes_body_hits() {
        let dir = std::env::temp_dir().join(format!("isyncyou-cli-sb-{}", std::process::id()));
        let arch = dir.join("arch");
        std::fs::create_dir_all(&arch).unwrap();
        let p = write_config(&dir, &arch);
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            store
                .upsert_item(&isyncyou_store::Item::new(
                    "a", "mail", "m1", "Receipt", "message",
                ))
                .unwrap();
            store
                .index_body("a", "mail", "m1", "the warranty covers two years")
                .unwrap();
        }
        let cfg = load_config(&p).unwrap();
        // a term only in the body (not the name) is found via the body index
        let hits = search_account(&cfg, "a", "warranty").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].remote_id, "m1");
        // a name term still works, and results are not duplicated
        assert_eq!(search_account(&cfg, "a", "receipt").unwrap().len(), 1);
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
    fn cmd_export_writes_ics_from_archived_event() {
        let dir = std::env::temp_dir().join(format!("isyncyou-cli-exp-{}", std::process::id()));
        let arch = dir.join("arch");
        let out = dir.join("out");
        std::fs::create_dir_all(arch.join("calendar/aa/bb")).unwrap();
        let p = write_config(&dir, &arch);
        std::fs::write(
            arch.join("calendar/aa/bb/e.json"),
            br#"{"id":"E1","iCalUId":"UID1","subject":"Standup","start":{"dateTime":"2026-03-01T09:00:00"},"end":{"dateTime":"2026-03-01T09:15:00"}}"#,
        )
        .unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let mut e = isyncyou_store::Item::new("a", "calendar", "E1", "Standup", "event");
            e.local_path = Some("calendar/aa/bb/e.json".into());
            store.upsert_item(&e).unwrap();
            // an event without a body is skipped (not exported)
            store
                .upsert_item(&isyncyou_store::Item::new(
                    "a", "calendar", "E2", "NoBody", "event",
                ))
                .unwrap();
        }
        cmd_export(&p, "a", "calendar", &out).unwrap();
        let ics = std::fs::read_to_string(out.join("E1.ics")).unwrap();
        assert!(ics.contains("BEGIN:VEVENT") && ics.contains("UID:UID1"));
        assert!(ics.contains("DTSTART:20260301T090000"));
        // only the archived event was written
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 1);
        // unsupported service is rejected
        assert!(cmd_export(&p, "a", "mail", &out)
            .unwrap_err()
            .contains("supports calendar|contacts"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_migrate() {
        let c = parse(&[
            "isyncyou",
            "migrate",
            "--account",
            "a",
            "--new-archive-root",
            "/data/new",
        ])
        .unwrap();
        assert_eq!(
            c.command,
            Command::Migrate {
                config: "isyncyou.toml".into(),
                account: "a".into(),
                new_archive_root: "/data/new".into(),
            }
        );
    }

    #[test]
    fn migrate_moves_archive_and_updates_config() {
        let dir = std::env::temp_dir().join(format!("isyncyou-cli-mig-a-{}", std::process::id()));
        let old = dir.join("old");
        let new = dir.join("new");
        std::fs::create_dir_all(&old).unwrap();
        let p = write_config(&dir, &old);
        {
            let store = Store::open(old.join(".isyncyou-store.db")).unwrap();
            let mut it = isyncyou_store::Item::new("a", "mail", "m1", "Hi", "message");
            it.local_path = Some("mail/aa/bb/x.eml".into());
            store.upsert_item(&it).unwrap();
        }
        let body = old.join("mail/aa/bb");
        std::fs::create_dir_all(&body).unwrap();
        std::fs::write(body.join("x.eml"), b"From: a\r\n").unwrap();

        cmd_migrate(&p, "a", &new).unwrap();

        assert!(!old.exists(), "old archive removed");
        assert!(new.join(".isyncyou-store.db").exists());
        assert!(new.join("mail/aa/bb/x.eml").exists());
        // config now points at the new root
        let cfg = load_config(&p).unwrap();
        assert_eq!(cfg.accounts[0].archive_root, new);
        // store reopens at the new root and the relative body still resolves
        let store = Store::open(new.join(".isyncyou-store.db")).unwrap();
        let it = store.get_item("a", "mail", "m1").unwrap().unwrap();
        assert!(new.join(it.local_path.unwrap()).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_rejects_bad_targets() {
        let dir = std::env::temp_dir().join(format!("isyncyou-cli-mig-b-{}", std::process::id()));
        let old = dir.join("old");
        std::fs::create_dir_all(&old).unwrap();
        let p = write_config(&dir, &old);
        assert!(cmd_migrate(&p, "a", &old).unwrap_err().contains("equals"));
        assert!(cmd_migrate(&p, "a", &old.join("sub"))
            .unwrap_err()
            .contains("inside"));
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("f"), b"x").unwrap();
        assert!(cmd_migrate(&p, "a", &other)
            .unwrap_err()
            .contains("not empty"));
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
        let err = cmd_backup(
            &p,
            Some("a".into()),
            false,
            Some("bogus".into()),
            0,
            "s",
            "e",
            Some("T".into()),
        )
        .unwrap_err();
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
