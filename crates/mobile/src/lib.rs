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
use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// The single configured account id on the phone. The user signs in to it via the
/// account menu's device-code flow; its token cache + store live under `filesDir`.
const ACCOUNT: &str = "me";

struct EngineState {
    session_token: String,
    /// The live router. In the default build it is reached **only** in-process (the
    /// message bridge + `shouldInterceptRequest` asset path) — no TCP port is bound, so no
    /// other app on the device can reach it (#0A netstat AC). A loopback server is bound
    /// **only** under the experimental agent-subscription feature (whose OAuth flow needs a
    /// `http://127.0.0.1/callback` redirect target); its port is not retained here.
    router: Arc<isyncyou_webui::Router>,
}

/// Process-global engine handle. `start` is idempotent (Activity recreation must not
/// start a second server) — a second call returns the already-bound port.
static ENGINE: OnceLock<Mutex<Option<EngineState>>> = OnceLock::new();

fn cell() -> &'static Mutex<Option<EngineState>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

/// Host label for keep-both conflict copies (`*-<host>-safeBackup-NNNN`) written by the
/// offline pass on this device.
const HOST: &str = "phone";

/// Process-global device transfer conditions (metered / charging / free bytes), pushed from
/// Kotlin via `nativeDeviceState` and read by the offline pass's policy gate (#655). Defaults
/// to the fail-open baseline (unmetered, charging, ample space) until the first push, so the
/// first materialize is never blocked before a real reading arrives.
static DEVICE_STATE: OnceLock<Mutex<isyncyou_core::policy::DeviceState>> = OnceLock::new();

fn device_state_cell() -> &'static Mutex<isyncyou_core::policy::DeviceState> {
    DEVICE_STATE.get_or_init(|| Mutex::new(isyncyou_core::policy::DeviceState::always_on(u64::MAX)))
}

/// Update the cached device state (called from the Android platform layer via JNI). Host-
/// testable (no JNI); the JNI entry is a thin wrapper.
pub fn set_device_state(metered: bool, charging: bool, free_bytes: u64) {
    let mut g = device_state_cell()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *g = isyncyou_core::policy::DeviceState {
        metered,
        charging,
        free_bytes,
    };
}

fn current_device_state() -> isyncyou_core::policy::DeviceState {
    *device_state_cell()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Start the embedded engine if not already running. Idempotent. Host-testable (no JNI):
/// the JNI entry is a thin wrapper. No loopback port is bound in the default build (#0A);
/// the UI reaches the engine only in-process (bridge + asset path).
pub fn start_engine(files_dir: &str) -> Result<(), String> {
    let mut guard = cell().lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_some() {
        return Ok(()); // already running — idempotent
    }
    let (session_token, router) = start_inner(files_dir)?;
    *guard = Some(EngineState {
        session_token,
        router,
    });
    Ok(())
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
        None => r#"{"t":"res","id":null,"status":503,"body":"{\"error\":\"engine not started\"}"}"#
            .to_string(),
    }
}

/// Answer one browser-initiated GET subresource (#0A) — the static shell and any
/// `img`/`iframe`/viewer the WebView loads itself — for Kotlin's `shouldInterceptRequest`.
/// **Binary-safe** (unlike the JSON bridge envelope, which is text-only) and header-faithful
/// so a viewer's per-response `Content-Security-Policy` survives. Frame:
/// `[status:u16 BE][ct_len:u16 BE][content_type][hdr_len:u16 BE][headers "K: V\r\n"…][body]`.
/// Cookie-gated exactly like the loopback path. Empty vec when the engine hasn't started.
pub fn asset_request(path: &str, cookie: Option<String>) -> Vec<u8> {
    let router = {
        let guard = cell().lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map(|s| Arc::clone(&s.router))
    };
    let Some(router) = router else {
        return Vec::new();
    };
    let resp =
        isyncyou_webui::dispatch_message(&router, "GET", path, None, None, cookie, Vec::new());
    let ct = resp.content_type.as_bytes();
    // Extra response headers (e.g. the viewer's CSP) as "Key: Value\r\n", CRLF-sanitised.
    let mut hdrs = String::new();
    for (k, v) in &resp.headers {
        let v = v.replace(['\r', '\n'], " ");
        hdrs.push_str(&format!("{k}: {v}\r\n"));
    }
    let hdrs = hdrs.as_bytes();
    let mut out = Vec::with_capacity(6 + ct.len() + hdrs.len() + resp.body.len());
    out.extend_from_slice(&resp.status.to_be_bytes());
    out.extend_from_slice(&(ct.len() as u16).to_be_bytes());
    out.extend_from_slice(ct);
    out.extend_from_slice(&(hdrs.len() as u16).to_be_bytes());
    out.extend_from_slice(hdrs);
    out.extend_from_slice(&resp.body);
    out
}

