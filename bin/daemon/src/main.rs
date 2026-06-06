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
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Parser, Debug, PartialEq, Eq)]
#[command(name = "isyncyoud", version, about = "iSyncYou engine daemon")]
struct Args {
    /// Configuration file.
    #[arg(long, default_value = "isyncyou.toml")]
    config: PathBuf,
    /// Serve over TCP instead of the default owner-only Unix socket.
    #[arg(long)]
    tcp: bool,
    /// TCP address to bind when --tcp is set (loopback-only).
    #[arg(long, default_value = "127.0.0.1:8765")]
    bind: String,
    /// Unix-domain socket path (owner-only, mode 0600). Default: $XDG_RUNTIME_DIR/isyncyou.sock.
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
    let socket = selected_socket(args.tcp, args.socket.clone());
    let where_ = match &socket {
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

    // Auto-recovery on boot (ADR-001): finish any restore operation left mid-flight
    // by a previous run *before* serving — reconcile by marker, never blind-retry.
    // Runs before any thread is spawned, so it needs no store-access gate.
    recover_pending_restores(&cfg);

    // The store-access gate is shared by the web UI and the sync thread so only
    // one of them ever holds a store's single-instance file lock at a time.
    let gate = Arc::new(Mutex::new(()));

    // Desktop integration (plan §13): publish the Dolphin/KIO FileStatus provider
    // on the session bus so the overlay-icon plugin + ServiceMenu can ask the sync
    // status of any path. Linux-only and non-fatal — a headless server has no
    // session bus, so the thread just logs and exits while sync runs unaffected.
    #[cfg(target_os = "linux")]
    {
        let accounts: Vec<isyncyou_dbus_status::AccountRoot> = cfg
            .accounts
            .iter()
            .map(|a| isyncyou_dbus_status::AccountRoot {
                sync_root: a.sync_root.clone(),
                store_db: a.archive_root.join(".isyncyou-store.db"),
            })
            .collect();
        if !accounts.is_empty() {
            std::thread::spawn(move || {
                let provider = Arc::new(isyncyou_dbus_status::StoreStatusProvider::new(accounts));
                match isyncyou_dbus_status::serve_blocking(provider) {
                    Ok(()) => {}
                    Err(e) => eprintln!(
                        "isyncyoud: Dolphin DBus status provider not started ({e}); \
                         overlays disabled, sync unaffected"
                    ),
                }
            });
        }
    }

    // A per-process capability token gates the destructive restore POST.
    let cap_token = mint_cap_token();
    let handler: Arc<dyn isyncyou_webui::RestoreHandler> =
        Arc::new(DaemonRestore { cfg: cfg.clone() });
    eprintln!("isyncyoud: restore enabled; capability token: {cap_token}");

    let mut router = if args.sync_secs > 0 {
        isyncyou_webui::Router::with_gate(cfg.clone(), gate.clone())
    } else {
        isyncyou_webui::Router::new(cfg.clone())
    }
    .with_restore(handler, cap_token.clone());

    // When scheduled sync runs, share a Scheduler so the UI can pause/resume/now.
    if args.sync_secs > 0 {
        let secs = args.sync_secs;
        let sched = Arc::new(Scheduler::default());
        eprintln!("isyncyoud: background sync every {secs}s (pausable from the UI)");
        let (cfg2, gate2, sched2) = (cfg, gate, sched.clone());
        std::thread::spawn(move || sync_loop(cfg2, gate2, secs, sched2));
        router = router.with_sync_control(sched, cap_token);
    }

    match socket {
        #[cfg(unix)]
        Some(path) => isyncyou_webui::serve_unix(&path, router).map_err(|e| format!("serve: {e}")),
        // On non-unix targets `selected_socket` always returns None (no unix-socket
        // transport), so this arm only exists to keep the match exhaustive there.
        #[cfg(not(unix))]
        Some(_) => unreachable!("selected_socket returns None on non-unix platforms"),
        None => isyncyou_webui::serve(&args.bind, router).map_err(|e| format!("serve: {e}")),
    }
}

#[cfg(unix)]
fn selected_socket(tcp: bool, socket: Option<PathBuf>) -> Option<PathBuf> {
    if tcp {
        None
    } else {
        Some(socket.unwrap_or_else(isyncyou_webui::default_unix_socket_path))
    }
}

#[cfg(not(unix))]
fn selected_socket(_tcp: bool, _socket: Option<PathBuf>) -> Option<PathBuf> {
    None
}

/// Mint a per-process capability token from `/dev/urandom` (hex), with a
/// pid-based fallback. Required on the destructive restore POST.
fn mint_cap_token() -> String {
    use std::io::Read;
    let mut buf = [0u8; 16];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => buf.iter().map(|b| format!("{b:02x}")).collect(),
        Err(_) => format!("isy-{}-fallback", std::process::id()),
    }
}

