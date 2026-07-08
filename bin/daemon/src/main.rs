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
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Proactively silent-refresh every account's cached read+write tokens this
    /// often, in seconds, so a long-running daemon never lets a refresh token lapse
    /// from inactivity (after the one-time login, auth stays alive with no user
    /// action). 0 = off. Default 6h.
    #[arg(long, default_value_t = 21_600)]
    token_refresh_secs: u64,
}

/// The startup line that announces the capability gate. SECURITY (AUDIT-1, #72):
/// it reports only that protection is enabled and the token's length — NEVER the
/// token value, which gates every destructive write. Kept as a pure function so a
/// regression test can pin the format.
fn cap_status_line(token_len: usize) -> String {
    format!(
        "isyncyoud: restore + sharing + verify enabled; capability token: set ({token_len} bytes)"
    )
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

    // Token keep-alive: proactively refresh every account's read+write tokens on a
    // timer so the refresh tokens never lapse from inactivity (MSA ~90-day window).
    // After the one-time login a running daemon keeps auth alive with no user action
    // — the "set-once, runs-forever" guarantee. Each refresh is silent and persists
    // the renewed token; a missing/uncached token is skipped (logged), never fatal.
    if args.token_refresh_secs > 0 && !cfg.accounts.is_empty() {
        let (cfg_ka, secs) = (cfg.clone(), args.token_refresh_secs);
        std::thread::spawn(move || token_keepalive_loop(cfg_ka, secs));
        eprintln!(
            "isyncyoud: token keep-alive every {}s ({} account(s))",
            args.token_refresh_secs,
            cfg.accounts.len()
        );
    }

    // Auto-recovery on boot (ADR-001): finish any restore operation left mid-flight
    // by a previous run *before* serving — reconcile by marker, never blind-retry.
    // Runs before any thread is spawned, so it needs no store-access gate.
    recover_pending_restores(&cfg);

    // The store-access gate is shared by the web UI and the sync thread so only
    // one of them ever holds a store's single-instance file lock at a time.
    let gate = Arc::new(Mutex::new(()));

    // FUSE Files-on-Demand (#330) + the unified read-write OneDrive folder (#478):
    // for each account with a `mount_point`, mount a placeholder view of the whole
    // OneDrive tree. Files materialize to an on-disk cache on first read; with a
    // write token the mount is read-write (edit/create/delete/rename/mkdir →
    // OneDrive) and refreshes from the cloud on browse, so it behaves like the one
    // Windows-OneDrive folder. It is registered as a single Places entry (#478 P5).
    // Linux-only and non-fatal: a missing /dev/fuse, no cached token, or an
    // unreadable store just logs and skips while everything else runs. The tree is
    // snapshotted under the shared store gate (so it never races the sync thread's
    // lock); the same snapshot feeds the DBus status provider's per-account path
    // index (placeholder vs materialized vs syncing overlays in Dolphin, #330 P4).
    // A daemon restart re-snapshots and re-mounts.
    // Shared across all placeholder mounts + the /api/v1/hydrations endpoint.
    #[cfg(target_os = "linux")]
    let hydration_tracker = Arc::new(HydrationTracker::new());

    // Desktop integration (plan §13 + #330 P4): publish the Dolphin/KIO FileStatus
    // provider on the session bus so the overlay-icon plugin + ServiceMenu can ask
    // the status of any path. The provider is a composite: paths under a FUSE
    // mount answer placeholder/materialized/syncing from the per-account index +
    // cache + live hydration set; every other path falls back to the store-backed
    // sync status. Linux-only and non-fatal — a headless server has no session bus,
    // so the thread just logs and exits while sync runs unaffected.
    #[cfg(target_os = "linux")]
    {
        // Build each mount's path index under the store gate (so it never races the
        // sync thread's lock), then hand the same items to the mount thread for the
        // Tree. An account whose store can't be read is skipped here AND below.
        let mut fuse_mounts: Vec<FuseMountInfo> = Vec::new();
        for acc in &cfg.accounts {
            let Some(mp) = acc.mount_point.clone() else {
                continue;
            };
            let items = {
                let _guard = gate.lock().unwrap();
                match Store::open(acc.archive_root.join(".isyncyou-store.db"))
                    .and_then(|s| s.all_items_by_service(&acc.id, "onedrive"))
                {
                    Ok(items) => items,
                    Err(e) => {
                        eprintln!("isyncyoud: FUSE mount '{}' skipped: store: {e}", acc.id);
                        continue;
                    }
                }
            };
            let cache_dir = acc.archive_root.join(".isyncyou-fuse-cache");
            let index = Arc::new(isyncyou_fuse::PlaceholderIndex::from_items(&items));
            fuse_mounts.push(FuseMountInfo {
                mount_point: mp.clone(),
                cache_dir: cache_dir.clone(),
                index: index.clone(),
            });
            // One Places sidebar entry for the single OneDrive folder (#478 P5):
            // the mount IS the folder, so register only it (no second sync_root
            // entry — the dual-entry variant was confusing).
            register_onedrive_place(&mp);
            // Spawn the mount thread with the already-snapshotted items.
            let account = acc.id.clone();
            let cfg_m = cfg.clone();
            let tracker = hydration_tracker.clone();
            // the mount's cloud refresh opens the store, so it shares the gate
            let gate_r = gate.clone();
            std::thread::spawn(move || {
                // clear a stale mount from a previous crash, then ensure the dir exists
                let _ = std::process::Command::new("fusermount3")
                    .arg("-u")
                    .arg(&mp)
                    .status();
                if let Err(e) = std::fs::create_dir_all(&mp) {
                    eprintln!(
                        "isyncyoud: FUSE mount '{account}' skipped: mkdir {}: {e}",
                        mp.display()
                    );
                    return;
                }
                // Fail-fast: skip the mount cleanly if no read token is resolvable
                // now. The hydrator itself re-resolves (silent-refresh) per fetch, so
                // the mount keeps working past the token's ~1h lifetime.
                if let Err(e) = isyncyou_engine::auth::resolve_cached_read_token(&cfg_m, &account) {
                    eprintln!("isyncyoud: FUSE mount '{account}' skipped: {e}");
                    return;
                }
                // Mount read-write when a write token (Files.ReadWrite) is available so
                // edits in the mount upload to OneDrive (unified-folder #478 P1);
                // otherwise stay read-only. Uploads re-resolve the token per write.
                let writable =
                    isyncyou_engine::auth::resolve_cached_sync_token(&cfg_m, &account).is_ok();
                let tree = isyncyou_fuse::Tree::from_items(&items);
                let mut fs = isyncyou_fuse::PlaceholderFs::new(
                    tree,
                    Box::new(GraphHydrator {
                        cfg: cfg_m.clone(),
                        account: account.clone(),
                    }),
                    cache_dir,
                )
                .with_observer(tracker as Arc<dyn isyncyou_fuse::HydrationObserver>);
                if writable {
                    fs = fs
                        .with_uploader(Box::new(GraphUploader {
                            cfg: cfg_m.clone(),
                            account: account.clone(),
                        }))
                        // a read-write mount also refreshes from the cloud on browse
                        // so changes made elsewhere (another device, the web) appear
                        // (#478 P4). The read-only mount path keeps the static tree.
                        .with_refresher(Box::new(GraphRefresher {
                            cfg: cfg_m.clone(),
                            account: account.clone(),
                            gate: gate_r,
                        }));
                }
                let mode = if writable { "read-write" } else { "read-only" };
                eprintln!(
                    "isyncyoud: mounting OneDrive placeholders ({mode}) for '{account}' at {}",
                    mp.display()
                );
                if let Err(e) = isyncyou_fuse::mount(fs, &mp) {
                    eprintln!("isyncyoud: FUSE mount '{account}' ended: {e}");
                }
            });
        }

        let store_accounts: Vec<isyncyou_dbus_status::AccountRoot> = cfg
            .accounts
            .iter()
            .map(|a| isyncyou_dbus_status::AccountRoot {
                sync_root: a.sync_root.clone(),
                store_db: a.archive_root.join(".isyncyou-store.db"),
            })
            .collect();
        if !fuse_mounts.is_empty() || !store_accounts.is_empty() {
            let provider = Arc::new(DaemonStatusProvider {
                mounts: fuse_mounts,
                store: isyncyou_dbus_status::StoreStatusProvider::new(store_accounts),
                hydration: hydration_tracker.clone(),
            });
            std::thread::spawn(
                move || match isyncyou_dbus_status::serve_blocking(provider) {
                    Ok(()) => {}
                    Err(e) => eprintln!(
                        "isyncyoud: Dolphin DBus status provider not started ({e}); \
                         overlays disabled, sync unaffected"
                    ),
                },
            );
        }
    }

    // Capability tokens for the daemon-only destructive actions; the live-handler
    // tokens are minted inside build_live_router (#89).
    let cap_token = isyncyou_app_host::mint_cap_token();
    let push_cap_token = isyncyou_app_host::mint_cap_token();
    // Live cloud-poll interval (#559): seeded from config, adjusted live by the
    // settings POST, read by the sync loop each tick.
    let live_interval = Arc::new(AtomicU64::new(cfg.sync.poll_interval_secs.max(1)));
    // Push notifier (#576) — shared with the sync loop so a completed backup notifies
    // the phone (FCM). Daemon-only: the standalone mobile profile never wires it.
    let push_notifier = Arc::new(isyncyou_app_host::DaemonPush::new(&cfg));
    // SSE change bus (#559): the sync loop notifies it after each pass; the web UI
    // subscribes at /api/v1/events and refetches the active view.
    let events = Arc::new(isyncyou_webui::EventBus::new());
    // SECURITY: never log the capability token itself — it gates every destructive
    // write. Log only that protection is enabled (format pinned by cap_status_line).
    eprintln!("{}", cap_status_line(cap_token.len()));

    // The shared "live companion" router (#89): read endpoints + live-write handlers
    // + account-auth + settings + events. The daemon extends it with the daemon-only
    // restore/share/push handlers the standalone mobile profile omits.
    let gate_opt = if args.sync_secs > 0 {
        Some(gate.clone())
    } else {
        None
    };
    let mut router = isyncyou_app_host::build_live_router(
        cfg.clone(),
        gate_opt,
        events.clone(),
        args.config.clone(),
        live_interval.clone(),
        // Desktop runs the whole-tree sync_once, not the mobile offline pass, so this stays
        // idle (GET /onedrive/transfers reports empty) — the wiring is for the mobile profile.
        isyncyou_connectors::SharedProgress::new(),
    )
    .with_restore(
        Arc::new(isyncyou_app_host::DaemonRestore::new(cfg.clone()))
            as Arc<dyn isyncyou_webui::RestoreHandler>,
        cap_token.clone(),
    )
    // Outbound share is now wired inside build_live_router (#onedrive-mobile 0.9) so both
    // desktop and mobile get it; no separate daemon wiring here.
    .with_push(
        push_notifier.clone() as Arc<dyn isyncyou_webui::PushHandler>,
        push_cap_token,
    );

    // Expose in-flight FUSE hydrations to the status bar (Linux placeholder mounts).
    #[cfg(target_os = "linux")]
    {
        router =
            router.with_hydrations(hydration_tracker as Arc<dyn isyncyou_webui::HydrationStatus>);
    }

    // When scheduled sync runs, share a Scheduler so the UI can pause/resume/now.
    if args.sync_secs > 0 {
        let sched = Arc::new(Scheduler::default());
        eprintln!(
            "isyncyoud: cloud-poll sync enabled, interval {}s (live-adjustable from the UI)",
            live_interval.load(Ordering::Relaxed)
        );
        // Event-driven accelerator (#331): one change-source watcher per account
        // wakes this sync loop early on local changes (honoring
        // `cfg.sync.change_source` — inotify by default, the privileged mount-wide
        // fanotify backend when opted in + permitted). The periodic timer still
        // ticks, so the reconciler stays the source of truth even if events are
        // missed; this only shortens the latency between a local edit and its sync.
        std::thread::spawn({
            let (cfg_w, sched_w) = (cfg.clone(), sched.clone());
            move || watch_loop(cfg_w, sched_w)
        });
        let (cfg2, gate2, sched2) = (cfg, gate, sched.clone());
        let (interval2, events2) = (live_interval.clone(), events.clone());
        let push2 = push_notifier.clone();
        std::thread::spawn(move || sync_loop(cfg2, gate2, interval2, sched2, events2, push2));
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

// --- KDE Places integration (unified-folder #478 P5, Linux desktop) ----------
// Register the placeholder mount as a single Places sidebar entry so the one
// read-write OneDrive folder is one click away in Dolphin — the Windows model of
// a single folder, not a confusing pair (sync_root + mount). One entry per
// account's mount, idempotent (keyed on the file:// href).

/// `~/.local/share/user-places.xbel` (the KDE Places bookmark file), honoring
/// `XDG_DATA_HOME`. `None` if neither it nor `$HOME` is set.
#[cfg(target_os = "linux")]
fn places_file() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .map(|base| base.join("user-places.xbel"))
}

/// `file://` URI for an absolute path, percent-encoding everything outside the
/// unreserved set (keeping `/` as the separator).
#[cfg(target_os = "linux")]
fn path_to_file_uri(p: &Path) -> String {
    let mut out = String::from("file://");
    for b in p.to_string_lossy().bytes() {
        match b {
            b'/' | b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Minimal XML escaping for text/attribute content.
#[cfg(target_os = "linux")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A stable hex id for a Places bookmark, derived from its href (FNV-1a) so a
/// re-run reuses the same `<ID>` rather than spraying duplicates.
#[cfg(target_os = "linux")]
fn stable_id(href: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in href.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Add a single KDE Places bookmark for `mount_point` to `xbel_path` if one for
/// that href isn't already present. Idempotent (keyed on the `file://` href), so
/// it's safe to call on every daemon start. Returns whether it added an entry.
#[cfg(target_os = "linux")]
fn ensure_place_in(
    xbel_path: &Path,
    mount_point: &Path,
    label: &str,
    icon: &str,
) -> std::io::Result<bool> {
    let href = path_to_file_uri(mount_point);
    let existing = std::fs::read_to_string(xbel_path).unwrap_or_default();
    if existing.contains(&format!("href=\"{href}\"")) {
        return Ok(false); // already registered — never duplicate
    }
    let bookmark = format!(
        "  <bookmark href=\"{href}\">\n   <title>{title}</title>\n   <info>\n    \
         <metadata owner=\"http://freedesktop.org\">\n     <bookmark:icon name=\"{icon}\"/>\n    \
         </metadata>\n    <metadata owner=\"http://www.kde.org\">\n     <ID>{id}</ID>\n     \
         <isSystemItem>false</isSystemItem>\n    </metadata>\n   </info>\n  </bookmark>\n",
        href = href,
        title = xml_escape(label),
        icon = icon,
        id = stable_id(&href),
    );
    let header = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE xbel>\n\
         <xbel xmlns:bookmark=\"http://www.freedesktop.org/standards/desktop-bookmarks\" \
         xmlns:mime=\"http://www.freedesktop.org/standards/shared-mime-info\" \
         xmlns:kdepriv=\"http://www.kde.org/kdepriv\">\n";
    let new_content = if existing.trim().is_empty() {
        format!("{header}{bookmark}</xbel>\n")
    } else if let Some(pos) = existing.rfind("</xbel>") {
        let mut c = existing.clone();
        c.replace_range(pos..pos, &bookmark);
        c
    } else {
        // no closing tag (unexpected): append our bookmark + a close, don't corrupt
        format!("{existing}{bookmark}</xbel>\n")
    };
    if let Some(parent) = xbel_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(xbel_path, new_content)?;
    Ok(true)
}

/// Register the one OneDrive folder (`mount_point`) in the KDE Places sidebar.
/// Best-effort + non-fatal: a missing data dir / no KDE just logs.
#[cfg(target_os = "linux")]
fn register_onedrive_place(mount_point: &Path) {
    let Some(xbel) = places_file() else {
        return;
    };
    match ensure_place_in(&xbel, mount_point, "OneDrive", "folder-cloud") {
        Ok(true) => eprintln!(
            "isyncyoud: registered Places entry 'OneDrive' -> {}",
            mount_point.display()
        ),
        Ok(false) => {}
        Err(e) => eprintln!("isyncyoud: Places registration skipped: {e}"),
    }
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
/// contacts, todo, onenote) is reconciled with the one cached write token; an account with pending
/// operations but no cached write token is logged and retried next start. Best-effort
/// and never fatal — a recovery failure must not stop the daemon.
fn recover_pending_restores(cfg: &Config) {
    for acc in &cfg.accounts {
        // #654/#655/#722: reconcile any OneDrive cloud-write left mid-flight
        // (create/rename/move/delete/upload/replace/share/invite) before serving —
        // probe-adopt / etag-guarded / fail-closed, never blind.
        // Runs regardless of the restore-pending count below.
        if isyncyou_engine::pending_cloud_write_count(cfg, &acc.id).unwrap_or(0) > 0 {
            match isyncyou_engine::recover_pending_cloud_writes_for(cfg, &acc.id) {
                Ok(n) => eprintln!(
                    "isyncyoud: onedrive cloud-write recovery [{}]: {n} reconciled",
                    acc.id
                ),
                Err(e) => eprintln!(
                    "isyncyoud: onedrive cloud-write recovery [{}] failed: {e}",
                    acc.id
                ),
            }
        }

        let mail_pending = isyncyou_engine::pending_mail_restore_count(cfg, &acc.id).unwrap_or(0);
        let cal_pending =
            isyncyou_engine::pending_calendar_restore_count(cfg, &acc.id).unwrap_or(0);
        let contact_pending =
            isyncyou_engine::pending_contacts_restore_count(cfg, &acc.id).unwrap_or(0);
        let todo_pending = isyncyou_engine::pending_todo_restore_count(cfg, &acc.id).unwrap_or(0);
        let onenote_pending =
            isyncyou_engine::pending_onenote_restore_count(cfg, &acc.id).unwrap_or(0);
        let pending = mail_pending + cal_pending + contact_pending + todo_pending + onenote_pending;
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
                        isyncyou_engine::recover_pending_contacts_restores(
                            cfg,
                            &acc.id,
                            token.clone(),
                        ),
                    );
                }
                if todo_pending > 0 {
                    report(
                        "todo",
                        isyncyou_engine::recover_pending_todo_restores(cfg, &acc.id, token.clone()),
                    );
                }
                if onenote_pending > 0 {
                    report(
                        "onenote",
                        isyncyou_engine::recover_pending_onenote_restores(cfg, &acc.id, token),
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

/// Forever: wait up to the live cloud-poll interval (or until the UI triggers/
/// pauses), then run one sync pass per account unless paused. The interval is read
/// from `interval` each tick, so the UI's settings slider takes effect on the next
/// wait without a restart (`429`/`Retry-After` backoff is handled inside the Graph
/// client's retry). After a pass that ran, `events` is notified so SSE subscribers
/// refetch. An explicit `now` trigger always runs. A pass that errors (no cached
/// token, a network blip) is logged and never kills the loop.
fn sync_loop(
    cfg: Config,
    gate: Arc<Mutex<()>>,
    interval: Arc<AtomicU64>,
    sched: Arc<Scheduler>,
    events: Arc<isyncyou_webui::EventBus>,
    push: Arc<isyncyou_app_host::DaemonPush>,
) {
    let host = local_host();
    loop {
        // read the live interval each tick so a UI slider change applies promptly
        let secs = interval.load(Ordering::Relaxed).max(1);
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
            // Keep the per-service archive fresh too (live client, #563 AC-5): an
            // incremental mail pass so new cloud mail lands in the store and the
            // SSE notify below surfaces it without a manual reload.
            match backup_account(&cfg, &acc.id, &gate) {
                Ok((summary, delta)) => {
                    eprintln!("isyncyoud: backup {} -> {summary}", acc.id);
                    // Push (#576): notify the phone when new content was archived. The
                    // FCM token must have been registered by the UI; otherwise no-op.
                    if let Some(body) = delta.notification() {
                        let n = push.notify("iSyncYou — backup complete", &body);
                        if n > 0 {
                            eprintln!("isyncyoud: push '{body}' sent to {n} device(s)");
                        }
                    }
                }
                Err(e) => eprintln!("isyncyoud: backup {} skipped: {e}", acc.id),
            }
        }
        // wake SSE subscribers so the UI refetches the active view (near-real-time)
        events.notify();
    }
}

/// Event-driven accelerator (#331): one change-source watcher thread per account.
/// On any local change under a sync root it wakes [`sync_loop`] early via the same
/// one-shot trigger the web-UI "sync now" uses — unless paused. Accounts whose
/// backend is reconcile-only (or where no watcher could start) are left to the
/// timer. The periodic reconcile stays authoritative, so a missed/dropped event is
/// harmless; this only shortens the latency between a local edit and its sync.
fn watch_loop(cfg: Config, sched: Arc<Scheduler>) {
    use isyncyou_change_source::ChangeSource as _;
    for acc in &cfg.accounts {
        let root = acc.sync_root.clone();
        let account = acc.id.clone();
        let change_source = cfg.sync.clone();
        let sched = sched.clone();
        std::thread::spawn(move || {
            let Some(mut watcher) =
                isyncyou_change_source::select_change_source(&change_source, &root)
            else {
                // reconcile-only or no watcher available: the periodic timer covers it.
                return;
            };
            eprintln!(
                "isyncyoud: change accelerator watching {} for '{account}'",
                root.display()
            );
            loop {
                let changes = watcher.poll(Duration::from_secs(30), Duration::from_secs(2));
                if changes.is_empty() {
                    continue;
                }
                // Local change → wake the sync loop early, unless paused. Set the
                // same one-shot trigger as "sync now", atomically with the paused
                // check so an event never wakes a paused loop.
                let mut st = sched.state.lock().unwrap_or_else(|e| e.into_inner());
                if !st.paused {
                    st.trigger = true;
                    sched.cv.notify_all();
                }
            }
        });
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

/// One incremental **mail backup** pass for the live client (#563 AC-5). Uses the
/// cached **read** token (`Mail.Read` + `MailboxSettings.Read`, S-P4.1), runs the
/// per-folder delta (cheap when idle), downloads bodies for newly-seen messages
/// (capped per pass so a burst can't stall the loop), and refreshes the mailbox
/// flank snapshots. Keeps the store/archive current so the SSE notify surfaces new
/// mail. Read-only; a missing token is a clean skip (logged, never fatal). Mail is
/// the pilot — other services follow their own rollout stories.
/// What a backup pass newly archived, for the push notification (#576). Only the
/// counts the user wants to be told about ("3 new emails backed up").
#[derive(Default)]
struct BackupDelta {
    mail: u64,
    calendar: u64,
    contacts: u64,
    todo: u64,
    onenote: u64,
}
impl BackupDelta {
    fn total(&self) -> u64 {
        self.mail + self.calendar + self.contacts + self.todo + self.onenote
    }
    /// A short human notification body, or `None` when nothing new was archived.
    fn notification(&self) -> Option<String> {
        if self.total() == 0 {
            return None;
        }
        let one_or_many =
            |n: u64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let mut parts = Vec::new();
        if self.mail > 0 {
            parts.push(one_or_many(self.mail, "email", "emails"));
        }
        if self.calendar > 0 {
            parts.push(one_or_many(self.calendar, "event", "events"));
        }
        if self.contacts > 0 {
            parts.push(one_or_many(self.contacts, "contact", "contacts"));
        }
        if self.todo > 0 {
            parts.push(one_or_many(self.todo, "task", "tasks"));
        }
        if self.onenote > 0 {
            parts.push(one_or_many(self.onenote, "note", "notes"));
        }
        Some(format!("{} backed up", parts.join(", ")))
    }
}

fn backup_account(
    cfg: &Config,
    account: &str,
    gate: &Arc<Mutex<()>>,
) -> Result<(String, BackupDelta), String> {
    let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
    // Read-only cache fill across the five non-file services. Shared with the
    // standalone mobile client via `engine::refresh_cache_account` (#89). The read
    // token drives the pass; the restore token (best-effort) is used only for the
    // ToDo attachments endpoint, which denies the read scope.
    let read = isyncyou_engine::auth::resolve_cached_read_token(cfg, account)?;
    let write = isyncyou_engine::auth::resolve_cached_restore_token(cfg, account).ok();
    let c = isyncyou_engine::refresh_cache_account(cfg, account, read, write)?;
    // Notification delta (#576): new mail bodies (not delta upserts, which include
    // flag/read changes) is the user-relevant "new mail" signal; same per service.
    let delta = BackupDelta {
        mail: c.mail_bodies as u64,
        calendar: c.calendar_bodies as u64,
        contacts: c.contacts_bodies as u64,
        todo: c.todo_bodies as u64,
        onenote: c.onenote_bodies as u64,
    };
    let summary = format!(
        "mail: {} folders, {} upserted, {} deleted; {} new bodies; {} flanks | \
         calendar: {} events, {} bodies, {} flanks | \
         contacts: {} upserted, {} bodies, {} photos | \
         todo: {} indexed, {} bodies, {} flanks, {} sub | \
         onenote: {} pages, {} bodies, {} resources, {} containers",
        c.mail_folders,
        c.mail_upserted,
        c.mail_deleted,
        c.mail_bodies,
        c.mail_flanks,
        c.calendar_events,
        c.calendar_bodies,
        c.calendar_flanks,
        c.contacts_upserted,
        c.contacts_bodies,
        c.contacts_photos,
        c.todo_indexed,
        c.todo_bodies,
        c.todo_flanks,
        c.todo_sub,
        c.onenote_pages,
        c.onenote_bodies,
        c.onenote_resources,
        c.onenote_containers,
    );
    Ok((summary, delta))
}

fn load_config(path: &Path) -> Result<Config, String> {
    let cfg = Config::load(path)?;
    cfg.validate().map_err(|errs| errs.join("; "))?;
    Ok(cfg)
}

/// Forever: every `secs`, silent-refresh each account's cached **read** and
/// **write** tokens so their refresh tokens never lapse from inactivity. Read and
/// write live in separate caches/clients; resolving each renews and persists it
/// (an access token still valid is reused, but the keep-alive interval is chosen
/// well above the ~1h access-token lifetime so each pass forces a real refresh and
/// resets the refresh token's inactivity clock). Best-effort: an account with no
/// cached token (read or write) is logged and skipped, never fatal.
fn token_keepalive_loop(cfg: Config, secs: u64) {
    loop {
        std::thread::sleep(Duration::from_secs(secs));
        let mut refreshed = 0usize;
        for acc in &cfg.accounts {
            match isyncyou_engine::auth::resolve_cached_read_token(&cfg, &acc.id) {
                Ok(_) => refreshed += 1,
                Err(e) => {
                    eprintln!(
                        "isyncyoud: token keep-alive (read) [{}] skipped: {e}",
                        acc.id
                    )
                }
            }
            match isyncyou_engine::auth::resolve_cached_sync_token(&cfg, &acc.id) {
                Ok(_) => refreshed += 1,
                Err(e) => {
                    eprintln!(
                        "isyncyoud: token keep-alive (write) [{}] skipped: {e}",
                        acc.id
                    )
                }
            }
        }
        eprintln!(
            "isyncyoud: token keep-alive: {refreshed} token(s) kept alive across {} account(s)",
            cfg.accounts.len()
        );
    }
}

/// Hydrates a FUSE placeholder by downloading its content from OneDrive on first
/// read (the read-only mount path; #330).
///
/// The cached read token is re-resolved **per fetch** (silent-refresh) rather than
/// captured once at mount time: a placeholder mount is long-lived, so a token
/// snapshotted at startup would expire after ~1h and then every download would
/// fail (EIO) until the daemon restarted. `resolve_cached_read_token` is cheap when
/// the token is still valid (a file read + expiry check) and only hits the network
/// to refresh, so per-fetch resolution keeps a mount downloading indefinitely.
#[cfg(target_os = "linux")]
struct GraphHydrator {
    cfg: Config,
    account: String,
}
#[cfg(target_os = "linux")]
impl isyncyou_fuse::Hydrator for GraphHydrator {
    fn fetch(&self, remote_id: &str) -> Result<Vec<u8>, String> {
        let token = isyncyou_engine::auth::resolve_cached_read_token(&self.cfg, &self.account)?;
        isyncyou_graph::GraphClient::new(token)
            .download_content(remote_id)
            .map_err(|e| e.to_string())
    }
}

/// Uploads an edited FUSE file back to OneDrive (write-back on the read-write mount;
/// unified-folder #478 P1). Re-resolves the cached write token (Files.ReadWrite) per
/// upload so a long-lived mount keeps working past the token's lifetime.
#[cfg(target_os = "linux")]
struct GraphUploader {
    cfg: Config,
    account: String,
}
#[cfg(target_os = "linux")]
impl isyncyou_fuse::Uploader for GraphUploader {
    fn upload(&self, remote_id: &str, data: &[u8]) -> Result<(), String> {
        let token = isyncyou_engine::auth::resolve_cached_sync_token(&self.cfg, &self.account)?;
        let r = isyncyou_graph::GraphClient::new(token)
            .put_content(remote_id, data)
            .map(|_| ())
            .map_err(|e| e.to_string());
        eprintln!(
            "isyncyoud: write-back upload {remote_id} ({} bytes) -> {}",
            data.len(),
            if r.is_ok() {
                "ok".to_string()
            } else {
                format!("ERR {r:?}")
            }
        );
        r
    }
    fn create(&self, dest_path: &str, data: &[u8]) -> Result<String, String> {
        let token = isyncyou_engine::auth::resolve_cached_sync_token(&self.cfg, &self.account)?;
        let item = isyncyou_graph::GraphClient::new(token)
            .upload_file(dest_path, data, 10 * 1024 * 1024)
            .map_err(|e| e.to_string())?;
        let id = item
            .get("id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| "create response had no id".to_string())?;
        eprintln!(
            "isyncyoud: write-back create {dest_path} ({} bytes) -> {id}",
            data.len()
        );
        Ok(id)
    }
    fn delete(&self, remote_id: &str) -> Result<(), String> {
        let token = isyncyou_engine::auth::resolve_cached_sync_token(&self.cfg, &self.account)?;
        let r = isyncyou_graph::GraphClient::new(token)
            .delete_item(remote_id)
            .map_err(|e| e.to_string());
        eprintln!(
            "isyncyoud: write-back delete {remote_id} -> {}",
            if r.is_ok() {
                "ok".to_string()
            } else {
                format!("ERR {r:?}")
            }
        );
        r
    }
    fn mkdir(&self, parent_id: &str, name: &str) -> Result<String, String> {
        let token = isyncyou_engine::auth::resolve_cached_sync_token(&self.cfg, &self.account)?;
        let item = isyncyou_graph::GraphClient::new(token)
            .create_folder(parent_id, name)
            .map_err(|e| e.to_string())?;
        let id = item
            .get("id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| "mkdir response had no id".to_string())?;
        let under = if parent_id.is_empty() {
            "root"
        } else {
            parent_id
        };
        eprintln!("isyncyoud: write-back mkdir {name} under {under} -> {id}");
        Ok(id)
    }
    fn rename(
        &self,
        remote_id: &str,
        new_parent_id: Option<&str>,
        new_name: &str,
    ) -> Result<(), String> {
        let token = isyncyou_engine::auth::resolve_cached_sync_token(&self.cfg, &self.account)?;
        let r = isyncyou_graph::GraphClient::new(token)
            .move_item(remote_id, new_parent_id, new_name)
            .map(|_| ())
            .map_err(|e| e.to_string());
        eprintln!(
            "isyncyoud: write-back rename {remote_id} -> {new_name} (reparent={}) {}",
            new_parent_id.is_some(),
            if r.is_ok() {
                "ok".to_string()
            } else {
                format!("ERR {r:?}")
            }
        );
        r
    }
}

/// Refreshes a read-write placeholder mount from the cloud (unified-folder #478
/// P4): runs a OneDrive delta into the account's store, then returns the current
/// items so the mount's tree reconciles in changes made elsewhere (another device,
/// the web). Read-only (no local-file side effects) — it does not touch the
/// sync_root, only the store's item index + delta cursor. Opens the store under
/// the shared gate so it never races the sync thread's single-instance lock.
#[cfg(target_os = "linux")]
struct GraphRefresher {
    cfg: Config,
    account: String,
    gate: Arc<Mutex<()>>,
}
#[cfg(target_os = "linux")]
impl isyncyou_fuse::Refresher for GraphRefresher {
    fn refresh(&self) -> Result<Vec<isyncyou_store::Item>, String> {
        let _g = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        let token = isyncyou_engine::auth::resolve_cached_read_token(&self.cfg, &self.account)?;
        let archive_root = self
            .cfg
            .accounts
            .iter()
            .find(|a| a.id == self.account)
            .map(|a| a.archive_root.clone())
            .ok_or_else(|| format!("no account '{}'", self.account))?;
        let store =
            Store::open(archive_root.join(".isyncyou-store.db")).map_err(|e| e.to_string())?;
        let mut client = isyncyou_graph::GraphClient::new(token);
        let mut map = MappingTable::new();
        let now = unix_now();
        isyncyou_connectors::incremental_sync(
            &mut client,
            &store,
            &mut map,
            &self.account,
            &now,
            &archive_root,
        )
        .map_err(|e| e.to_string())?;
        store
            .all_items_by_service(&self.account, "onedrive")
            .map_err(|e| e.to_string())
    }
}

/// Tracks in-flight FUSE hydrations (on-demand downloads) and surfaces a
/// batch-coalesced desktop toast (#330 P3): one "downloading…" notification when
/// a batch starts, updated in place as more files join, and one "ready"
/// notification when the batch drains. Also feeds `/api/v1/hydrations` so the
/// status bar can show the in-flight list. Toasts go through the system
/// `notify-send` (desktop-only, non-fatal — a headless box just has none).
/// FUSE serializes reads, so coalescing is time-windowed, not overlap-based: the
/// "ready" toast is delayed by this debounce and any new download within the
/// window rejoins the batch (a multi-select / folder fetch = one notification).
#[cfg(target_os = "linux")]
const HYDRATION_DEBOUNCE: Duration = Duration::from_millis(1500);

#[cfg(target_os = "linux")]
#[derive(Default)]
struct HydInner {
    /// File names currently materializing.
    active: Vec<String>,
    /// Remote ids currently materializing — keyed by id (not name) so the overlay
    /// provider can answer "is this exact file syncing?" without name collisions.
    active_ids: std::collections::HashSet<String>,
    /// Files started since the current batch began (for the "N ready" message).
    batch_total: u32,
    /// notify-send id of the live toast, reused via --replace-id (0 = none).
    toast_id: u32,
    /// Bumped on every start and every drain; a pending finalize fires only if its
    /// captured generation still matches (no new activity in the meantime).
    generation: u64,
}

#[cfg(target_os = "linux")]
struct HydrationTracker {
    st: Arc<Mutex<HydInner>>,
}

#[cfg(target_os = "linux")]
impl HydrationTracker {
    fn new() -> Self {
        HydrationTracker {
            st: Arc::new(Mutex::new(HydInner::default())),
        }
    }

    /// Show/replace a desktop toast; returns the (possibly new) notification id.
    /// Non-fatal: if `notify-send` is missing or fails, returns `replace` unchanged.
    fn toast(summary: &str, body: &str, replace: u32) -> u32 {
        let mut cmd = std::process::Command::new("notify-send");
        cmd.arg("--app-name=iSyncYou").arg("--print-id");
        if replace > 0 {
            cmd.arg(format!("--replace-id={replace}"));
        }
        cmd.arg(summary).arg(body);
        match cmd.output() {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse()
                .unwrap_or(replace),
            _ => replace,
        }
    }
}

#[cfg(target_os = "linux")]
impl isyncyou_fuse::HydrationObserver for HydrationTracker {
    fn on_start(&self, name: &str, remote_id: &str) {
        // A genuinely new batch = nothing active AND nothing pending a finalize
        // (batch_total back to 0). Otherwise this start rejoins the current batch
        // (incl. one that drained but is still inside its debounce window).
        let (new_batch, total, replace) = {
            let mut s = self.st.lock().unwrap();
            let new_batch = s.active.is_empty() && s.batch_total == 0;
            s.active.push(name.to_string());
            s.active_ids.insert(remote_id.to_string());
            s.batch_total += 1;
            s.generation += 1; // invalidate any pending finalize
            (new_batch, s.batch_total, s.toast_id)
        };
        let body = if new_batch {
            format!("Fetching {name}…")
        } else {
            format!("Fetching {total} files…")
        };
        let id = Self::toast("Downloading from OneDrive", &body, replace);
        self.st.lock().unwrap().toast_id = id;
    }

    fn on_done(&self, name: &str, remote_id: &str, _ok: bool) {
        let gen = {
            let mut s = self.st.lock().unwrap();
            if let Some(pos) = s.active.iter().position(|n| n == name) {
                s.active.remove(pos);
            }
            s.active_ids.remove(remote_id);
            if !s.active.is_empty() {
                return; // batch still draining
            }
            s.generation += 1;
            s.generation
        };
        // Delay the "ready" toast; a new download within the window rejoins the
        // batch (bumping generation), which cancels this finalize.
        let st = self.st.clone();
        std::thread::spawn(move || {
            std::thread::sleep(HYDRATION_DEBOUNCE);
            let (total, replace) = {
                let s = st.lock().unwrap();
                if s.generation != gen || !s.active.is_empty() {
                    return; // superseded by newer activity
                }
                (s.batch_total, s.toast_id)
            };
            let body = if total == 1 {
                "A file is ready offline".to_string()
            } else {
                format!("{total} files are ready offline")
            };
            Self::toast("OneDrive", &body, replace);
            let mut s = st.lock().unwrap();
            if s.generation == gen {
                s.batch_total = 0;
                s.toast_id = 0;
            }
        });
    }
}

#[cfg(target_os = "linux")]
impl HydrationTracker {
    /// Whether a specific remote id is materializing right now (for the overlay
    /// provider's "syncing" state).
    fn is_hydrating(&self, remote_id: &str) -> bool {
        self.st.lock().unwrap().active_ids.contains(remote_id)
    }
}

#[cfg(target_os = "linux")]
impl isyncyou_webui::HydrationStatus for HydrationTracker {
    fn active(&self) -> Vec<String> {
        self.st.lock().unwrap().active.clone()
    }
}

/// One placeholder mount's data for the DBus overlay provider: its mount point, the
/// materialization cache dir, and the path→item index built from the same store
/// snapshot the mount used (#330 P4).
#[cfg(target_os = "linux")]
struct FuseMountInfo {
    mount_point: PathBuf,
    cache_dir: PathBuf,
    index: Arc<isyncyou_fuse::PlaceholderIndex>,
}

#[cfg(target_os = "linux")]
impl FuseMountInfo {
    /// Overlay status for an absolute path under this mount. A directory is a cloud
    /// container (`Placeholder`); a file is `Syncing` while hydrating, else
    /// `Materialized` if its cache file exists, else `Placeholder`. A path the
    /// index doesn't know (e.g. the mount root itself) is `Unknown`.
    fn status(
        &self,
        path: &Path,
        hydration: &HydrationTracker,
    ) -> isyncyou_dbus_status::OverlayStatus {
        use isyncyou_dbus_status::OverlayStatus;
        let Ok(rel) = path.strip_prefix(&self.mount_point) else {
            return OverlayStatus::Unknown;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let Some(rid) = self.index.remote_id(&rel) else {
            return OverlayStatus::Unknown;
        };
        if self.index.is_dir(&rel).unwrap_or(false) {
            return OverlayStatus::Placeholder; // a browsable cloud folder
        }
        if hydration.is_hydrating(rid) {
            return OverlayStatus::Syncing;
        }
        if self
            .cache_dir
            .join(isyncyou_fuse::cache_file_name(rid))
            .exists()
        {
            OverlayStatus::Materialized
        } else {
            OverlayStatus::Placeholder
        }
    }
}

/// The daemon's composite [`isyncyou_dbus_status::StatusProvider`]: paths under a
/// FUSE placeholder mount answer placeholder/materialized/syncing; every other path
/// falls back to the store-backed sync status (#330 P4).
#[cfg(target_os = "linux")]
struct DaemonStatusProvider {
    mounts: Vec<FuseMountInfo>,
    store: isyncyou_dbus_status::StoreStatusProvider,
    hydration: Arc<HydrationTracker>,
}

#[cfg(target_os = "linux")]
impl isyncyou_dbus_status::StatusProvider for DaemonStatusProvider {
    fn status(&self, path: &Path) -> isyncyou_dbus_status::OverlayStatus {
        for m in &self.mounts {
            if path.starts_with(&m.mount_point) {
                return m.status(path, &self.hydration);
            }
        }
        self.store.status(path)
    }

    fn roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = self.mounts.iter().map(|m| m.mount_point.clone()).collect();
        roots.extend(self.store.roots());
        roots
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_delta_notification_text() {
        // Nothing new → no notification (the loop stays silent).
        assert_eq!(BackupDelta::default().notification(), None);
        // Singular vs plural per service.
        assert_eq!(
            BackupDelta {
                mail: 1,
                ..Default::default()
            }
            .notification()
            .as_deref(),
            Some("1 email backed up")
        );
        assert_eq!(
            BackupDelta {
                mail: 3,
                ..Default::default()
            }
            .notification()
            .as_deref(),
            Some("3 emails backed up")
        );
        // Multiple services join in a stable order.
        assert_eq!(
            BackupDelta {
                mail: 2,
                calendar: 1,
                onenote: 4,
                ..Default::default()
            }
            .notification()
            .as_deref(),
            Some("2 emails, 1 event, 4 notes backed up")
        );
    }

    #[test]
    fn cap_status_line_never_contains_the_token() {
        // AUDIT-1 (#72) regression freeze: the startup line announces only that the
        // capability gate is enabled + the token length, never the token itself.
        let token = "TOPSECRET-do-not-ever-log-this-cap-token";
        let line = cap_status_line(token.len());
        assert!(
            !line.contains(token),
            "startup line leaked the token: {line}"
        );
        assert!(
            !line.contains("TOPSECRET"),
            "startup line leaked token bytes: {line}"
        );
        assert!(
            line.contains(&format!("{} bytes", token.len())),
            "should report the length: {line}"
        );
    }

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
                token_refresh_secs: 21_600,
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
    fn parses_token_refresh_secs() {
        // keep-alive defaults to 6h and is tunable / disablable
        assert_eq!(
            Args::try_parse_from(["isyncyoud"])
                .unwrap()
                .token_refresh_secs,
            21_600
        );
        let a = Args::try_parse_from(["isyncyoud", "--token-refresh-secs", "0"]).unwrap();
        assert_eq!(a.token_refresh_secs, 0);
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

    #[cfg(target_os = "linux")]
    #[test]
    fn fuse_overlay_status_maps_placeholder_materialized_syncing() {
        use isyncyou_dbus_status::OverlayStatus;
        use isyncyou_store::Item;
        let dir = std::env::temp_dir().join(format!("isy-overlay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mount = dir.join("OneDrive-cloud");
        let cache = dir.join("cache");
        std::fs::create_dir_all(&cache).unwrap();

        let folder = Item::new("a", "onedrive", "F1", "Docs", "folder");
        let mut f1 = Item::new("a", "onedrive", "f1", "note.txt", "file");
        f1.parent_remote_id = Some("F1".into());
        f1.size = Some(7);
        let items = vec![folder, f1];
        let index = Arc::new(isyncyou_fuse::PlaceholderIndex::from_items(&items));
        let info = FuseMountInfo {
            mount_point: mount.clone(),
            cache_dir: cache.clone(),
            index,
        };
        let tracker = HydrationTracker::new();

        // not yet materialized → Placeholder
        let file_path = mount.join("Docs").join("note.txt");
        assert_eq!(
            info.status(&file_path, &tracker),
            OverlayStatus::Placeholder
        );
        // a folder is a cloud container → Placeholder
        assert_eq!(
            info.status(&mount.join("Docs"), &tracker),
            OverlayStatus::Placeholder
        );
        // while hydrating (remote id active) → Syncing
        isyncyou_fuse::HydrationObserver::on_start(&tracker, "note.txt", "f1");
        assert_eq!(info.status(&file_path, &tracker), OverlayStatus::Syncing);
        isyncyou_fuse::HydrationObserver::on_done(&tracker, "note.txt", "f1", true);
        // cache file present → Materialized
        std::fs::write(cache.join(isyncyou_fuse::cache_file_name("f1")), b"hello").unwrap();
        assert_eq!(
            info.status(&file_path, &tracker),
            OverlayStatus::Materialized
        );
        // a path the index doesn't know → Unknown
        assert_eq!(
            info.status(&mount.join("nope.bin"), &tracker),
            OverlayStatus::Unknown
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn file_uri_and_xml_escape_encode_special_chars() {
        assert_eq!(
            path_to_file_uri(Path::new("/home/u/One Drive")),
            "file:///home/u/One%20Drive"
        );
        assert_eq!(
            path_to_file_uri(Path::new("/a/b-c_d.e~f")),
            "file:///a/b-c_d.e~f"
        );
        assert_eq!(
            xml_escape("a & b <c> \"d\""),
            "a &amp; b &lt;c&gt; &quot;d&quot;"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ensure_place_adds_exactly_one_entry_and_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("isy-places-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let xbel = dir.join("user-places.xbel");
        let mount = Path::new("/home/u/OneDrive");

        // first call creates the file with one bookmark
        assert!(ensure_place_in(&xbel, mount, "OneDrive", "folder-cloud").unwrap());
        let c1 = std::fs::read_to_string(&xbel).unwrap();
        assert_eq!(c1.matches("<bookmark ").count(), 1);
        assert!(c1.contains("href=\"file:///home/u/OneDrive\""));
        assert!(c1.contains("<title>OneDrive</title>"));
        assert!(c1.trim_end().ends_with("</xbel>"));

        // second call is a no-op (keyed on href) → still exactly one entry
        assert!(!ensure_place_in(&xbel, mount, "OneDrive", "folder-cloud").unwrap());
        assert_eq!(
            std::fs::read_to_string(&xbel)
                .unwrap()
                .matches("<bookmark ")
                .count(),
            1
        );

        // a *different* mount appends a second, distinct entry without clobbering
        let other = Path::new("/home/u/OneDrive-Work");
        assert!(ensure_place_in(&xbel, other, "OneDrive Work", "folder-cloud").unwrap());
        let c3 = std::fs::read_to_string(&xbel).unwrap();
        assert_eq!(c3.matches("<bookmark ").count(), 2);
        assert!(c3.contains("href=\"file:///home/u/OneDrive\""));
        assert!(c3.contains("href=\"file:///home/u/OneDrive-Work\""));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