// ---------------------------------------------------------------- push streams (#0A)
// The bridge's SSE replacement: each open stream is a `Receiver<String>` of ready-to-embed
// JSON event objects. Kotlin runs one thread per stream: open → loop next → close. Single
// consumer per stream, so `next` takes the receiver out of the registry for the (blocking)
// recv and puts it back — `open`/`close` stay responsive. `close` while a `next` is
// in-flight tombstones the id so the receiver is dropped on return.

#[derive(Default)]
struct StreamRegistry {
    rx: HashMap<i64, Receiver<String>>,
    closed: HashSet<i64>,
}
static STREAMS: OnceLock<Mutex<StreamRegistry>> = OnceLock::new();
static STREAM_SEQ: AtomicU64 = AtomicU64::new(1);

fn streams() -> &'static Mutex<StreamRegistry> {
    STREAMS.get_or_init(|| Mutex::new(StreamRegistry::default()))
}

/// Open a bridge push stream (#0A) for `path`, session-gated. Returns a stream id (>0), or
/// 0 when the engine hasn't started / the stream is unknown / the session is unauthorized.
pub fn stream_open(path: &str, session_token: Option<&str>) -> i64 {
    let router = {
        let guard = cell().lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map(|s| Arc::clone(&s.router))
    };
    let Some(router) = router else {
        return 0;
    };
    let Some(rx) = router.open_bridge_stream(path, session_token) else {
        return 0;
    };
    let id = STREAM_SEQ.fetch_add(1, Ordering::SeqCst) as i64;
    streams()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .rx
        .insert(id, rx);
    id
}

/// Block for the next event on stream `id` (a JSON `{event,data}` object). Returns "" when
/// the stream ended, was closed, or is unknown; a `ping` heartbeat on idle timeout.
pub fn stream_next(id: i64) -> String {
    let rx = {
        let mut reg = streams().lock().unwrap_or_else(|e| e.into_inner());
        if reg.closed.remove(&id) {
            reg.rx.remove(&id);
            return String::new();
        }
        match reg.rx.remove(&id) {
            Some(rx) => rx,
            None => return String::new(),
        }
    };
    let outcome = rx.recv_timeout(Duration::from_secs(20));
    let mut reg = streams().lock().unwrap_or_else(|e| e.into_inner());
    if reg.closed.remove(&id) {
        return String::new(); // closed during recv → drop the receiver, end the stream
    }
    match outcome {
        Ok(s) => {
            reg.rx.insert(id, rx);
            s
        }
        Err(RecvTimeoutError::Timeout) => {
            reg.rx.insert(id, rx);
            r#"{"event":"ping","data":""}"#.to_string()
        }
        Err(RecvTimeoutError::Disconnected) => String::new(), // source ended → drop, don't reinsert
    }
}

