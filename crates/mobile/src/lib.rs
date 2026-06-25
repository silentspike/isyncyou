//! Standalone Android client (#89): runs the real iSyncYou engine **in the app
//! process**. A tiny JNI surface lets Kotlin start the embedded loopback server
//! (the same `build_live_router` the desktop daemon uses, in the live-companion
//! profile) and read the per-process session token; the app's WebView then loads
//! `http://127.0.0.1:<port>/`. No desktop daemon, no `adb reverse` — the phone is a
//! self-contained iSyncYou node over mobile data.
//!
//! SECURITY: the loopback API is fully session-token gated (#89 P1) because any app
//! on the device can reach `127.0.0.1`. The token is minted here, handed to Kotlin
//! over JNI (never served in a static asset), and required on every `/api/v1/*`
//! route. Tokens are NEVER logged.

use isyncyou_core::{AccountConfig, Config};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// The single configured account id on the phone. The user signs in to it via the
/// account menu's device-code flow; its token cache + store live under `filesDir`.
const ACCOUNT: &str = "me";

struct EngineState {
    port: u16,
    session_token: String,
}

/// Process-global engine handle. `start` is idempotent (Activity recreation must not
/// start a second server) — a second call returns the already-bound port.
static ENGINE: OnceLock<Mutex<Option<EngineState>>> = OnceLock::new();

fn cell() -> &'static Mutex<Option<EngineState>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

/// Start the embedded engine if not already running, returning the bound loopback
/// port. Idempotent. Host-testable (no JNI): the JNI entry is a thin wrapper.
pub fn start_engine(files_dir: &str) -> Result<u16, String> {
    let mut guard = cell().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(st) = guard.as_ref() {
        return Ok(st.port); // already running — reuse it
    }
    let (port, session_token) = start_inner(files_dir)?;
    *guard = Some(EngineState {
        port,
        session_token,
    });
    Ok(port)
}

/// The per-process session token Kotlin must hand to the WebView (header + cookie)
/// so the WebUI can reach the gated loopback API. `None` until the engine started.
pub fn session_token() -> Option<String> {
    cell()
        .lock()
        .ok()?
        .as_ref()
        .map(|s| s.session_token.clone())
}

fn start_inner(files_dir: &str) -> Result<(u16, String), String> {
    let base = PathBuf::from(files_dir);
    let archive_root = base.join("archive");
    let sync_root = base.join("sync");
    std::fs::create_dir_all(&archive_root).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&sync_root).map_err(|e| e.to_string())?;
    let config_path = base.join("isyncyou.toml");

    let cfg = Config {
        accounts: vec![AccountConfig {
            id: ACCOUNT.into(),
            username: ACCOUNT.into(),
            sync_root,
            archive_root,
            mount_point: None,
        }],
        ..Default::default()
    };
    // Persist so DaemonSettings (which reads/writes the config file) works on-device.
    cfg.save(&config_path)?;

    // An unguessable per-process token gating the whole data API (#89 P1).
    let session_token = isyncyou_app_host::mint_cap_token();
    // The store-access gate: serialize the per-request store opens against the
    // cache-refresh thread (the store holds a single-instance lock).
    let gate = Arc::new(Mutex::new(()));
    let events = Arc::new(isyncyou_webui::EventBus::new());
    let live_interval = Arc::new(AtomicU64::new(cfg.sync.poll_interval_secs.max(1)));

    let router = isyncyou_app_host::build_live_router(
        cfg.clone(),
        Some(gate.clone()),
        events.clone(),
        config_path,
        live_interval.clone(),
    )
    .with_session_token(session_token.clone());

    // OS-assigned free loopback port (read it back before serving).
    let listener = isyncyou_webui::bind_loopback("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();

    // Serve on a background thread, panic-isolated: a request-handling panic must
    // never take down the host app process.
    std::thread::spawn(move || {
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = isyncyou_webui::serve_listener(listener, router);
        }));
    });

    // Cache-refresh thread (#89 P2): once the account is signed in, periodically pull
    // mail/calendar/contacts/todo/onenote from Graph into the local cache store
    // (read-only — never writes back to the cloud) and wake SSE subscribers so the
    // UI refreshes. Skips silently until a token is cached.
    std::thread::spawn(move || refresh_loop(cfg, gate, events, live_interval));

    Ok((port, session_token))
}

fn refresh_loop(
    cfg: Config,
    gate: Arc<Mutex<()>>,
    events: Arc<isyncyou_webui::EventBus>,
    interval: Arc<AtomicU64>,
) {
    loop {
        let secs = interval.load(Ordering::Relaxed).max(5);
        std::thread::sleep(Duration::from_secs(secs));
        // Isolate a refresh panic so the loop (and the app) survives.
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let refreshed = {
                let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
                match isyncyou_engine::auth::resolve_cache_refresh_token(&cfg, ACCOUNT) {
                    Ok(read) => {
                        let write =
                            isyncyou_engine::auth::resolve_cached_restore_token(&cfg, ACCOUNT).ok();
                        isyncyou_engine::refresh_cache_account(&cfg, ACCOUNT, read, write).is_ok()
                    }
                    Err(_) => false, // not signed in yet — skip quietly
                }
            };
            if refreshed {
                events.notify(); // wake SSE subscribers so the UI refetches
            }
        }));
    }
}

// ----------------------------------------------------------------- JNI surface
// `com.silentspike.isyncyou.NativeEngine.nativeStart(filesDir)` -> bound port (or -1)
// `com.silentspike.isyncyou.NativeEngine.nativeSessionToken()` -> token string

/// JNI: start the engine, returning the bound loopback port (or -1 on error).
/// SECURITY: never logs the session token or any secret.
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeStart(
    mut env: jni::JNIEnv,
    _class: jni::objects::JClass,
    files_dir: jni::objects::JString,
) -> jni::sys::jint {
    let dir: String = match env.get_string(&files_dir) {
        Ok(s) => s.into(),
        Err(_) => return -1,
    };
    match start_engine(&dir) {
        Ok(port) => jni::sys::jint::from(port as i32),
        Err(_) => -1,
    }
}

/// JNI: the per-process session token Kotlin hands to the WebView (header + cookie).
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeSessionToken(
    env: jni::JNIEnv,
    _class: jni::objects::JClass,
) -> jni::sys::jstring {
    let tok = session_token().unwrap_or_default();
    env.new_string(tok)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_engine_binds_a_port_and_is_idempotent() {
        // Host test of the non-JNI core (#89 P4): start binds a loopback port and a
        // second call returns the SAME port (Activity recreation must not double-bind).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        let port1 = start_engine(path).expect("engine starts");
        assert!(port1 > 0, "must bind a real port");
        let port2 = start_engine(path).expect("idempotent restart");
        assert_eq!(port1, port2, "second start must reuse the running port");
        // The session token is set and non-empty (gates the data API).
        let tok = session_token().expect("token present");
        assert!(!tok.is_empty(), "session token must be set");
    }
}
