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
    /// The live router, shared with the loopback server. Held so the in-process message
    /// bridge (#0A) answers requests against the **same** router (no second TCP port).
    router: Arc<isyncyou_webui::Router>,
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
    let (port, session_token, router) = start_inner(files_dir)?;
    *guard = Some(EngineState {
        port,
        session_token,
        router,
    });
    Ok(port)
}

/// Handle one JSON-framed request from the Android in-process bridge (#0A) against the
/// running engine's router — **no loopback TCP port involved**. Returns a JSON response
/// envelope, or an error envelope when the engine hasn't started. Host-testable.
pub fn bridge_request(request_json: &str) -> String {
    let router = {
        let guard = cell().lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map(|s| Arc::clone(&s.router))
    };
    match router {
        Some(router) => isyncyou_webui::handle_bridge_request(&router, request_json),
        None => r#"{"status":503,"body":"{\"error\":\"engine not started\"}"}"#.to_string(),
    }
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

fn start_inner(files_dir: &str) -> Result<(u16, String, Arc<isyncyou_webui::Router>), String> {
    let base = PathBuf::from(files_dir);
    let archive_root = base.join("archive");
    let sync_root = base.join("sync");
    // OneDrive online/sync-mode lazy previews live here, apart from the offline working
    // copy in sync_root (#onedrive-mobile 0C); the writeback scanner ignores it.
    let cache_root = base.join("cache");
    std::fs::create_dir_all(&archive_root).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&sync_root).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&cache_root).map_err(|e| e.to_string())?;
    let config_path = base.join("isyncyou.toml");

    let cfg = Config {
        accounts: vec![AccountConfig {
            id: ACCOUNT.into(),
            username: ACCOUNT.into(),
            sync_root,
            archive_root,
            cache_root,
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

    let router = Arc::new(
        isyncyou_app_host::build_live_router(
            cfg.clone(),
            Some(gate.clone()),
            events.clone(),
            config_path,
            live_interval.clone(),
        )
        .with_session_token(session_token.clone()),
    );

    // OS-assigned free loopback port (read it back before serving).
    let listener = isyncyou_webui::bind_loopback("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();

    // Serve on a background thread, panic-isolated: a request-handling panic must
    // never take down the host app process. The same router is shared with the
    // in-process bridge (#0A) via the stored `Arc` clone.
    let serve_router = Arc::clone(&router);
    std::thread::spawn(move || {
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = isyncyou_webui::serve_listener_shared(listener, serve_router);
        }));
    });

    // Cache-refresh thread (#89 P2): once the account is signed in, periodically pull
    // mail/calendar/contacts/todo/onenote from Graph into the local cache store
    // (read-only — never writes back to the cloud) and wake SSE subscribers so the
    // UI refreshes. Skips silently until a token is cached.
    std::thread::spawn(move || refresh_loop(cfg, gate, events, live_interval));

    Ok((port, session_token, router))
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
                        // Notify the UI ONLY when the pass actually changed something —
                        // a no-op refresh (the common idle case every poll_interval_secs)
                        // must not wake SSE, or the whole view reloads periodically (the
                        // visible "screen flicker"). `.changed()` is false for a no-op.
                        isyncyou_engine::refresh_cache_account(&cfg, ACCOUNT, read, write)
                            .map(|counts| counts.changed())
                            .unwrap_or(false)
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

/// JNI: answer one in-process bridge request (#0A). Kotlin passes the JSON request
/// envelope from the `WebMessageListener` and posts the returned JSON envelope back on the
/// message port — no loopback TCP port is used. SECURITY: never logs tokens or bodies.
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeBridgeRequest(
    mut env: jni::JNIEnv,
    _class: jni::objects::JClass,
    request_json: jni::objects::JString,
) -> jni::sys::jstring {
    let req: String = match env.get_string(&request_json) {
        Ok(s) => s.into(),
        Err(_) => return std::ptr::null_mut(),
    };
    // Panic-isolate: a request-handling panic must never unwind across the FFI boundary.
    let resp = std::panic::catch_unwind(AssertUnwindSafe(|| bridge_request(&req)))
        .unwrap_or_else(|_| r#"{"status":500,"body":"{\"error\":\"internal error\"}"}"#.to_string());
    env.new_string(resp)
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

    #[test]
    fn standalone_serves_ui_and_gates_the_api_end_to_end() {
        // #89 P7 (host slice): the embedded engine — the exact code that runs on the
        // phone — serves the web UI over loopback and fully session-token gates the
        // data API. The WebView visual + device-code login + over-LTE render are the
        // genuinely device-bound parts; the engine/serving/gating is proven here.
        use std::io::{Read, Write};
        use std::net::TcpStream;
        let dir = tempfile::tempdir().unwrap();
        let port = start_engine(dir.path().to_str().unwrap()).expect("engine starts");
        let tok = session_token().expect("token");

        let req = |raw: &str| {
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            c.write_all(raw.as_bytes()).unwrap();
            let mut s = String::new();
            c.read_to_string(&mut s).unwrap();
            s
        };
        // The UI shell is served by the embedded engine (no daemon).
        let shell = req("GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert!(
            shell.starts_with("HTTP/1.1 200"),
            "engine must serve the UI: {shell}"
        );
        // Data route without the session token → 401 (the Android-loopback fix).
        let no_tok =
            req("GET /api/v1/items?account=me&service=mail HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert!(
            no_tok.starts_with("HTTP/1.1 401"),
            "data API must be gated: {no_tok}"
        );
        // With the session token → reaches the handler (not a 401).
        let with_tok = req(&format!(
            "GET /api/v1/items?account=me&service=mail HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Session-Token: {tok}\r\n\r\n"
        ));
        assert!(
            !with_tok.starts_with("HTTP/1.1 401"),
            "valid token must pass: {with_tok}"
        );
        // Restore is absent in the mobile profile (cache, not backup-of-record) → 404.
        let restore = req(&format!(
            "POST /api/v1/restore?account=me&service=mail&id=x HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Session-Token: {tok}\r\n\r\n"
        ));
        assert!(
            restore.starts_with("HTTP/1.1 404"),
            "restore must be absent on mobile: {restore}"
        );
    }

    #[test]
    fn bridge_request_routes_against_the_running_engine_without_a_port() {
        // #0A: the in-process bridge answers against the same router as loopback and
        // enforces the same session gate — proving the phone needs no TCP port to serve
        // its own UI's data calls.
        let dir = tempfile::tempdir().unwrap();
        start_engine(dir.path().to_str().unwrap()).expect("engine starts");
        let tok = session_token().expect("token");
        // No session token → 401 envelope (the loopback-exposure gate applies here too).
        let denied = bridge_request(
            r#"{"method":"GET","path":"/api/v1/items?account=me&service=mail","headers":{}}"#,
        );
        assert!(denied.contains("\"status\":401"), "bridge must gate: {denied}");
        // With the token → reaches the handler (not a 401).
        let ok = bridge_request(&format!(
            r#"{{"method":"GET","path":"/api/v1/items?account=me&service=mail","headers":{{"X-Session-Token":"{tok}"}}}}"#
        ));
        assert!(!ok.contains("\"status\":401"), "valid token must pass: {ok}");
    }
}