/// Close a bridge push stream. If a `next` is currently blocked on it, the stream is
/// tombstoned so the receiver is dropped when that `next` returns.
pub fn stream_close(id: i64) {
    let mut reg = streams().lock().unwrap_or_else(|e| e.into_inner());
    if reg.rx.remove(&id).is_none() {
        reg.closed.insert(id);
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

/// Record a successful native `BiometricPrompt` for a pending destructive action
/// (#onedrive-mobile 0.6). Kotlin calls this ONLY after the biometric succeeds; the
/// WebView has no route to it, which is what makes the per-action token a real second
/// factor even though the UI holds every cap-token. Returns `false` when the engine
/// hasn't started or the id is unknown/expired.
pub fn confirm_action(pending_id: &str) -> bool {
    let router = {
        let guard = cell().lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map(|s| Arc::clone(&s.router))
    };
    match router {
        Some(r) => r.confirm_biometric(pending_id),
        None => false,
    }
}

fn start_inner(files_dir: &str) -> Result<(String, Arc<isyncyou_webui::Router>), String> {
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

    let mut cfg = Config {
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
    // A phone is a cache/live companion, not the desktop's 5 s near-real-time loop:
    // poll every 30 s to spare battery/mobile-data and cut the periodic view refresh
    // frequency (the live-refresh is otherwise driven every poll a change lands).
    cfg.sync.poll_interval_secs = 30;
    // Persist so DaemonSettings (which reads/writes the config file) works on-device.
    cfg.save(&config_path)?;

    // An unguessable per-process token gating the whole data API (#89 P1).
    let session_token = isyncyou_app_host::mint_cap_token();
    // The store-access gate: serialize the per-request store opens against the
    // cache-refresh thread (the store holds a single-instance lock).
    let gate = Arc::new(Mutex::new(()));
    let events = Arc::new(isyncyou_webui::EventBus::new());
    let live_interval = Arc::new(AtomicU64::new(cfg.sync.poll_interval_secs.max(1)));
    // The Mode-3 offline pass (refresh loop) writes per-file progress here; the router reads
    // the same handle at GET /api/v1/onedrive/transfers (#655). One shared instance, cloned.
    let transfer_progress = isyncyou_app_host::SharedProgress::new();

    let router = Arc::new(
        isyncyou_app_host::build_live_router(
            cfg.clone(),
            Some(gate.clone()),
            events.clone(),
            config_path,
            live_interval.clone(),
            transfer_progress.clone(),
        )
        .with_session_token(session_token.clone())
        // #onedrive-mobile 0.6: only the standalone Android app arms the biometric gate.
        // Destructive routes then require a per-action token that is valid only after a
        // native BiometricPrompt (confirmed over `nativeConfirmAction`, below).
        .with_biometric_gate(),
    );

    // #0A: NO loopback TCP port in the default build — the WebView reaches the engine only
    // in-process (the message bridge for data, `shouldInterceptRequest`→`asset_request` for
    // GET assets), so nothing is reachable by another app on the device. A loopback server
    // is bound ONLY under the experimental agent-subscription feature, whose device-code
    // OAuth flow returns to a `http://127.0.0.1:<port>/callback` redirect the browser hits.
    #[cfg(feature = "agent-subscription-experimental")]
    {
        let listener = isyncyou_webui::bind_loopback("127.0.0.1:0").map_err(|e| e.to_string())?;
        // Serve on a background thread, panic-isolated: a request-handling panic must never
        // take down the host app process. Shares the same router as the in-process bridge.
        let serve_router = Arc::clone(&router);
        std::thread::spawn(move || {
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
                let _ = isyncyou_webui::serve_listener_shared(listener, serve_router);
            }));
        });
    }

    // Cache-refresh thread (#89 P2): once the account is signed in, periodically pull
    // mail/calendar/contacts/todo/onenote from Graph into the local cache store
    // (read-only — never writes back to the cloud) and wake SSE subscribers so the
    // UI refreshes. Skips silently until a token is cached.
    std::thread::spawn(move || refresh_loop(cfg, gate, events, live_interval, transfer_progress));

    Ok((session_token, router))
}

