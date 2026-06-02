//! `isyncyoud` — the iSyncYou engine daemon.
//!
//! The long-running service (systemd-user / system). It serves the local web
//! UI + JSON API (via [`isyncyou_webui`]), logs a liveness heartbeat, and — when
//! `--sync-secs` is set — runs a **scheduled background bidirectional sync** for
//! every configured account using the cached `login --write` token (refreshed
//! silently). The sync thread and the web UI share a store-access gate so the
//! per-request `Store::open` never races the single-instance lock the sync pass
//! holds. With no cached token an account is simply skipped (logged), never
//! blocked.

use clap::Parser;
use isyncyou_core::Config;
use isyncyou_pathmap::MappingTable;
use isyncyou_store::Store;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Parser, Debug, PartialEq, Eq)]
#[command(name = "isyncyoud", version, about = "iSyncYou engine daemon")]
struct Args {
    /// Configuration file.
    #[arg(long, default_value = "isyncyou.toml")]
    config: PathBuf,
    /// Address to bind the local web UI/API (localhost only by default).
    #[arg(long, default_value = "127.0.0.1:8765")]
    bind: String,
    /// Serve on a Unix-domain socket instead of TCP (owner-only, mode 0600 — the
    /// desktop default per plan §11). When set, --bind is ignored.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Liveness heartbeat interval in seconds (0 disables).
    #[arg(long, default_value_t = 300)]
    heartbeat_secs: u64,
    /// Run a background bidirectional sync for every account this often, in
    /// seconds (0 = off; the daemon only serves the UI).
    #[arg(long, default_value_t = 0)]
    sync_secs: u64,
}

fn main() {
    let args = Args::parse();
    if let Err(e) = run(&args) {
        eprintln!("isyncyoud: error: {e}");
        std::process::exit(1);
    }
}

fn run(args: &Args) -> Result<(), String> {
    let cfg = load_config(&args.config)?;
    let n = cfg.accounts.len();
    let where_ = match &args.socket {
        Some(p) => format!("unix:{}", p.display()),
        None => format!("http://{}/", args.bind),
    };
    eprintln!("isyncyoud: {n} account(s) configured; serving web UI on {where_}");

    if args.heartbeat_secs > 0 {
        let (where_, secs) = (where_.clone(), args.heartbeat_secs);
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(secs));
            eprintln!("isyncyoud: alive, {n} account(s), web UI on {where_}");
        });
    }

    // The store-access gate is shared by the web UI and the sync thread so only
    // one of them ever holds a store's single-instance file lock at a time.
    let gate = Arc::new(Mutex::new(()));

    let router = if args.sync_secs > 0 {
        let (cfg2, gate2, secs) = (cfg.clone(), gate.clone(), args.sync_secs);
        eprintln!("isyncyoud: background sync every {secs}s");
        std::thread::spawn(move || sync_loop(cfg2, gate2, secs));
        isyncyou_webui::Router::with_gate(cfg, gate)
    } else {
        isyncyou_webui::Router::new(cfg)
    };

    match &args.socket {
        Some(path) => isyncyou_webui::serve_unix(path, router).map_err(|e| format!("serve: {e}")),
        None => isyncyou_webui::serve(&args.bind, router).map_err(|e| format!("serve: {e}")),
    }
}

fn unix_now() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}

/// Short host label for conflict-copy names (`*-<host>-safeBackup-NNNN`).
fn local_host() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .and_then(|h| h.split('.').next().map(str::to_string))
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "local".to_string())
}

/// Forever: every `secs`, run one sync pass per account. A pass that errors (no
/// cached token, a network blip) is logged and never kills the loop.
fn sync_loop(cfg: Config, gate: Arc<Mutex<()>>, secs: u64) {
    let host = local_host();
    loop {
        std::thread::sleep(Duration::from_secs(secs));
        for acc in &cfg.accounts {
            match sync_account(&cfg, &acc.id, &gate, &host) {
                Ok(summary) => eprintln!("isyncyoud: sync {} -> {summary}", acc.id),
                Err(e) => eprintln!("isyncyoud: sync {} skipped: {e}", acc.id),
            }
        }
    }
}

/// One scheduled pass for an account: hold the gate, resolve the cached write
/// token (silent refresh), open the store, run [`isyncyou_engine::sync_once`], and
/// record the run in the activity log. Returns the one-line summary.
fn sync_account(
    cfg: &Config,
    account: &str,
    gate: &Arc<Mutex<()>>,
    host: &str,
) -> Result<String, String> {
    let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
    let token = isyncyou_engine::auth::resolve_cached_sync_token(cfg, account)?;
    let store_path = cfg
        .accounts
        .iter()
        .find(|a| a.id == account)
        .map(|a| a.archive_root.join(".isyncyou-store.db"))
        .ok_or_else(|| format!("no account '{account}'"))?;
    let store = Store::open(store_path).map_err(|e| e.to_string())?;
    let mut client = isyncyou_graph::GraphClient::new(token);
    let mut map = MappingTable::new();
    let started = unix_now();
    let result = isyncyou_engine::sync_once(cfg, account, &store, &mut client, &mut map, host);
    let finished = unix_now();
    let (status, summary) = match &result {
        Ok(r) => ("ok", r.summary()),
        Err(e) => ("error", e.clone()),
    };
    if let Err(e) = store.add_run(account, "sync", &started, &finished, status, &summary) {
        eprintln!("isyncyoud: could not record run for {account}: {e}");
    }
    result.map(|_| summary)
}

fn load_config(path: &Path) -> Result<Config, String> {
    let cfg = Config::load(path)?;
    cfg.validate().map_err(|errs| errs.join("; "))?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_defaults() {
        let a = Args::try_parse_from(["isyncyoud"]).unwrap();
        assert_eq!(
            a,
            Args {
                config: "isyncyou.toml".into(),
                bind: "127.0.0.1:8765".into(),
                socket: None,
                heartbeat_secs: 300,
                sync_secs: 0,
            }
        );
    }

    #[test]
    fn parses_sync_secs() {
        let a = Args::try_parse_from(["isyncyoud", "--sync-secs", "300"]).unwrap();
        assert_eq!(a.sync_secs, 300);
        // off by default
        assert_eq!(Args::try_parse_from(["isyncyoud"]).unwrap().sync_secs, 0);
    }

    #[test]
    fn parses_overrides() {
        let a = Args::try_parse_from([
            "isyncyoud",
            "--config",
            "/etc/isyncyou.toml",
            "--bind",
            "0.0.0.0:9000",
            "--heartbeat-secs",
            "0",
        ])
        .unwrap();
        assert_eq!(a.config, PathBuf::from("/etc/isyncyou.toml"));
        assert_eq!(a.bind, "0.0.0.0:9000");
        assert_eq!(a.heartbeat_secs, 0);
        assert_eq!(a.socket, None);
    }

    #[test]
    fn parses_socket() {
        let a = Args::try_parse_from(["isyncyoud", "--socket", "/run/user/1000/isyncyou.sock"])
            .unwrap();
        assert_eq!(
            a.socket,
            Some(PathBuf::from("/run/user/1000/isyncyou.sock"))
        );
    }

    #[test]
    fn run_rejects_invalid_config() {
        let dir = std::env::temp_dir().join(format!("isyncyoud-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bad.toml");
        // archive_root == sync_root is invalid -> load_config errors before serving
        std::fs::write(
            &p,
            "[[accounts]]\nid=\"a\"\nusername=\"a@o\"\nsync_root=\"/x\"\narchive_root=\"/x\"\n",
        )
        .unwrap();
        assert!(load_config(&p).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