/// The daemon's destructive-action handler: re-create an archived item in the
/// cloud using the cached `login --write` (restore-scoped) token.
struct DaemonRestore {
    cfg: Config,
}
impl isyncyou_webui::RestoreHandler for DaemonRestore {
    fn restore(&self, account: &str, service: &str, id: &str) -> Result<String, String> {
        // Refuse a not-yet-ledger-migrated service before resolving a token, so the
        // web UI gets the clear "not crash-safe yet" message. (Engine re-checks.)
        if !isyncyou_engine::cloud_restore_service_supported(service) {
            return Err(isyncyou_engine::unsupported_cloud_restore_service_error(
                service,
            ));
        }
        let token = isyncyou_engine::auth::resolve_cached_restore_token(&self.cfg, account)?;
        isyncyou_engine::restore_cloud(&self.cfg, account, service, id, token)
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

/// Shared scheduled-sync control: a `paused` flag and a one-shot `trigger`, with a
/// condvar the loop waits on so pause/resume/now take effect immediately.
#[derive(Default)]
struct SchedState {
    paused: bool,
    trigger: bool,
}
#[derive(Default)]
struct Scheduler {
    state: Mutex<SchedState>,
    cv: Condvar,
}
impl isyncyou_webui::SyncControl for Scheduler {
    fn pause(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).paused = true;
        self.cv.notify_all();
    }
    fn resume(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).paused = false;
        self.cv.notify_all();
    }
    fn trigger(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).trigger = true;
        self.cv.notify_all();
    }
    fn is_paused(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).paused
    }
}

/// Finish any restore operations left mid-flight by a previous run, before serving
/// (ADR-001 auto-recovery on boot). Each ledger-backed service (mail, calendar,
/// contacts) is reconciled with the one cached write token; an account with pending
/// operations but no cached write token is logged and retried next start. Best-effort
/// and never fatal — a recovery failure must not stop the daemon.
fn recover_pending_restores(cfg: &Config) {
    for acc in &cfg.accounts {
        let mail_pending = isyncyou_engine::pending_mail_restore_count(cfg, &acc.id).unwrap_or(0);
        let cal_pending =
            isyncyou_engine::pending_calendar_restore_count(cfg, &acc.id).unwrap_or(0);
        let contact_pending =
            isyncyou_engine::pending_contacts_restore_count(cfg, &acc.id).unwrap_or(0);
        let pending = mail_pending + cal_pending + contact_pending;
        if pending == 0 {
            continue;
        }
        match isyncyou_engine::auth::resolve_cached_restore_token(cfg, &acc.id) {
            Ok(token) => {
                // One token recovers every vertical; report per service.
                let report = |svc: &str, r: Result<(usize, usize), String>| match r {
                    Ok((done, failed)) => eprintln!(
                        "isyncyoud: {svc} restore recovery [{}]: {done} completed, {failed} \
                         still pending",
                        acc.id
                    ),
                    Err(e) => {
                        eprintln!("isyncyoud: {svc} restore recovery [{}] failed: {e}", acc.id)
                    }
                };
                if mail_pending > 0 {
                    report(
                        "mail",
                        isyncyou_engine::recover_pending_mail_restores(cfg, &acc.id, token.clone()),
                    );
                }
                if cal_pending > 0 {
                    report(
                        "calendar",
                        isyncyou_engine::recover_pending_calendar_restores(
                            cfg,
                            &acc.id,
                            token.clone(),
                        ),
                    );
                }
                if contact_pending > 0 {
                    report(
                        "contacts",
                        isyncyou_engine::recover_pending_contacts_restores(cfg, &acc.id, token),
                    );
                }
            }
            Err(e) => eprintln!(
                "isyncyoud: restore recovery [{}]: {pending} operation(s) pending but no write \
                 token ({e}); will retry next start",
                acc.id
            ),
        }
    }
}

/// Forever: wait up to `secs` (or until the UI triggers/pauses), then run one sync
/// pass per account unless paused. An explicit `now` trigger always runs. A pass
/// that errors (no cached token, a network blip) is logged and never kills the loop.
fn sync_loop(cfg: Config, gate: Arc<Mutex<()>>, secs: u64, sched: Arc<Scheduler>) {
    let host = local_host();
    loop {
        // wait for the interval to elapse OR an explicit trigger to arrive
        let run = {
            let guard = sched.state.lock().unwrap_or_else(|e| e.into_inner());
            let (mut guard, res) = sched
                .cv
                .wait_timeout_while(guard, Duration::from_secs(secs), |s| !s.trigger)
                .unwrap_or_else(|e| e.into_inner());
            let triggered = guard.trigger;
            guard.trigger = false;
            // run on an explicit trigger, or on a periodic tick while not paused
            triggered || (res.timed_out() && !guard.paused)
        };
        if !run {
            continue;
        }
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
                tcp: false,
                bind: "127.0.0.1:8765".into(),
                socket: None,
                heartbeat_secs: 300,
                sync_secs: 0,
            }
        );
        #[cfg(unix)]
        assert!(selected_socket(a.tcp, a.socket.clone()).is_some());
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
            "--tcp",
            "--bind",
            "0.0.0.0:9000",
            "--heartbeat-secs",
            "0",
        ])
        .unwrap();
        assert_eq!(a.config, PathBuf::from("/etc/isyncyou.toml"));
        assert!(a.tcp);
        assert_eq!(a.bind, "0.0.0.0:9000");
        assert_eq!(a.heartbeat_secs, 0);
        assert_eq!(a.socket, None);
        assert!(selected_socket(a.tcp, a.socket.clone()).is_none());
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

    #[test]
    fn restore_handler_refuses_unmigrated_service_before_token_lookup() {
        // The web-UI restore handler refuses a not-yet-ledger-migrated service before
        // any cached-token lookup (so no token is needed to get the clear message).
        // Mail, calendar and contacts are ledger-backed and excluded here.
        let h = DaemonRestore {
            cfg: Config::default(),
        };
        for service in ["todo", "onenote"] {
            let err = isyncyou_webui::RestoreHandler::restore(&h, "a", service, "x").unwrap_err();
            assert!(err.contains("not crash-safe yet"), "{service}: got: {err}");
        }
    }
}