/// One Mode-3 offline pass under the store-access gate (#655): materialize the offline scopes
/// and mirror local edits back over the ledger. Returns whether anything changed (the UI is
/// woken only on real work). Skips quietly with no offline folders configured or no cached
/// write token (not signed in). `progress` is the shared tracker the router surfaces.
fn run_offline_pass(
    cfg: &Config,
    gate: &Arc<Mutex<()>>,
    progress: &isyncyou_app_host::SharedProgress,
) -> bool {
    let has_offline = cfg
        .onedrive_modes
        .get(ACCOUNT)
        .map(|m| {
            m.folder_modes
                .values()
                .any(|md| *md == isyncyou_core::OneDriveMode::Offline)
        })
        .unwrap_or(false);
    if !has_offline {
        return false;
    }
    let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
    let token = match isyncyou_engine::auth::resolve_cached_sync_token(cfg, ACCOUNT) {
        Ok(t) => t,
        Err(_) => return false, // no write token cached → skip quietly
    };
    let dev = current_device_state();
    match isyncyou_engine::offline_sync_once_for(cfg, ACCOUNT, HOST, token, dev, progress) {
        Ok(r) => {
            r.downloaded
                + r.dirs_created
                + r.uploaded_creates
                + r.modified_uploaded
                + r.cloud_deleted
                + r.local_trashed
                > 0
        }
        Err(_) => false,
    }
}

fn refresh_loop(
    cfg: Config,
    gate: Arc<Mutex<()>>,
    events: Arc<isyncyou_webui::EventBus>,
    interval: Arc<AtomicU64>,
    progress: isyncyou_app_host::SharedProgress,
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
            // #655: after the read-only cache refresh, run the Mode-3 offline pass (materialize +
            // ledger writeback) for any offline folders, then wake SSE if either changed anything.
            let offline_changed = run_offline_pass(&cfg, &gate, &progress);
            if refreshed || offline_changed {
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
    // The default build binds no loopback port, so there is no port to return: 1 = started,
    // -1 = failed. Kotlin only checks `> 0` to know the engine is up (#0A).
    match start_engine(&dir) {
        Ok(()) => 1,
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

/// JNI: push the current device transfer conditions from the Android platform layer — the
/// active network is metered, the device is charging, and the free bytes on the sync volume —
/// read by the offline pass's policy gate (#655). May be called any time; the latest wins.
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeDeviceState(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
    metered: jni::sys::jboolean,
    charging: jni::sys::jboolean,
    free_bytes: jni::sys::jlong,
) {
    set_device_state(metered != 0, charging != 0, free_bytes.max(0) as u64);
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
    let resp =
        std::panic::catch_unwind(AssertUnwindSafe(|| bridge_request(&req))).unwrap_or_else(|_| {
            r#"{"t":"res","id":null,"status":500,"body":"{\"error\":\"internal error\"}"}"#
                .to_string()
        });
    env.new_string(resp)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// JNI: answer one browser-initiated GET subresource (#0A) for `shouldInterceptRequest`,
/// returning the framed bytes (see [`asset_request`]). Binary-safe. Never logs the cookie.
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeAssetRequest(
    mut env: jni::JNIEnv,
    _class: jni::objects::JClass,
    path: jni::objects::JString,
    cookie: jni::objects::JString,
) -> jni::sys::jbyteArray {
    let path: String = match env.get_string(&path) {
        Ok(s) => s.into(),
        Err(_) => return std::ptr::null_mut(),
    };
    let cookie: String = env.get_string(&cookie).map(Into::into).unwrap_or_default();
    let cookie = if cookie.is_empty() {
        None
    } else {
        Some(cookie)
    };
    let bytes = std::panic::catch_unwind(AssertUnwindSafe(|| asset_request(&path, cookie)))
        .unwrap_or_default();
    env.byte_array_from_slice(&bytes)
        .map(|a| a.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// JNI: open a bridge push stream (#0A), returning a stream id (>0) or 0. The session
/// token is passed explicitly (the WebView can't set headers on a native stream open).
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeStreamOpen(
    mut env: jni::JNIEnv,
    _class: jni::objects::JClass,
    path: jni::objects::JString,
    session_token: jni::objects::JString,
) -> jni::sys::jlong {
    let path: String = match env.get_string(&path) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };
    let tok: String = env
        .get_string(&session_token)
        .map(Into::into)
        .unwrap_or_default();
    let tok = if tok.is_empty() { None } else { Some(tok) };
    std::panic::catch_unwind(AssertUnwindSafe(|| stream_open(&path, tok.as_deref()))).unwrap_or(0)
}

/// JNI: block for the next event on stream `id` (a JSON `{event,data}` object), or "" when
/// the stream ended/closed. Kotlin's per-stream thread loops on this. Never logs.
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeStreamNext(
    env: jni::JNIEnv,
    _class: jni::objects::JClass,
    id: jni::sys::jlong,
) -> jni::sys::jstring {
    let out = std::panic::catch_unwind(AssertUnwindSafe(|| stream_next(id))).unwrap_or_default();
    env.new_string(out)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// JNI: close a bridge push stream.
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeStreamClose(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
    id: jni::sys::jlong,
) {
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| stream_close(id)));
}

/// JNI: install the at-rest body key (#0B) — the 32-byte data key the Android Keystore
/// unwrapped, plus its `key_id` for rotation. MUST be called before [`nativeStart`] so the
/// first body write/read is already sealed. SECURITY: the key bytes are used in-process
/// only (ring needs raw key material); they are never logged. Returns 1 on success, 0 on a
/// bad length so Kotlin can surface a setup failure.
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeSetBodyKey(
    env: jni::JNIEnv,
    _class: jni::objects::JClass,
    key_id: jni::sys::jint,
    key: jni::objects::JByteArray,
) -> jni::sys::jint {
    let bytes = match env.convert_byte_array(&key) {
        Ok(b) => b,
        Err(_) => return 0,
    };
    if bytes.len() != 32 {
        return 0;
    }
    let mut k = [0u8; 32];
    k.copy_from_slice(&bytes);
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        // One Keystore-unwrapped data key protects both: the body-file envelope (#0B) AND
        // the SQLCipher store DB (its PRAGMA key). Both installed before the engine opens
        // the store or writes a body.
        isyncyou_core::envelope::set_body_key(key_id as u32, k);
        isyncyou_store::set_store_key(k.to_vec());
    }));
    1
}

/// JNI: record a successful native `BiometricPrompt` for a pending destructive action
/// (#onedrive-mobile 0.6). Kotlin calls this ONLY from the biometric success callback, so
/// the confirmation cannot originate in the WebView (which holds every cap-token). Returns
/// 1 when the pending id was found and armed, 0 otherwise (unknown/expired/engine down).
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeConfirmAction(
    mut env: jni::JNIEnv,
    _class: jni::objects::JClass,
    pending_id: jni::objects::JString,
) -> jni::sys::jboolean {
    let id: String = match env.get_string(&pending_id) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };
    let ok = std::panic::catch_unwind(AssertUnwindSafe(|| confirm_action(&id))).unwrap_or(false);
    u8::from(ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_engine_is_idempotent_and_mints_a_session_token() {
        // Host test of the non-JNI core (#89 P4 / #0A): start succeeds, mints a session
        // token, and a second call reuses the SAME running engine (Activity recreation must
        // not start a second one). No loopback port is bound in the default build.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        start_engine(path).expect("engine starts");
        let tok1 = session_token().expect("token present");
        assert!(!tok1.is_empty(), "session token must be set");
        start_engine(path).expect("idempotent restart");
        let tok2 = session_token().expect("token present");
        assert_eq!(tok1, tok2, "second start must reuse the running engine");
    }

    #[test]
    fn device_state_push_updates_the_global() {
        // Kotlin pushes the device transfer conditions over nativeDeviceState → the offline
        // pass's policy gate reads them here. (Process-global — assert on the values we set.)
        set_device_state(true, false, 4096);
        let d = current_device_state();
        assert!(d.metered);
        assert!(!d.charging);
        assert_eq!(d.free_bytes, 4096);
    }

    #[test]
    fn standalone_serves_ui_and_gates_the_api_in_process() {
        // #89 P7 / #0A (host slice): the embedded engine — the exact code that runs on the
        // phone — serves the UI shell and fully session-token gates the data API **entirely
        // in-process**, with NO loopback TCP port. `asset_request` serves the shell (as the
        // WebView's shouldInterceptRequest does); `bridge_request` carries the data API.
        let dir = tempfile::tempdir().unwrap();
        start_engine(dir.path().to_str().unwrap()).expect("engine starts");
        let tok = session_token().expect("token");

        // The UI shell is served in-process (binary-safe asset frame, status in the head).
        let shell = asset_request("/", None);
        assert!(shell.len() > 6, "shell framed response");
        assert_eq!(
            u16::from_be_bytes([shell[0], shell[1]]),
            200,
            "engine must serve the UI shell in-process"
        );
        // Data route without the session token → 401 (the Android-exposure gate, now over
        // the bridge rather than an open port).
        let no_tok = bridge_request(
            r#"{"method":"GET","path":"/api/v1/items?account=me&service=mail","headers":{}}"#,
        );
        assert!(
            no_tok.contains("\"status\":401"),
            "data API must be gated: {no_tok}"
        );
        // With the session token → reaches the handler (not a 401).
        let with_tok = bridge_request(&format!(
            r#"{{"method":"GET","path":"/api/v1/items?account=me&service=mail","headers":{{"X-Session-Token":"{tok}"}}}}"#
        ));
        assert!(
            !with_tok.contains("\"status\":401"),
            "valid token must pass: {with_tok}"
        );
        // Restore is absent in the mobile profile (cache, not backup-of-record) → 404.
        let restore = bridge_request(&format!(
            r#"{{"method":"POST","path":"/api/v1/restore?account=me&service=mail&id=x","headers":{{"X-Session-Token":"{tok}"}}}}"#
        ));
        assert!(
            restore.contains("\"status\":404"),
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
        assert!(
            denied.contains("\"status\":401"),
            "bridge must gate: {denied}"
        );
        // With the token → reaches the handler (not a 401).
        let ok = bridge_request(&format!(
            r#"{{"method":"GET","path":"/api/v1/items?account=me&service=mail","headers":{{"X-Session-Token":"{tok}"}}}}"#
        ));
        assert!(
            !ok.contains("\"status\":401"),
            "valid token must pass: {ok}"
        );
    }

    #[test]
    fn asset_request_serves_the_shell_framed_binary_safe() {
        // #0A: browser-initiated GETs (shell + subresources) are served binary-safe with
        // an explicit content-type, so images/viewers survive intact (no lossy UTF-8).
        let dir = tempfile::tempdir().unwrap();
        start_engine(dir.path().to_str().unwrap()).expect("engine starts");
        let framed = asset_request("/", None);
        assert!(framed.len() > 6, "framed response has a header");
        let status = u16::from_be_bytes([framed[0], framed[1]]);
        assert_eq!(status, 200, "the shell serves 200");
        let ctlen = u16::from_be_bytes([framed[2], framed[3]]) as usize;
        let ct = String::from_utf8_lossy(&framed[4..4 + ctlen]);
        assert!(ct.contains("text/html"), "shell content-type: {ct}");
        let hdr_off = 4 + ctlen;
        let hdrlen = u16::from_be_bytes([framed[hdr_off], framed[hdr_off + 1]]) as usize;
        let body = &framed[hdr_off + 2 + hdrlen..];
        assert!(
            String::from_utf8_lossy(body).contains("<"),
            "shell body is HTML"
        );
    }

    #[test]
    fn stream_registry_opens_gated_and_closes() {
        // #0A: the push-stream FFI plumbing — gating + open/close registry. Event delivery
        // semantics are proven in webui's open_bridge_stream test; the full push round-trip
        // is device-verified.
        let dir = tempfile::tempdir().unwrap();
        start_engine(dir.path().to_str().unwrap()).expect("engine starts");
        let tok = session_token().expect("token");
        assert_eq!(
            stream_open("/api/v1/events", None),
            0,
            "unauthorized → no stream"
        );
        assert_eq!(
            stream_open("/api/v1/nope", Some(&tok)),
            0,
            "unknown path → no stream"
        );
        let id = stream_open("/api/v1/events", Some(&tok));
        assert!(id > 0, "authorized events stream opens");
        stream_close(id);
        assert_eq!(stream_next(id), "", "a closed stream yields nothing");
    }
}
