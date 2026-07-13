//! Standalone Android client (#89): runs the real iSyncYou engine **in the app
//! process**. A small JNI surface starts the same `build_live_router` used by the
//! desktop daemon. The app's WebView reaches that router only through the native
//! message/asset bridge at the appassets origin. No desktop daemon, loopback TCP
//! server, or `adb reverse` is involved.
//!
//! SECURITY: the data API remains session-token gated (#89 P1/#721). The token is
//! minted here, used only by trusted native request paths, and required on every
//! `/api/v1/*` route. Tokens are NEVER logged or exposed to WebView JavaScript.

use isyncyou_core::{AccountConfig, Config, OneDriveMode};
#[cfg(test)]
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

#[cfg(target_os = "android")]
fn android_info(message: &str) {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int};

    unsafe extern "C" {
        fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }

    let Ok(tag) = CString::new("iSyncYou") else {
        return;
    };
    let Ok(msg) = CString::new(message) else {
        return;
    };
    // ANDROID_LOG_INFO = 4
    unsafe {
        let _ = __android_log_write(4, tag.as_ptr(), msg.as_ptr());
    }
}

#[cfg(not(target_os = "android"))]
fn android_info(message: &str) {
    eprintln!("{message}");
}

/// The single configured account id on the phone. The user signs in to it via the
/// account menu's device-code flow; its token cache + store live under `filesDir`.
const ACCOUNT: &str = "me";

struct EngineState {
    session_token: String,
    /// The live router is reached only in-process through the message bridge and
    /// `shouldInterceptRequest` asset path. No TCP port is bound (#0A/#721).
    router: Arc<isyncyou_webui::Router>,
    mobile_jobs: Arc<isyncyou_app_host::MobileJobRuntime>,
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
#[cfg(not(test))]
static MOBILE_ENCRYPTION_READY: AtomicBool = AtomicBool::new(false);
#[cfg(not(test))]
static MOBILE_AGENT_CREDENTIAL_READY: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
thread_local! {
    static TEST_MOBILE_ENCRYPTION_READY: Cell<bool> = const { Cell::new(false) };
    static TEST_MOBILE_AGENT_CREDENTIAL_READY: Cell<bool> = const { Cell::new(false) };
}
#[cfg(test)]
static TEST_FAIL_NEXT_MOBILE_KEY_INSTALL: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static TEST_FAIL_NEXT_MOBILE_AGENT_CREDENTIAL_KEY_INSTALL: AtomicBool = AtomicBool::new(false);

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

fn mark_mobile_encryption_ready() {
    #[cfg(not(test))]
    MOBILE_ENCRYPTION_READY.store(true, Ordering::SeqCst);
    #[cfg(test)]
    TEST_MOBILE_ENCRYPTION_READY.with(|ready| ready.set(true));
}

fn mark_mobile_agent_credential_ready() {
    #[cfg(not(test))]
    MOBILE_AGENT_CREDENTIAL_READY.store(true, Ordering::SeqCst);
    #[cfg(test)]
    TEST_MOBILE_AGENT_CREDENTIAL_READY.with(|ready| ready.set(true));
}

fn mobile_encryption_ready() -> bool {
    #[cfg(not(test))]
    {
        MOBILE_ENCRYPTION_READY.load(Ordering::SeqCst)
    }
    #[cfg(test)]
    {
        TEST_MOBILE_ENCRYPTION_READY.with(Cell::get)
    }
}

fn mobile_agent_credential_ready() -> bool {
    #[cfg(not(test))]
    {
        MOBILE_AGENT_CREDENTIAL_READY.load(Ordering::SeqCst)
    }
    #[cfg(test)]
    {
        TEST_MOBILE_AGENT_CREDENTIAL_READY.with(Cell::get)
    }
}

#[cfg(test)]
fn reset_mobile_encryption_ready_for_tests() {
    TEST_MOBILE_ENCRYPTION_READY.with(|ready| ready.set(false));
}

#[cfg(test)]
fn reset_mobile_agent_credential_ready_for_tests() {
    TEST_MOBILE_AGENT_CREDENTIAL_READY.with(|ready| ready.set(false));
}

#[cfg(test)]
fn install_test_mobile_encryption() {
    let key = [9u8; 32];
    isyncyou_core::envelope::set_body_key(1, key);
    isyncyou_core::envelope::require_body_envelope_for_process();
    isyncyou_store::set_store_key(key.to_vec());
    isyncyou_store::require_store_key_for_process();
    isyncyou_agent::set_process_credential_key(key);
    mark_mobile_encryption_ready();
    mark_mobile_agent_credential_ready();
}

fn install_mobile_body_key(key_id: i32, bytes: &[u8]) -> bool {
    if key_id <= 0 || bytes.len() != 32 {
        return false;
    }
    let mut k = [0u8; 32];
    k.copy_from_slice(bytes);
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        #[cfg(test)]
        if TEST_FAIL_NEXT_MOBILE_KEY_INSTALL.swap(false, Ordering::SeqCst) {
            panic!("injected mobile key install failure");
        }
        // One Keystore-unwrapped data key protects both: the body-file envelope (#0B) AND
        // the SQLCipher store DB (its PRAGMA key). Both installed before the engine opens
        // the store or writes a body.
        isyncyou_core::envelope::set_body_key(key_id as u32, k);
        isyncyou_core::envelope::require_body_envelope_for_process();
        isyncyou_store::set_store_key(k.to_vec());
        isyncyou_store::require_store_key_for_process();
        mark_mobile_encryption_ready();
    }))
    .is_ok()
}

fn install_mobile_agent_credential_key(bytes: &[u8]) -> bool {
    if bytes.len() != 32 {
        return false;
    }
    let mut k = [0u8; 32];
    k.copy_from_slice(bytes);
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        #[cfg(test)]
        if TEST_FAIL_NEXT_MOBILE_AGENT_CREDENTIAL_KEY_INSTALL.swap(false, Ordering::SeqCst) {
            panic!("injected mobile agent credential key install failure");
        }
        isyncyou_agent::set_process_credential_key(k);
        mark_mobile_agent_credential_ready();
    }))
    .is_ok()
}

fn mobile_roots(base: &Path) -> (PathBuf, PathBuf, PathBuf) {
    (base.join("archive"), base.join("sync"), base.join("cache"))
}

fn has_plaintext_sqlite_header(path: &Path) -> std::io::Result<bool> {
    use std::io::Read;
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let mut magic = [0u8; 16];
    match f.read_exact(&mut magic) {
        Ok(()) => Ok(&magic == b"SQLite format 3\0"),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
}

fn cleanup_legacy_plaintext_mobile_state(base: &Path) -> Result<(), String> {
    let archive_root = base.join("archive");
    let db = archive_root.join(".isyncyou-store.db");
    if !has_plaintext_sqlite_header(&db).map_err(|e| e.to_string())? {
        return Ok(());
    }
    for p in [
        db.clone(),
        archive_root.join(".isyncyou-store.db-wal"),
        archive_root.join(".isyncyou-store.db-shm"),
    ] {
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    for p in [base.join("cache"), base.join("sync")] {
        match std::fs::remove_dir_all(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    isyncyou_core::obs::event(
        "mobile-encryption",
        "legacy-plaintext-cache-cleared",
        "scope=store-db-bodies",
    );
    Ok(())
}

fn prepare_mobile_config_for_files_dir(cfg: &mut Config, base: &Path) -> Result<(), String> {
    let (archive_root, sync_root, cache_root) = mobile_roots(base);
    if let Some(acc) = cfg.accounts.iter_mut().find(|a| a.id == ACCOUNT) {
        acc.sync_root = sync_root;
        acc.archive_root = archive_root;
        acc.cache_root = cache_root;
        acc.mount_point = None;
    } else {
        cfg.accounts.push(AccountConfig {
            id: ACCOUNT.into(),
            username: ACCOUNT.into(),
            sync_root,
            archive_root,
            cache_root,
            mount_point: None,
        });
    }
    cfg.validate().map_err(|errs| errs.join("; "))
}

#[cfg(feature = "agent-session-kdf-bench")]
pub fn agent_session_kdf_benchmark_json(iterations: usize) -> Result<String, String> {
    let iterations = iterations.clamp(1, 25);
    let profile = isyncyou_agent::KdfProfile::production([0x42; 16]);
    let config =
        isyncyou_agent::SessionCryptoConfig::new(profile.clone()).map_err(|e| e.to_string())?;
    let pairing_secret = vec![0xA7; 32];
    let mut micros = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = std::time::Instant::now();
        let session = isyncyou_agent::Session::new_with_crypto_config(
            "benchsess",
            pairing_secret.clone(),
            isyncyou_agent::InMemoryTransport::new(),
            config.clone(),
        )
        .map_err(|e| e.to_string())?;
        std::hint::black_box(session.session_id.as_str());
        micros.push(start.elapsed().as_micros() as u64);
    }
    micros.sort_unstable();
    let median = micros[micros.len() / 2];
    let p95_idx = ((micros.len() * 95).div_ceil(100))
        .saturating_sub(1)
        .min(micros.len() - 1);
    let p95 = micros[p95_idx];
    serde_json::to_string(&serde_json::json!({
        "benchmark": "agent_session_argon2id_hkdf",
        "scope": "jni_only_feature_gated",
        "iterations": iterations,
        "median_ms": (median as f64) / 1000.0,
        "p95_ms": (p95 as f64) / 1000.0,
        "samples_us": micros,
        "kdf": {
            "alg": profile.alg,
            "version": profile.version,
            "memory_kib": profile.memory_kib,
            "iterations": profile.iterations,
            "lanes": profile.lanes
        }
    }))
    .map_err(|e| e.to_string())
}

#[cfg(feature = "agent-credential-store-self-test")]
fn tree_contains_bytes(root: &Path, needle: &[u8]) -> Result<(bool, usize), String> {
    if !root.exists() {
        return Ok((false, 0));
    }
    let mut found = false;
    let mut files = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let meta = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if meta.is_dir() {
            for entry in std::fs::read_dir(&path).map_err(|e| e.to_string())? {
                stack.push(entry.map_err(|e| e.to_string())?.path());
            }
        } else if meta.is_file() {
            files += 1;
            let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
            if bytes.windows(needle.len()).any(|w| w == needle) {
                found = true;
            }
        }
    }
    Ok((found, files))
}

#[cfg(feature = "agent-credential-store-self-test")]
fn file_contains_bytes(path: &Path, needle: &[u8]) -> Result<bool, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(bytes.windows(needle.len()).any(|w| w == needle)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(feature = "agent-credential-store-self-test")]
pub fn agent_credential_store_self_test_json(
    files_dir: &str,
    sentinel: &str,
) -> Result<String, String> {
    if !mobile_agent_credential_ready() {
        return Err("agent credential key is not installed".into());
    }
    if sentinel.is_empty() {
        return Err("sentinel must not be empty".into());
    }
    let base = PathBuf::from(files_dir);
    let cfg = isyncyou_agent::CredentialStoreConfig::new(&base);
    let store = isyncyou_agent::CredentialStoreResolver::new(cfg.clone())
        .resolve()
        .map_err(|e| e.to_string())?;
    let id = isyncyou_agent::provider_api_key_secret_id("anthropic", Some("self-test"))
        .map_err(|e| e.to_string())?;
    store
        .put(
            isyncyou_agent::SecretClass::ProviderApiKey,
            &id,
            &isyncyou_agent::Secret::new(sentinel.as_bytes()),
        )
        .map_err(|e| e.to_string())?;
    let round_trip = store
        .get(isyncyou_agent::SecretClass::ProviderApiKey, &id)
        .map_err(|e| e.to_string())?
        .map(|secret| secret.expose() == sentinel.as_bytes())
        .unwrap_or(false);
    let (store_plaintext_found, store_file_count) =
        tree_contains_bytes(cfg.store_dir(), sentinel.as_bytes())?;
    let wrapped_key_file = base.join("agent_credential.key");
    let wrapped_plaintext_found = file_contains_bytes(&wrapped_key_file, sentinel.as_bytes())?;
    let _ = store.delete(isyncyou_agent::SecretClass::ProviderApiKey, &id);

    serde_json::to_string(&serde_json::json!({
        "self_test": "agent_credential_store",
        "scope": "jni_only_feature_gated",
        "status": if round_trip && !store_plaintext_found && !wrapped_plaintext_found { "ok" } else { "failed" },
        "key_source": "android_installed",
        "round_trip": round_trip,
        "plaintext_sentinel_in_credential_store": store_plaintext_found,
        "plaintext_sentinel_in_wrapped_key_file": wrapped_plaintext_found,
        "credential_store_file_count": store_file_count,
        "credential_store_dir": cfg.store_dir().file_name().and_then(|s| s.to_str()).unwrap_or("agent-credentials"),
        "wrapped_key_file": wrapped_key_file.file_name().and_then(|s| s.to_str()).unwrap_or("agent_credential.key")
    }))
    .map_err(|e| e.to_string())
}

fn log_mobile_config_reload_failure(reason: &str, detail: &str, warned: &AtomicBool) {
    if warned.swap(true, Ordering::Relaxed) {
        return;
    }
    isyncyou_core::obs::event(
        "mobile-config-reload",
        "failed",
        &format!(
            "reason={reason} error={}",
            isyncyou_core::obs::redact(detail)
        ),
    );
}

fn load_mobile_loop_config(
    config_path: &Path,
    base: &Path,
    last_good: &Config,
    warned: &AtomicBool,
) -> Config {
    let mut next = match Config::load(config_path) {
        Ok(cfg) => cfg,
        Err(_) => {
            log_mobile_config_reload_failure("parse", "load_or_parse_failed", warned);
            return last_good.clone();
        }
    };
    match prepare_mobile_config_for_files_dir(&mut next, base) {
        Ok(()) => {
            warned.store(false, Ordering::Relaxed);
            next
        }
        Err(e) => {
            let count = e.split("; ").filter(|s| !s.is_empty()).count().max(1);
            log_mobile_config_reload_failure(
                "validate",
                &format!("validation_errors={count}"),
                warned,
            );
            last_good.clone()
        }
    }
}

fn has_onedrive_explicit_scopes(cfg: &Config, account: &str) -> bool {
    cfg.onedrive_modes
        .get(account)
        .map(|m| {
            m.folder_modes
                .values()
                .any(|mode| matches!(mode, OneDriveMode::Sync | OneDriveMode::Offline))
        })
        .unwrap_or(false)
}

/// UI data-refresh signal for the mobile event bus. This deliberately ignores
/// error/status-only counters; those belong in diagnostics/transfer status, not a full view reload.
fn sync_report_changed(r: &isyncyou_engine::SyncReport) -> bool {
    r.resynced
        || r.upserted
            + r.deleted
            + r.downloaded
            + r.dirs_created
            + r.local_trashed
            + r.uploaded_creates
            + r.modified_uploaded
            + r.modified_conflicts
            + r.cloud_deleted
            > 0
}

/// Start the embedded engine if not already running. Idempotent. Host-testable (no JNI):
/// the JNI entry is a thin wrapper. No loopback port is bound in the default build (#0A);
/// the UI reaches the engine only in-process (bridge + asset path).
pub fn start_engine(files_dir: &str) -> Result<(), String> {
    let mut guard = cell().lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_some() {
        return Ok(()); // already running — idempotent
    }
    let (session_token, router, mobile_jobs) = start_inner(files_dir)?;
    *guard = Some(EngineState {
        session_token,
        router,
        mobile_jobs,
    });
    Ok(())
}

/// Handle one JSON-framed request from the Android in-process bridge (#0A) against the
/// running engine's router — **no loopback TCP port involved**. Returns a JSON response
/// envelope, or an error envelope when the engine hasn't started. Host-testable.
pub fn bridge_request(request_json: &str) -> String {
    let router = current_router();
    match router {
        Some(router) => isyncyou_webui::handle_bridge_request(&router, request_json),
        None => r#"{"t":"res","id":null,"status":503,"body":"{\"error\":\"engine not started\"}"}"#
            .to_string(),
    }
}

fn current_router() -> Option<Arc<isyncyou_webui::Router>> {
    let guard = cell().lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().map(|s| Arc::clone(&s.router))
}

/// Answer one browser-initiated GET subresource (#0A) — the static shell and any
/// `img`/`iframe`/viewer the WebView loads itself — for Kotlin's `shouldInterceptRequest`.
/// **Binary-safe** (unlike the JSON bridge envelope, which is text-only) and header-faithful
/// so a viewer's per-response `Content-Security-Policy` survives. Frame:
/// `[status:u16 BE][ct_len:u16 BE][content_type][hdr_len:u16 BE][headers "K: V\r\n"…][body]`.
/// Session-gated like every native data path. Empty vec when the engine has not started.
pub fn asset_request(path: &str, cookie: Option<String>) -> Vec<u8> {
    let router = current_router();
    let Some(router) = router else {
        return Vec::new();
    };
    let resp = isyncyou_webui::dispatch_message(
        &router,
        isyncyou_webui::BridgeDispatchRequest {
            method: "GET",
            target: path,
            cap_token: None,
            session_token: None,
            cookie,
            content_type: None,
            body: Vec::new(),
        },
    );
    frame_response(resp)
}

/// Answer one app-origin GET using the Activity-held native session token, without a
/// WebView-readable cookie or `_st` query parameter. This is the MainActivity asset path
/// used after #721; trusted native callers pass the session they obtained from the engine.
pub fn asset_request_with_session(path: &str, session_token: Option<&str>) -> Vec<u8> {
    asset_request_with_session_for_router(current_router(), path, session_token)
}

fn asset_request_with_session_for_router(
    router: Option<Arc<isyncyou_webui::Router>>,
    path: &str,
    session_token: Option<&str>,
) -> Vec<u8> {
    let Some(session_token) = session_token.filter(|token| !token.is_empty()) else {
        return framed_error(401, "missing session token");
    };
    let Some(router) = router else {
        return framed_error(503, "engine not started");
    };
    if !router.session_authorized(Some(session_token)) {
        return framed_error(401, "missing or invalid session token");
    }
    let req = isyncyou_webui::ApiRequest::new("GET", path)
        .with_session_token(Some(session_token.to_string()));
    frame_response(router.route(&req))
}

fn framed_error(status: u16, message: &str) -> Vec<u8> {
    let body = format!(r#"{{"error":"{message}"}}"#).into_bytes();
    frame_response(isyncyou_webui::ApiResponse {
        status,
        content_type: "application/json".into(),
        body,
        headers: Vec::new(),
    })
}

fn frame_response(resp: isyncyou_webui::ApiResponse) -> Vec<u8> {
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

/// The per-process session token used by trusted Kotlin/native request paths. It is
/// never exposed to WebView JavaScript. `None` until the engine starts.
pub fn session_token() -> Option<String> {
    cell()
        .lock()
        .ok()?
        .as_ref()
        .map(|s| s.session_token.clone())
}

/// Register a Kotlin-captured connectivity snapshot for one mobile preflight. Kotlin validates
/// the foreground guard before this JNI-owned call; JavaScript receives only the opaque result.
struct NetworkSnapshotRegistration<'a> {
    guard_id: &'a str,
    reason: &'a str,
    active_network: bool,
    internet_capability: bool,
    validated_capability: bool,
    metered: bool,
    restrict_background: &'a str,
    notifications_visible: bool,
    test_hook: Option<&'a str>,
}

fn register_network_snapshot(input: NetworkSnapshotRegistration<'_>) -> Result<String, String> {
    let restrict_background = match input.restrict_background {
        "disabled" => isyncyou_agent::RestrictBackgroundStatus::Disabled,
        "whitelisted" => isyncyou_agent::RestrictBackgroundStatus::Whitelisted,
        "enabled" => isyncyou_agent::RestrictBackgroundStatus::Enabled,
        _ => isyncyou_agent::RestrictBackgroundStatus::Unknown,
    };
    let session_token = session_token().ok_or_else(|| "engine not started".to_string())?;
    isyncyou_app_host::register_mobile_connectivity_snapshot(
        &session_token,
        input.guard_id,
        input.reason,
        isyncyou_agent::AndroidNetworkSnapshot {
            active_network: input.active_network,
            internet_capability: input.internet_capability,
            validated_capability: input.validated_capability,
            metered: input.metered,
            restrict_background,
            notifications_visible: input.notifications_visible,
            guard_ready: true,
        },
        input.test_hook,
    )
}

pub fn invalidate_network_guard(guard_id: &str) {
    isyncyou_app_host::invalidate_mobile_connectivity_guard(guard_id);
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

/// Return the fixed Rust-owned descriptor for native prompt rendering. The WebView
/// can carry the opaque handle but cannot ask this function through its bridge.
pub fn describe_action(
    pending_id: &str,
) -> Result<isyncyou_core::pending::PendingActionDescriptor, isyncyou_core::pending::DescribeError>
{
    let router = {
        let guard = cell().lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map(|s| Arc::clone(&s.router))
    };
    match router {
        Some(r) => r.describe_biometric(pending_id),
        None => Err(isyncyou_core::pending::DescribeError::NotFound),
    }
}

fn describe_action_json(pending_id: &str) -> String {
    match describe_action(pending_id) {
        Ok(descriptor) => serde_json::json!({
            "status": "ok",
            "op": descriptor.op.as_str(),
            "service": descriptor.service.as_str(),
        })
        .to_string(),
        Err(isyncyou_core::pending::DescribeError::Expired) => {
            r#"{"status":"expired"}"#.to_string()
        }
        Err(isyncyou_core::pending::DescribeError::NotFound) => {
            r#"{"status":"not_found"}"#.to_string()
        }
    }
}

fn start_inner(
    files_dir: &str,
) -> Result<
    (
        String,
        Arc<isyncyou_webui::Router>,
        Arc<isyncyou_app_host::MobileJobRuntime>,
    ),
    String,
> {
    if !mobile_encryption_ready() {
        return Err("encrypted storage setup failed; local data was not opened".into());
    }
    if !mobile_agent_credential_ready() {
        return Err("agent credential storage setup failed; local data was not opened".into());
    }
    isyncyou_core::envelope::require_body_envelope_for_process();
    isyncyou_store::require_store_key_for_process();
    let base = PathBuf::from(files_dir);
    cleanup_legacy_plaintext_mobile_state(&base)?;
    let (archive_root, sync_root, cache_root) = mobile_roots(&base);
    // OneDrive online/sync-mode lazy previews live here, apart from the offline working
    // copy in sync_root (#onedrive-mobile 0C); the writeback scanner ignores it.
    std::fs::create_dir_all(&archive_root).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&sync_root).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&cache_root).map_err(|e| e.to_string())?;
    let config_path = base.join("isyncyou.toml");

    // Load the persisted config so user settings — notably the per-folder `onedrive_modes` the
    // Mode-3 offline pass (#655) acts on — survive restarts; fall back to a fresh default on
    // first run or a parse error. The account's storage roots (and the companion poll interval)
    // are ALWAYS re-derived from `files_dir` below, so a persisted config can never point the
    // engine at a stale device path.
    let mut cfg = Config::load(&config_path).unwrap_or_default();
    prepare_mobile_config_for_files_dir(&mut cfg, &base)?;
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
    let mobile_jobs = Arc::new(isyncyou_app_host::MobileJobRuntime::new(
        cfg.clone(),
        gate.clone(),
        events.clone(),
    ));
    #[cfg(feature = "mobile-job-device-test-hooks")]
    mobile_jobs.set_device_test_hook_root(base.clone());
    let config_path_for_router = config_path.clone();
    let config_path_for_loop = config_path.clone();

    let router = Arc::new(
        isyncyou_app_host::with_mobile_full_node_jobs(
            isyncyou_app_host::build_live_router(
                cfg.clone(),
                Some(gate.clone()),
                events.clone(),
                config_path_for_router,
                live_interval.clone(),
                transfer_progress.clone(),
                isyncyou_app_host::AgentOperationPolicy::MobileFullNode {
                    mobile_jobs: mobile_jobs.clone(),
                },
            ),
            mobile_jobs.clone(),
        )
        .with_session_token(session_token.clone())
        // #onedrive-mobile 0.6: only the standalone Android app arms the biometric gate.
        // Destructive routes then require a per-action token that is valid only after a
        // native BiometricPrompt (confirmed over `nativeConfirmAction`, below).
        .with_biometric_gate(),
    );

    // Cache-refresh thread (#89 P2): once the account is signed in, periodically pull
    // mail/calendar/contacts/todo/onenote from Graph into the local cache store
    // (read-only — never writes back to the cloud) and wake SSE subscribers so the
    // UI refreshes. Skips silently until a token is cached.
    std::thread::spawn(move || {
        refresh_loop(
            cfg.clone(),
            base,
            config_path_for_loop,
            gate,
            events,
            live_interval,
            transfer_progress.clone(),
        )
    });

    Ok((session_token, router, mobile_jobs))
}

/// One mobile scoped OneDrive pass under the store-access gate (#655/#718): Sync scopes ingest
/// metadata; Offline scopes additionally materialize and mirror local edits back over the ledger.
/// Returns whether UI-visible data changed. Skips quietly with no explicit Sync/Offline scopes
/// configured or no cached sync/write token (not signed in). `progress` is the shared tracker the
/// router surfaces. Token policy is unchanged: the scoped pass still uses `resolve_cached_sync_token`.
fn run_onedrive_scoped_pass(
    cfg: &Config,
    gate: &Arc<Mutex<()>>,
    progress: &isyncyou_app_host::SharedProgress,
) -> bool {
    if !has_onedrive_explicit_scopes(cfg, ACCOUNT) {
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
            let changed = sync_report_changed(&r);
            if changed || r.modified_failed > 0 || r.materialize_failed > 0 {
                android_info(&format!("mobile OneDrive scoped pass: {}", r.summary()));
            }
            changed
        }
        Err(e) => {
            android_info(&format!(
                "mobile OneDrive scoped pass failed: {}",
                isyncyou_core::obs::redact(&e)
            ));
            false
        }
    }
}

fn refresh_loop(
    mut cfg: Config,
    base: PathBuf,
    config_path: PathBuf,
    gate: Arc<Mutex<()>>,
    events: Arc<isyncyou_webui::EventBus>,
    interval: Arc<AtomicU64>,
    progress: isyncyou_app_host::SharedProgress,
) {
    let reload_warned = AtomicBool::new(false);
    loop {
        let secs = interval.load(Ordering::Relaxed).max(5);
        std::thread::sleep(Duration::from_secs(secs));
        // Isolate a refresh panic so the loop (and the app) survives.
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            cfg = load_mobile_loop_config(&config_path, &base, &cfg, &reload_warned);
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
            // #655/#718: after the read-only cache refresh, run the mobile scoped OneDrive pass
            // for any explicit Sync/Offline folders, then wake SSE if either changed data.
            let onedrive_changed = run_onedrive_scoped_pass(&cfg, &gate, &progress);
            if refreshed || onedrive_changed {
                events.notify(); // wake SSE subscribers so the UI refetches
            }
        }));
    }
}

// ----------------------------------------------------------------- JNI surface
// `com.silentspike.isyncyou.NativeEngine.nativeStart(filesDir)` -> bound port (or -1)
// `com.silentspike.isyncyou.NativeEngine.nativeSessionToken()` -> token string

#[derive(Debug)]
struct NativeMobileJobRunRequest {
    v: u32,
    job_id: String,
    kind: String,
    device: NativeMobileDeviceSnapshot,
}

#[derive(Debug)]
struct NativeMobileDeviceSnapshot {
    network_validated: bool,
    metered: bool,
    charging: bool,
    free_bytes: u64,
}

fn valid_mobile_job_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn mobile_job_kind_from_wire(kind: &str) -> Option<isyncyou_app_host::MobileJobKind> {
    match kind {
        "backup" => Some(isyncyou_app_host::MobileJobKind::Backup),
        "restore-cloud" => Some(isyncyou_app_host::MobileJobKind::RestoreCloud),
        _ => None,
    }
}

fn mobile_jobs_handle() -> Option<Arc<isyncyou_app_host::MobileJobRuntime>> {
    cell()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|state| state.mobile_jobs.clone())
}

fn native_mobile_job_plan_json() -> String {
    let Some(jobs) = mobile_jobs_handle() else {
        return r#"{"v":1,"status":"not_started"}"#.to_string();
    };
    match jobs.mobile_worker_plan(ACCOUNT) {
        Ok((entries, truncated)) => {
            let (wifi_only, charging_only, min_free_bytes) = jobs.mobile_worker_constraints();
            let jobs = entries
                .into_iter()
                .map(|(job_id, kind)| serde_json::json!({"job_id": job_id, "kind": kind.as_str()}))
                .collect::<Vec<_>>();
            serde_json::json!({
                "v": 1,
                "status": "ok",
                "jobs": jobs,
                "truncated": truncated,
                "constraints": {
                    "wifi_only": wifi_only,
                    "charging_only": charging_only,
                    "min_free_bytes": min_free_bytes,
                },
            })
            .to_string()
        }
        Err(_) => r#"{"v":1,"status":"error","code":"internal"}"#.to_string(),
    }
}

fn native_mobile_job_run_json(request_json: &str) -> String {
    let value = match serde_json::from_str::<serde_json::Value>(request_json) {
        Ok(value) => value,
        Err(_) => return r#"{"v":1,"status":"failed","code":"invalid_request"}"#.to_string(),
    };
    let device = match value.get("device").and_then(|v| v.as_object()) {
        Some(device) => device,
        None => return r#"{"v":1,"status":"failed","code":"invalid_request"}"#.to_string(),
    };
    let (
        Some(v),
        Some(job_id),
        Some(kind),
        Some(network_validated),
        Some(metered),
        Some(charging),
        Some(free_bytes),
    ) = (
        value.get("v").and_then(|v| v.as_u64()),
        value.get("job_id").and_then(|v| v.as_str()),
        value.get("kind").and_then(|v| v.as_str()),
        device.get("network_validated").and_then(|v| v.as_bool()),
        device.get("metered").and_then(|v| v.as_bool()),
        device.get("charging").and_then(|v| v.as_bool()),
        device.get("free_bytes").and_then(|v| v.as_u64()),
    )
    else {
        return r#"{"v":1,"status":"failed","code":"invalid_request"}"#.to_string();
    };
    let request = NativeMobileJobRunRequest {
        v: v as u32,
        job_id: job_id.to_string(),
        kind: kind.to_string(),
        device: NativeMobileDeviceSnapshot {
            network_validated,
            metered,
            charging,
            free_bytes,
        },
    };
    if request.v != 1 {
        return r#"{"v":1,"status":"failed","code":"invalid_request"}"#.to_string();
    }
    let Some(kind) = mobile_job_kind_from_wire(&request.kind) else {
        return r#"{"v":1,"status":"failed","code":"invalid_kind"}"#.to_string();
    };
    if !valid_mobile_job_id(&request.job_id) {
        return r#"{"v":1,"status":"failed","code":"invalid_job_id"}"#.to_string();
    }
    let Some(jobs) = mobile_jobs_handle() else {
        return r#"{"v":1,"status":"failed","code":"not_started"}"#.to_string();
    };
    let device = isyncyou_app_host::MobileWorkerDeviceSnapshot {
        network_validated: request.device.network_validated,
        metered: request.device.metered,
        charging: request.device.charging,
        free_bytes: request.device.free_bytes,
    };
    let outcome = jobs.run_mobile_job_for_worker(&request.job_id, kind, device);
    match outcome {
        Ok(isyncyou_app_host::MobileJobRunOutcome::Succeeded { .. }) => {
            r#"{"v":1,"status":"succeeded"}"#.to_string()
        }
        Ok(isyncyou_app_host::MobileJobRunOutcome::Retrying {
            code,
            retry_after_secs,
            ..
        }) => serde_json::json!({
            "v": 1,
            "status": "retry",
            "code": code.as_str(),
            "retry_after_secs": retry_after_secs,
        })
        .to_string(),
        Ok(isyncyou_app_host::MobileJobRunOutcome::Failed { code, .. }) => {
            serde_json::json!({"v": 1, "status": "failed", "code": code.as_str()}).to_string()
        }
        Ok(isyncyou_app_host::MobileJobRunOutcome::Deferred { code, .. }) => {
            serde_json::json!({"v": 1, "status": "retry", "code": code.as_str()}).to_string()
        }
        Ok(isyncyou_app_host::MobileJobRunOutcome::Noop { code, .. }) => {
            serde_json::json!({"v": 1, "status": "succeeded", "code": code.as_str()}).to_string()
        }
        Err(error) => {
            let code = if error == "job_kind_mismatch" {
                "kind_mismatch"
            } else {
                "internal"
            };
            serde_json::json!({"v": 1, "status": "failed", "code": code}).to_string()
        }
    }
}

fn jni_get_string<'local>(
    env: &mut jni::EnvUnowned<'local>,
    value: &jni::objects::JString<'local>,
) -> Option<String> {
    match env
        .with_env(|env| -> jni::errors::Result<String> { value.try_to_string(env) })
        .into_outcome()
    {
        jni::Outcome::Ok(value) => Some(value),
        _ => None,
    }
}

fn jni_new_string<'local>(env: &mut jni::EnvUnowned<'local>, value: String) -> jni::sys::jstring {
    match env
        .with_env(move |env| -> jni::errors::Result<jni::sys::jstring> {
            Ok(env.new_string(value)?.into_raw())
        })
        .into_outcome()
    {
        jni::Outcome::Ok(value) => value,
        _ => std::ptr::null_mut(),
    }
}

fn jni_byte_array_from_slice<'local>(
    env: &mut jni::EnvUnowned<'local>,
    bytes: &[u8],
) -> jni::sys::jbyteArray {
    let bytes = bytes.to_vec();
    match env
        .with_env(move |env| -> jni::errors::Result<jni::sys::jbyteArray> {
            Ok(env.byte_array_from_slice(&bytes)?.into_raw())
        })
        .into_outcome()
    {
        jni::Outcome::Ok(value) => value,
        _ => std::ptr::null_mut(),
    }
}

fn jni_convert_byte_array<'local>(
    env: &mut jni::EnvUnowned<'local>,
    value: &jni::objects::JByteArray<'local>,
) -> Option<Vec<u8>> {
    match env
        .with_env(|env| -> jni::errors::Result<Vec<u8>> { env.convert_byte_array(value) })
        .into_outcome()
    {
        jni::Outcome::Ok(value) => Some(value),
        _ => None,
    }
}

/// JNI: start the in-process engine, returning a positive readiness value or -1.
/// SECURITY: never logs the session token or any secret.
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeStart<'local>(
    mut env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
    files_dir: jni::objects::JString<'local>,
) -> jni::sys::jint {
    let dir = match jni_get_string(&mut env, &files_dir) {
        Some(s) => s,
        None => return -1,
    };
    // The default build binds no loopback port, so there is no port to return: 1 = started,
    // -1 = failed. Kotlin only checks `> 0` to know the engine is up (#0A).
    match start_engine(&dir) {
        Ok(()) => 1,
        Err(_) => -1,
    }
}

/// JNI: return the bounded list of recoverable mobile jobs for WorkManager. The
/// response contains only opaque job IDs, kinds, and truncation metadata.
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeMobileJobPlan<'local>(
    mut env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
) -> jni::sys::jstring {
    let response = std::panic::catch_unwind(native_mobile_job_plan_json)
        .unwrap_or_else(|_| r#"{"v":1,"status":"failed","code":"internal"}"#.to_string());
    jni_new_string(&mut env, response)
}

/// JNI: validate and run one WorkManager-selected mobile job using a strict
/// versioned request. No account, target, cloud result, or raw error is returned.
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeRunMobileJob<'local>(
    mut env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
    request_json: jni::objects::JString<'local>,
) -> jni::sys::jstring {
    let request = match jni_get_string(&mut env, &request_json) {
        Some(request) if request.len() <= 16 * 1024 => request,
        _ => {
            return jni_new_string(
                &mut env,
                r#"{"v":1,"status":"failed","code":"invalid_request"}"#.to_string(),
            )
        }
    };
    let response = std::panic::catch_unwind(|| native_mobile_job_run_json(&request))
        .unwrap_or_else(|_| r#"{"v":1,"status":"failed","code":"internal"}"#.to_string());
    jni_new_string(&mut env, response)
}

/// JNI: the per-process session token Kotlin hands to the WebView (header + cookie).
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeSessionToken<'local>(
    mut env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
) -> jni::sys::jstring {
    let tok = session_token().unwrap_or_default();
    jni_new_string(&mut env, tok)
}

/// JNI: push the current device transfer conditions from the Android platform layer — the
/// active network is metered, the device is charging, and the free bytes on the sync volume —
/// read by the offline pass's policy gate (#655). May be called any time; the latest wins.
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeDeviceState<'local>(
    _env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
    metered: jni::sys::jboolean,
    charging: jni::sys::jboolean,
    free_bytes: jni::sys::jlong,
) {
    set_device_state(metered, charging, free_bytes.max(0) as u64);
}

/// JNI: register a one-shot, session-bound Android connectivity snapshot. No raw snapshot
/// fields are returned to WebView JavaScript; callers receive only an opaque handle.
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeRegisterNetworkSnapshot<
    'local,
>(
    mut env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
    guard_id: jni::objects::JString<'local>,
    reason: jni::objects::JString<'local>,
    active_network: jni::sys::jboolean,
    internet_capability: jni::sys::jboolean,
    validated_capability: jni::sys::jboolean,
    metered: jni::sys::jboolean,
    restrict_background: jni::objects::JString<'local>,
    notifications_visible: jni::sys::jboolean,
    test_hook: jni::objects::JString<'local>,
) -> jni::sys::jstring {
    let result = (|| {
        let guard_id = jni_get_string(&mut env, &guard_id)?;
        let reason = jni_get_string(&mut env, &reason)?;
        let restrict_background = jni_get_string(&mut env, &restrict_background)?;
        let test_hook = jni_get_string(&mut env, &test_hook)?;
        register_network_snapshot(NetworkSnapshotRegistration {
            guard_id: &guard_id,
            reason: &reason,
            active_network,
            internet_capability,
            validated_capability,
            metered,
            restrict_background: &restrict_background,
            notifications_visible,
            test_hook: (!test_hook.is_empty()).then_some(test_hook.as_str()),
        })
        .ok()
    })()
    .unwrap_or_default();
    jni_new_string(&mut env, result)
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeInvalidateNetworkGuard<
    'local,
>(
    mut env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
    guard_id: jni::objects::JString<'local>,
) {
    if let Some(guard_id) = jni_get_string(&mut env, &guard_id) {
        invalidate_network_guard(&guard_id);
    }
}

/// JNI: answer one in-process bridge request (#0A). Kotlin passes the JSON request
/// envelope from the `WebMessageListener` and posts the returned JSON envelope back on the
/// message port — no loopback TCP port is used. SECURITY: never logs tokens or bodies.
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeBridgeRequest<'local>(
    mut env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
    request_json: jni::objects::JString<'local>,
) -> jni::sys::jstring {
    let req = match jni_get_string(&mut env, &request_json) {
        Some(s) => s,
        None => return std::ptr::null_mut(),
    };
    // Panic-isolate: a request-handling panic must never unwind across the FFI boundary.
    let resp =
        std::panic::catch_unwind(AssertUnwindSafe(|| bridge_request(&req))).unwrap_or_else(|_| {
            r#"{"t":"res","id":null,"status":500,"body":"{\"error\":\"internal error\"}"}"#
                .to_string()
        });
    jni_new_string(&mut env, resp)
}

/// JNI: answer one browser-initiated GET subresource (#0A) for `shouldInterceptRequest`,
/// returning the framed bytes (see [`asset_request`]). Binary-safe. Never logs the cookie.
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeAssetRequest<'local>(
    mut env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
    path: jni::objects::JString<'local>,
    cookie: jni::objects::JString<'local>,
) -> jni::sys::jbyteArray {
    let path = match jni_get_string(&mut env, &path) {
        Some(s) => s,
        None => return std::ptr::null_mut(),
    };
    let cookie = jni_get_string(&mut env, &cookie).unwrap_or_default();
    let cookie = if cookie.is_empty() {
        None
    } else {
        Some(cookie)
    };
    let bytes = std::panic::catch_unwind(AssertUnwindSafe(|| asset_request(&path, cookie)))
        .unwrap_or_default();
    jni_byte_array_from_slice(&mut env, &bytes)
}

/// JNI: answer one browser-initiated GET subresource using the trusted Activity-held
/// session token, not a WebView cookie. Binary-safe. Never logs the token.
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeAssetRequestWithSession<
    'local,
>(
    mut env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
    path: jni::objects::JString<'local>,
    session_token: jni::objects::JString<'local>,
) -> jni::sys::jbyteArray {
    let path = match jni_get_string(&mut env, &path) {
        Some(s) => s,
        None => return std::ptr::null_mut(),
    };
    let session_token = jni_get_string(&mut env, &session_token).unwrap_or_default();
    let session_token = if session_token.is_empty() {
        None
    } else {
        Some(session_token.as_str())
    };
    let bytes = std::panic::catch_unwind(AssertUnwindSafe(|| {
        asset_request_with_session(&path, session_token)
    }))
    .unwrap_or_default();
    jni_byte_array_from_slice(&mut env, &bytes)
}

/// JNI: open a bridge push stream (#0A), returning a stream id (>0) or 0. The session
/// token is passed explicitly (the WebView can't set headers on a native stream open).
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeStreamOpen<'local>(
    mut env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
    path: jni::objects::JString<'local>,
    session_token: jni::objects::JString<'local>,
) -> jni::sys::jlong {
    let path = match jni_get_string(&mut env, &path) {
        Some(s) => s,
        None => return 0,
    };
    let tok = jni_get_string(&mut env, &session_token).unwrap_or_default();
    let tok = if tok.is_empty() { None } else { Some(tok) };
    std::panic::catch_unwind(AssertUnwindSafe(|| stream_open(&path, tok.as_deref()))).unwrap_or(0)
}

/// JNI: block for the next event on stream `id` (a JSON `{event,data}` object), or "" when
/// the stream ended/closed. Kotlin's per-stream thread loops on this. Never logs.
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeStreamNext<'local>(
    mut env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
    id: jni::sys::jlong,
) -> jni::sys::jstring {
    let out = std::panic::catch_unwind(AssertUnwindSafe(|| stream_next(id))).unwrap_or_default();
    jni_new_string(&mut env, out)
}

/// JNI: close a bridge push stream.
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeStreamClose<'local>(
    _env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
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
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeSetBodyKey<'local>(
    mut env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
    key_id: jni::sys::jint,
    key: jni::objects::JByteArray<'local>,
) -> jni::sys::jint {
    let bytes = match jni_convert_byte_array(&mut env, &key) {
        Some(b) => b,
        None => return 0,
    };
    if install_mobile_body_key(key_id, &bytes) {
        1
    } else {
        0
    }
}

/// JNI: install the agent credential at-rest key (#620) — the 32-byte data key the
/// Android Keystore unwrapped for provider credentials. MUST be called before
/// [`nativeStart`] so app-host credential consumers use the Android-installed key instead
/// of env/local fallback. SECURITY: the key bytes are never logged.
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeSetAgentCredentialKey<
    'local,
>(
    mut env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
    key: jni::objects::JByteArray<'local>,
) -> jni::sys::jint {
    let bytes = match jni_convert_byte_array(&mut env, &key) {
        Some(b) => b,
        None => return 0,
    };
    if install_mobile_agent_credential_key(&bytes) {
        1
    } else {
        0
    }
}

/// JNI test/evidence hook for #619. Built only with
/// `ISY_CARGO_FEATURES=agent-session-kdf-bench`; there is deliberately no WebView,
/// bridge, HTTP, or normal app UI path to this method.
#[cfg(feature = "agent-session-kdf-bench")]
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeAgentSessionKdfBenchmark<
    'local,
>(
    mut env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
    iterations: jni::sys::jint,
) -> jni::sys::jstring {
    let iterations = if iterations <= 0 {
        3
    } else {
        iterations as usize
    };
    let out = std::panic::catch_unwind(AssertUnwindSafe(|| {
        agent_session_kdf_benchmark_json(iterations)
    }))
    .unwrap_or_else(|_| Err("panic".into()))
    .unwrap_or_else(|error| {
        serde_json::json!({
            "benchmark": "agent_session_argon2id_hkdf",
            "error": isyncyou_core::obs::redact(&error)
        })
        .to_string()
    });
    jni_new_string(&mut env, out)
}

/// JNI test/evidence hook for #620. Built only with
/// `ISY_CARGO_FEATURES=agent-credential-store-self-test`; there is deliberately no
/// WebView, bridge, HTTP, or normal app UI path to this method.
#[cfg(feature = "agent-credential-store-self-test")]
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeAgentCredentialStoreSelfTest<
    'local,
>(
    mut env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
    files_dir: jni::objects::JString<'local>,
    sentinel: jni::objects::JString<'local>,
) -> jni::sys::jstring {
    let files_dir = match jni_get_string(&mut env, &files_dir) {
        Some(s) => s,
        None => return std::ptr::null_mut(),
    };
    let sentinel = match jni_get_string(&mut env, &sentinel) {
        Some(s) => s,
        None => return std::ptr::null_mut(),
    };
    let out = std::panic::catch_unwind(AssertUnwindSafe(|| {
        agent_credential_store_self_test_json(&files_dir, &sentinel)
    }))
    .unwrap_or_else(|_| Err("panic".into()))
    .unwrap_or_else(|error| {
        serde_json::json!({
            "self_test": "agent_credential_store",
            "scope": "jni_only_feature_gated",
            "status": "error",
            "error": isyncyou_core::obs::redact(&error)
        })
        .to_string()
    });
    jni_new_string(&mut env, out)
}

/// JNI: record a successful native `BiometricPrompt` for a pending destructive action
/// (#onedrive-mobile 0.6). Kotlin calls this ONLY from the biometric success callback, so
/// the confirmation cannot originate in the WebView (which holds every cap-token). Returns
/// 1 when the pending id was found and armed, 0 otherwise (unknown/expired/engine down).
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeConfirmAction<'local>(
    mut env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
    pending_id: jni::objects::JString<'local>,
) -> jni::sys::jboolean {
    let id = match jni_get_string(&mut env, &pending_id) {
        Some(s) => s,
        None => return false,
    };
    std::panic::catch_unwind(AssertUnwindSafe(|| confirm_action(&id))).unwrap_or(false)
}

/// JNI: return only the bounded Rust-owned operation/service descriptor for a pending
/// action. Kotlin maps these enum names to fixed resources. The handle, action hash,
/// account, item, and destructive payload are never returned or logged.
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeDescribePendingAction<
    'local,
>(
    mut env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
    pending_id: jni::objects::JString<'local>,
) -> jni::sys::jstring {
    let id = jni_get_string(&mut env, &pending_id).unwrap_or_default();
    let out = std::panic::catch_unwind(AssertUnwindSafe(|| describe_action_json(&id)))
        .unwrap_or_else(|_| r#"{"status":"internal_error"}"#.to_string());
    jni_new_string(&mut env, out)
}

/// #640 evidence marker. It is intentionally JNI-only and has no WebView, bridge, HTTP,
/// or router caller. The fixed marker lets the device harness distinguish a hook APK from a
/// rebuilt default APK without relying on Cargo feature names surviving link-time stripping.
#[cfg(feature = "agent-network-device-test-hooks")]
#[used]
#[no_mangle]
pub static ISY_AGENT_NETWORK_DEVICE_HOOK_MARKER: &[u8] = b"ISY_AGENT_NETWORK_DEVICE_HOOK_V1";

/// Consume one app-private #640 diagnostic hook. This is deliberately JNI-only: WebView,
/// HTTP, bridge payloads, and capability tokens cannot select a diagnostic branch.
#[cfg(feature = "agent-network-device-test-hooks")]
fn take_private_device_test_hook(
    files_dir: &str,
    hook_file: &str,
    allowed_values: &[&str],
) -> String {
    use std::fs::OpenOptions;
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::Path;

    const MAX_HOOK_BYTES: u64 = 128;
    let root = Path::new(files_dir);
    if !root.is_absolute()
        || root.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return String::new();
    }
    let Ok(root_meta) = std::fs::symlink_metadata(root) else {
        return String::new();
    };
    if root_meta.file_type().is_symlink() || !root_meta.is_dir() {
        return String::new();
    }
    let path = root.join(hook_file);
    let Ok(pre_open_meta) = std::fs::symlink_metadata(&path) else {
        return String::new();
    };
    if pre_open_meta.file_type().is_symlink()
        || !pre_open_meta.is_file()
        || pre_open_meta.len() > MAX_HOOK_BYTES
        || pre_open_meta.mode() & 0o077 != 0
        || pre_open_meta.uid() != unsafe { libc::geteuid() }
    {
        let _ = std::fs::remove_file(&path);
        return String::new();
    }

    let mut bytes = Vec::with_capacity(pre_open_meta.len() as usize);
    let read_result = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .and_then(|mut file| {
            let meta = file.metadata()?;
            if !meta.is_file()
                || meta.len() > MAX_HOOK_BYTES
                || meta.mode() & 0o077 != 0
                || meta.uid() != unsafe { libc::geteuid() }
            {
                return Err(std::io::Error::other("invalid hook file"));
            }
            file.read_to_end(&mut bytes)
        });
    // The hook is always one-shot, including malformed or failed reads.
    let _ = std::fs::remove_file(&path);
    if read_result.is_err() || bytes.len() > MAX_HOOK_BYTES as usize {
        return String::new();
    }
    let Ok(value) = std::str::from_utf8(&bytes) else {
        return String::new();
    };
    let value = value.trim();
    if allowed_values.contains(&value) {
        value.to_string()
    } else {
        String::new()
    }
}

#[cfg(feature = "agent-network-device-test-hooks")]
fn take_network_device_test_hook(files_dir: &str) -> String {
    take_private_device_test_hook(
        files_dir,
        "network-diagnostic-test-hook",
        &[
            "no_validated_network",
            "connect_timeout",
            "tls_failed",
            "http_failed",
            "foreground_guard_unavailable",
        ],
    )
}

#[cfg(feature = "agent-network-device-test-hooks")]
fn arm_codex_refresh_device_test_hook(files_dir: &str) -> bool {
    if take_private_device_test_hook(
        files_dir,
        "credential-refresh-test-hook",
        &["codex_refresh_due"],
    ) != "codex_refresh_due"
    {
        return false;
    }
    isyncyou_app_host::arm_codex_refresh_for_device_test();
    true
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeNetworkDeviceHooksEnabled<
    'local,
>(
    _env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
) -> jni::sys::jboolean {
    #[cfg(feature = "agent-network-device-test-hooks")]
    {
        // Keep the marker referenced in the executable path so binary scans can prove the
        // hook/default split. Returning a boolean avoids exposing any diagnostic control.
        !ISY_AGENT_NETWORK_DEVICE_HOOK_MARKER.is_empty()
    }
    #[cfg(not(feature = "agent-network-device-test-hooks"))]
    {
        false
    }
}

#[cfg(feature = "agent-network-device-test-hooks")]
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeTakeNetworkDeviceTestHook<
    'local,
>(
    mut env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
    files_dir: jni::objects::JString<'local>,
) -> jni::sys::jstring {
    let value = jni_get_string(&mut env, &files_dir)
        .map(|path| take_network_device_test_hook(&path))
        .unwrap_or_default();
    jni_new_string(&mut env, value)
}

#[cfg(feature = "agent-network-device-test-hooks")]
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeArmCodexRefreshDeviceTestHook<
    'local,
>(
    mut env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
    files_dir: jni::objects::JString<'local>,
) -> jni::sys::jboolean {
    jni_get_string(&mut env, &files_dir)
        .map(|path| arm_codex_refresh_device_test_hook(&path))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "agent-network-device-test-hooks")]
    #[test]
    fn network_device_hook_is_one_shot_and_rejects_unsafe_files() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let dir = tempfile::tempdir().unwrap();
        let hook = dir.path().join("network-diagnostic-test-hook");
        std::fs::write(&hook, "connect_timeout\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            take_network_device_test_hook(dir.path().to_str().unwrap()),
            "connect_timeout"
        );
        assert!(!hook.exists(), "a valid hook must be consumed exactly once");
        assert!(
            take_network_device_test_hook(dir.path().to_str().unwrap()).is_empty(),
            "a consumed hook cannot be replayed"
        );

        let target = dir.path().join("outside");
        std::fs::write(&target, "tls_failed").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, &hook).unwrap();
        assert!(take_network_device_test_hook(dir.path().to_str().unwrap()).is_empty());
        assert!(
            !hook.exists(),
            "a rejected symlink is removed without following it"
        );

        std::fs::write(&hook, "http_failed").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(take_network_device_test_hook(dir.path().to_str().unwrap()).is_empty());
        assert!(
            !hook.exists(),
            "a non-owner-only hook is consumed and rejected"
        );
    }

    #[cfg(feature = "agent-network-device-test-hooks")]
    #[test]
    fn codex_refresh_device_hook_value_is_closed_and_one_shot() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let hook = dir.path().join("credential-refresh-test-hook");
        std::fs::write(&hook, "codex_refresh_due\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            take_private_device_test_hook(
                dir.path().to_str().unwrap(),
                "credential-refresh-test-hook",
                &["codex_refresh_due"],
            ),
            "codex_refresh_due"
        );
        assert!(!hook.exists());
        assert!(take_private_device_test_hook(
            dir.path().to_str().unwrap(),
            "credential-refresh-test-hook",
            &["codex_refresh_due"],
        )
        .is_empty());
    }

    #[test]
    fn mobile_product_has_no_local_cli_experimental_feature() {
        let manifest = include_str!("../Cargo.toml");
        let source = include_str!("lib.rs");
        let forbidden_feature = ["agent-subscription", "-experimental"].concat();
        let loopback_symbol = ["bind_", "loopback("].concat();

        assert!(!manifest.contains(&forbidden_feature));
        assert!(!source.contains(&forbidden_feature));
        assert!(!source.contains(&loopback_symbol));
    }

    #[test]
    fn android_rejects_agent_subscription_experimental_feature() {
        let gradle = include_str!("../../../android/app/build.gradle.kts");
        let forbidden_feature = ["agent-subscription", "-experimental"].concat();
        let expected = [
            "agent-session-kdf-bench",
            "agent-credential-store-self-test",
            "mobile-job-device-test-hooks",
            "agent-network-device-test-hooks",
        ];
        let allowlist = gradle
            .split_once("val allowedCargoTestFeatures = setOf(")
            .unwrap()
            .1
            .split_once(")\nval requestedCargoTestFeatures")
            .unwrap()
            .0;
        let declared = allowlist
            .lines()
            .filter_map(|line| {
                line.trim()
                    .strip_prefix('"')
                    .and_then(|line| line.strip_suffix("\","))
            })
            .collect::<Vec<_>>();

        assert_eq!(declared, expected);
        assert!(gradle.contains("Unsupported ISY_CARGO_FEATURES value"));
        assert!(!gradle.contains(&forbidden_feature));
    }

    #[test]
    fn native_mobile_job_run_rejects_malformed_and_unbounded_requests() {
        assert!(native_mobile_job_run_json("not-json").contains("invalid_request"));
        assert!(native_mobile_job_run_json(
            r#"{"v":1,"job_id":"bad/id","kind":"backup","device":{"network_validated":true,"metered":false,"charging":true,"free_bytes":999999999}}"#
        )
        .contains("invalid_job_id"));
        assert!(native_mobile_job_run_json(
            r#"{"v":1,"job_id":"job-1","kind":"backup","device":{"network_validated":true}}"#
        )
        .contains("invalid_request"));
    }

    #[test]
    fn native_mobile_job_kind_and_id_policy_is_bounded() {
        assert!(valid_mobile_job_id("mobile-backup-123"));
        assert!(!valid_mobile_job_id("mobile/job"));
        assert!(!valid_mobile_job_id(&"x".repeat(129)));
        assert!(mobile_job_kind_from_wire("backup").is_some());
        assert!(mobile_job_kind_from_wire("restore-cloud").is_some());
        assert!(mobile_job_kind_from_wire("unknown").is_none());
    }

    fn api_json(resp: isyncyou_webui::ApiResponse) -> serde_json::Value {
        serde_json::from_slice(&resp.body).expect("json response")
    }

    fn frame_status(framed: &[u8]) -> u16 {
        assert!(framed.len() >= 2, "framed response has status bytes");
        u16::from_be_bytes([framed[0], framed[1]])
    }

    fn cap_from_app_js(router: &isyncyou_webui::Router, key: &str) -> String {
        let resp = router.route(&isyncyou_webui::ApiRequest::get("/app.js"));
        assert_eq!(resp.status, 200, "app.js served");
        let js = String::from_utf8(resp.body).unwrap();
        let needle = format!("{key}: \"");
        let start = js.find(&needle).expect("cap key in app.js") + needle.len();
        let end = js[start..].find('"').expect("cap end") + start;
        let cap = js[start..end].to_string();
        assert!(!cap.is_empty(), "{key} cap must be populated");
        assert!(
            !cap.starts_with("__"),
            "{key} cap placeholder must be replaced"
        );
        cap
    }

    fn restore_enabled_mobile_config(files_dir: &std::path::Path) {
        let mut cfg = Config::default();
        cfg.restore.cloud_restore_enabled = true;
        prepare_mobile_config_for_files_dir(&mut cfg, files_dir).unwrap();
        cfg.save(files_dir.join("isyncyou.toml")).unwrap();
    }

    fn mobile_key_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap()
    }

    #[cfg(feature = "agent-session-kdf-bench")]
    #[test]
    fn agent_session_kdf_benchmark_json_is_structured_and_redacted() {
        let out = agent_session_kdf_benchmark_json(1).unwrap();
        assert!(!out.contains("A7A7"));
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            value["benchmark"].as_str(),
            Some("agent_session_argon2id_hkdf")
        );
        assert_eq!(value["scope"].as_str(), Some("jni_only_feature_gated"));
        assert_eq!(value["iterations"].as_u64(), Some(1));
        assert!(value["median_ms"].as_f64().unwrap() > 0.0);
        assert_eq!(value["kdf"]["memory_kib"].as_u64(), Some(65_536));
        assert_eq!(value["kdf"]["iterations"].as_u64(), Some(3));
        assert_eq!(value["kdf"]["lanes"].as_u64(), Some(4));
    }

    #[cfg(feature = "agent-credential-store-self-test")]
    #[test]
    fn agent_credential_store_self_test_json_is_structured_and_redacted() {
        let _guard = mobile_key_test_guard();
        reset_mobile_agent_credential_ready_for_tests();
        assert!(install_mobile_agent_credential_key(&[8u8; 32]));
        let dir = tempfile::tempdir().unwrap();
        let sentinel = "agent-credential-self-test-sentinel";
        let out = agent_credential_store_self_test_json(dir.path().to_str().unwrap(), sentinel)
            .expect("self-test json");
        assert!(!out.contains(sentinel));
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["self_test"].as_str(), Some("agent_credential_store"));
        assert_eq!(value["scope"].as_str(), Some("jni_only_feature_gated"));
        assert_eq!(value["status"].as_str(), Some("ok"));
        assert_eq!(value["key_source"].as_str(), Some("android_installed"));
        assert_eq!(value["round_trip"].as_bool(), Some(true));
        assert_eq!(
            value["plaintext_sentinel_in_credential_store"].as_bool(),
            Some(false)
        );
        assert_eq!(
            value["plaintext_sentinel_in_wrapped_key_file"].as_bool(),
            Some(false)
        );
        assert_eq!(
            value["credential_store_dir"].as_str(),
            Some("agent-credentials")
        );
        assert_eq!(
            value["wrapped_key_file"].as_str(),
            Some("agent_credential.key")
        );
    }

    #[test]
    fn start_engine_is_idempotent_and_mints_a_session_token() {
        // Host test of the non-JNI core (#89 P4 / #0A): start succeeds, mints a session
        // token, and a second call reuses the SAME running engine (Activity recreation must
        // not start a second one). No loopback port is bound in the default build.
        let _guard = mobile_key_test_guard();
        install_test_mobile_encryption();
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
    fn network_snapshot_registration_uses_engine_session_and_closed_guard_reason() {
        let _guard = mobile_key_test_guard();
        install_test_mobile_encryption();
        let dir = tempfile::tempdir().unwrap();
        start_engine(dir.path().to_str().unwrap()).expect("engine starts");
        let guard_id = "network-snapshot-mobile-test-guard";

        let snapshot_id = register_network_snapshot(NetworkSnapshotRegistration {
            guard_id,
            reason: "agent_turn",
            active_network: true,
            internet_capability: true,
            validated_capability: true,
            metered: false,
            restrict_background: "disabled",
            notifications_visible: true,
            test_hook: None,
        })
        .expect("trusted native snapshot registration succeeds");

        assert!(!snapshot_id.is_empty());
        assert_ne!(snapshot_id, guard_id);
        invalidate_network_guard(guard_id);
    }

    #[test]
    fn mobile_start_inner_fails_closed_without_encryption_ready() {
        let _guard = mobile_key_test_guard();
        reset_mobile_encryption_ready_for_tests();
        reset_mobile_agent_credential_ready_for_tests();
        let dir = tempfile::tempdir().unwrap();

        let err = match start_inner(dir.path().to_str().unwrap()) {
            Ok(_) => panic!("start_inner must fail without encryption readiness"),
            Err(err) => err,
        };

        assert!(
            err.contains("encrypted storage setup failed"),
            "startup must expose a redacted encrypted-storage failure: {err}"
        );
        assert!(
            !dir.path()
                .join("archive")
                .join(".isyncyou-store.db")
                .exists(),
            "no plaintext store is created when encryption setup failed"
        );
    }

    #[test]
    fn mobile_start_inner_fails_closed_without_agent_credential_key() {
        let _guard = mobile_key_test_guard();
        reset_mobile_encryption_ready_for_tests();
        reset_mobile_agent_credential_ready_for_tests();
        assert!(install_mobile_body_key(1, &[7u8; 32]));
        let dir = tempfile::tempdir().unwrap();

        let err = match start_inner(dir.path().to_str().unwrap()) {
            Ok(_) => panic!("start_inner must fail without the agent credential key"),
            Err(err) => err,
        };

        assert!(
            err.contains("agent credential storage setup failed"),
            "startup must fail closed before opening local data without the agent credential key: {err}"
        );
        assert!(
            !dir.path()
                .join("archive")
                .join(".isyncyou-store.db")
                .exists(),
            "no store is created when agent credential setup failed"
        );
    }

    #[test]
    fn mobile_body_key_install_rejects_bad_length_and_panic() {
        let _guard = mobile_key_test_guard();
        reset_mobile_encryption_ready_for_tests();
        assert!(!install_mobile_body_key(1, &[1, 2, 3]));
        assert!(!mobile_encryption_ready());

        TEST_FAIL_NEXT_MOBILE_KEY_INSTALL.store(true, Ordering::SeqCst);
        assert!(!install_mobile_body_key(1, &[7u8; 32]));
        assert!(!mobile_encryption_ready());
    }

    #[test]
    fn mobile_body_key_install_marks_encryption_ready_on_success() {
        let _guard = mobile_key_test_guard();
        reset_mobile_encryption_ready_for_tests();

        assert!(install_mobile_body_key(1, &[7u8; 32]));

        assert!(mobile_encryption_ready());
    }

    #[test]
    fn mobile_agent_credential_key_install_rejects_bad_length_and_panic() {
        let _guard = mobile_key_test_guard();
        reset_mobile_agent_credential_ready_for_tests();
        assert!(!install_mobile_agent_credential_key(&[1, 2, 3]));
        assert!(!mobile_agent_credential_ready());

        TEST_FAIL_NEXT_MOBILE_AGENT_CREDENTIAL_KEY_INSTALL.store(true, Ordering::SeqCst);
        assert!(!install_mobile_agent_credential_key(&[7u8; 32]));
        assert!(!mobile_agent_credential_ready());
    }

    #[test]
    fn mobile_agent_credential_key_install_marks_ready_on_success() {
        let _guard = mobile_key_test_guard();
        reset_mobile_agent_credential_ready_for_tests();

        assert!(install_mobile_agent_credential_key(&[7u8; 32]));

        assert!(mobile_agent_credential_ready());
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
    fn mobile_config_prepare_canonicalizes_me_without_deleting_other_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let other_sync = PathBuf::from("/other/sync");
        let other_archive = PathBuf::from("/other/archive");
        let other_cache = PathBuf::from("/other/cache");
        let mut cfg = Config {
            accounts: vec![
                AccountConfig {
                    id: ACCOUNT.into(),
                    username: "custom".into(),
                    sync_root: PathBuf::from("/wrong/sync"),
                    archive_root: PathBuf::from("/wrong/archive"),
                    cache_root: PathBuf::from("/wrong/cache"),
                    mount_point: Some(PathBuf::from("/mnt/old")),
                },
                AccountConfig {
                    id: "other".into(),
                    username: "other".into(),
                    sync_root: other_sync.clone(),
                    archive_root: other_archive.clone(),
                    cache_root: other_cache.clone(),
                    mount_point: Some(PathBuf::from("/mnt/other")),
                },
            ],
            ..Default::default()
        };
        cfg.onedrive_modes
            .insert("other".into(), isyncyou_core::OneDriveModes::default());

        prepare_mobile_config_for_files_dir(&mut cfg, dir.path()).unwrap();

        assert_eq!(cfg.accounts.len(), 2, "unrelated accounts stay present");
        let me = cfg.accounts.iter().find(|a| a.id == ACCOUNT).unwrap();
        assert_eq!(me.archive_root, dir.path().join("archive"));
        assert_eq!(me.sync_root, dir.path().join("sync"));
        assert_eq!(me.cache_root, dir.path().join("cache"));
        assert!(me.mount_point.is_none());
        let other = cfg.accounts.iter().find(|a| a.id == "other").unwrap();
        assert_eq!(other.sync_root, other_sync);
        assert_eq!(other.archive_root, other_archive);
        assert_eq!(other.cache_root, other_cache);
        assert_eq!(other.mount_point.as_deref(), Some(Path::new("/mnt/other")));
    }

    #[test]
    fn mobile_config_prepare_rejects_invalid_foreign_onedrive_modes() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.onedrive_modes
            .insert("ghost".into(), isyncyou_core::OneDriveModes::default());

        let err = prepare_mobile_config_for_files_dir(&mut cfg, dir.path()).unwrap_err();

        assert!(
            err.contains("unknown account id 'ghost'"),
            "foreign invalid modes must still fail validation: {err}"
        );
        assert!(
            cfg.onedrive_modes.contains_key("ghost"),
            "preparation must not delete invalid foreign entries"
        );
    }

    #[test]
    fn mobile_config_reload_accepts_change_canonicalizes_and_does_not_save() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("isyncyou.toml");
        let mut previous = Config::default();
        prepare_mobile_config_for_files_dir(&mut previous, dir.path()).unwrap();

        let mut on_disk = previous.clone();
        let me = on_disk
            .accounts
            .iter_mut()
            .find(|a| a.id == ACCOUNT)
            .unwrap();
        me.sync_root = PathBuf::from("/wrong/sync");
        me.archive_root = PathBuf::from("/wrong/archive");
        me.cache_root = PathBuf::from("/wrong/cache");
        on_disk
            .onedrive_modes
            .entry(ACCOUNT.into())
            .or_default()
            .folder_modes
            .insert("F_sync".into(), isyncyou_core::OneDriveMode::Sync);
        on_disk.save(&config_path).unwrap();
        let before = std::fs::read_to_string(&config_path).unwrap();
        let warned = AtomicBool::new(true);

        let loaded = load_mobile_loop_config(&config_path, dir.path(), &previous, &warned);
        let after = std::fs::read_to_string(&config_path).unwrap();

        assert_eq!(after, before, "loop reload must not rewrite TOML");
        assert!(
            !warned.load(Ordering::Relaxed),
            "successful reload resets the failure-period warning"
        );
        let me = loaded.accounts.iter().find(|a| a.id == ACCOUNT).unwrap();
        assert_eq!(me.archive_root, dir.path().join("archive"));
        assert_eq!(me.sync_root, dir.path().join("sync"));
        assert_eq!(me.cache_root, dir.path().join("cache"));
        assert_eq!(
            loaded.onedrive_modes[ACCOUNT].folder_modes["F_sync"],
            isyncyou_core::OneDriveMode::Sync
        );
    }

    #[test]
    fn mobile_config_reload_rejects_invalid_toml_and_invalid_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("isyncyou.toml");
        let mut previous = Config::default();
        prepare_mobile_config_for_files_dir(&mut previous, dir.path()).unwrap();
        previous
            .onedrive_modes
            .entry(ACCOUNT.into())
            .or_default()
            .folder_modes
            .insert("keep".into(), isyncyou_core::OneDriveMode::Offline);

        std::fs::write(&config_path, "not = [").unwrap();
        let warned = AtomicBool::new(false);
        let parsed = load_mobile_loop_config(&config_path, dir.path(), &previous, &warned);
        assert_eq!(parsed, previous, "parse failure keeps last-known-good");
        assert!(
            warned.load(Ordering::Relaxed),
            "parse failure marks the warning period"
        );

        let mut invalid = previous.clone();
        invalid
            .onedrive_modes
            .insert("ghost".into(), isyncyou_core::OneDriveModes::default());
        invalid.save(&config_path).unwrap();
        let warned = AtomicBool::new(false);
        let validated = load_mobile_loop_config(&config_path, dir.path(), &previous, &warned);
        assert_eq!(
            validated, previous,
            "validation failure keeps last-known-good"
        );
        assert!(
            warned.load(Ordering::Relaxed),
            "validation failure marks the warning period"
        );
    }

    #[test]
    fn mobile_config_start_inner_forces_poll_interval_and_persists_once() {
        let _guard = mobile_key_test_guard();
        install_test_mobile_encryption();
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("isyncyou.toml");
        let mut cfg = Config::default();
        prepare_mobile_config_for_files_dir(&mut cfg, dir.path()).unwrap();
        cfg.sync.poll_interval_secs = 7;
        cfg.save(&config_path).unwrap();

        let _engine = start_inner(dir.path().to_str().unwrap()).expect("engine starts");
        let saved = Config::load(&config_path).unwrap();

        assert_eq!(saved.sync.poll_interval_secs, 30);
        let me = saved.accounts.iter().find(|a| a.id == ACCOUNT).unwrap();
        assert_eq!(me.archive_root, dir.path().join("archive"));
        assert_eq!(me.sync_root, dir.path().join("sync"));
        assert_eq!(me.cache_root, dir.path().join("cache"));
    }

    #[test]
    fn mobile_start_inner_clears_legacy_plaintext_cache_but_keeps_tokens() {
        let _guard = mobile_key_test_guard();
        install_test_mobile_encryption();
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("archive");
        let cache = dir.path().join("cache");
        let sync = dir.path().join("sync");
        std::fs::create_dir_all(&archive).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::create_dir_all(&sync).unwrap();
        std::fs::write(
            archive.join(".isyncyou-store.db"),
            b"SQLite format 3\0legacy plaintext",
        )
        .unwrap();
        std::fs::write(archive.join(".isyncyou-store.db-wal"), b"wal").unwrap();
        std::fs::write(archive.join(".isyncyou-store.db-shm"), b"shm").unwrap();
        std::fs::write(archive.join(".isyncyou-token-write.json"), b"token").unwrap();
        std::fs::write(cache.join("plain-cache.txt"), b"sentinel").unwrap();
        std::fs::write(sync.join("plain-sync.txt"), b"sentinel").unwrap();

        let _engine = start_inner(dir.path().to_str().unwrap()).expect("engine starts");

        assert!(!archive.join(".isyncyou-store.db").exists());
        assert!(!archive.join(".isyncyou-store.db-wal").exists());
        assert!(!archive.join(".isyncyou-store.db-shm").exists());
        assert_eq!(
            std::fs::read(archive.join(".isyncyou-token-write.json")).unwrap(),
            b"token"
        );
        assert!(!cache.join("plain-cache.txt").exists());
        assert!(!sync.join("plain-sync.txt").exists());
    }

    #[test]
    fn onedrive_explicit_scopes_detect_sync_and_offline_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        prepare_mobile_config_for_files_dir(&mut cfg, dir.path()).unwrap();
        assert!(!has_onedrive_explicit_scopes(&cfg, ACCOUNT));

        cfg.onedrive_modes
            .entry(ACCOUNT.into())
            .or_default()
            .default_mode = OneDriveMode::Sync;
        assert!(
            !has_onedrive_explicit_scopes(&cfg, ACCOUNT),
            "default_mode alone is intentionally not a mobile scoped root"
        );

        cfg.onedrive_modes
            .entry(ACCOUNT.into())
            .or_default()
            .folder_modes
            .insert("A".into(), OneDriveMode::Online);
        assert!(!has_onedrive_explicit_scopes(&cfg, ACCOUNT));

        cfg.onedrive_modes
            .entry(ACCOUNT.into())
            .or_default()
            .folder_modes
            .insert("B".into(), OneDriveMode::Sync);
        assert!(
            has_onedrive_explicit_scopes(&cfg, ACCOUNT),
            "mixed Online + Sync still has scoped work"
        );

        cfg.onedrive_modes
            .entry(ACCOUNT.into())
            .or_default()
            .folder_modes
            .insert("B".into(), OneDriveMode::Online);
        cfg.onedrive_modes
            .entry(ACCOUNT.into())
            .or_default()
            .folder_modes
            .insert("C".into(), OneDriveMode::Offline);
        assert!(has_onedrive_explicit_scopes(&cfg, ACCOUNT));

        cfg.onedrive_modes
            .entry(ACCOUNT.into())
            .or_default()
            .folder_modes
            .insert("C".into(), OneDriveMode::Online);
        assert!(
            !has_onedrive_explicit_scopes(&cfg, ACCOUNT),
            "switching every explicit folder back online stops scoped work"
        );
    }

    #[test]
    fn onedrive_sync_report_changed_is_ui_data_refresh_signal() {
        assert!(
            !sync_report_changed(&isyncyou_engine::SyncReport::default()),
            "zero report must not refresh the UI"
        );
        assert!(sync_report_changed(&isyncyou_engine::SyncReport {
            upserted: 1,
            ..Default::default()
        }));
        assert!(sync_report_changed(&isyncyou_engine::SyncReport {
            deleted: 1,
            ..Default::default()
        }));
        assert!(sync_report_changed(&isyncyou_engine::SyncReport {
            resynced: true,
            ..Default::default()
        }));
        assert!(sync_report_changed(&isyncyou_engine::SyncReport {
            downloaded: 1,
            ..Default::default()
        }));
        assert!(sync_report_changed(&isyncyou_engine::SyncReport {
            modified_conflicts: 1,
            ..Default::default()
        }));

        for report in [
            isyncyou_engine::SyncReport {
                skipped: 1,
                ..Default::default()
            },
            isyncyou_engine::SyncReport {
                materialize_failed: 1,
                ..Default::default()
            },
            isyncyou_engine::SyncReport {
                materialize_cancelled: 1,
                ..Default::default()
            },
            isyncyou_engine::SyncReport {
                modified_failed: 1,
                ..Default::default()
            },
            isyncyou_engine::SyncReport {
                local_delete_blocked: Some("blocked".into()),
                ..Default::default()
            },
            isyncyou_engine::SyncReport {
                cloud_delete_blocked: Some("blocked".into()),
                ..Default::default()
            },
        ] {
            assert!(
                !sync_report_changed(&report),
                "status/error-only reports must not trigger full UI refresh: {report:?}"
            );
        }
    }

    #[test]
    fn onedrive_scoped_pass_waits_for_store_gate_before_token_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        prepare_mobile_config_for_files_dir(&mut cfg, dir.path()).unwrap();
        cfg.onedrive_modes
            .entry(ACCOUNT.into())
            .or_default()
            .folder_modes
            .insert("F_sync".into(), OneDriveMode::Sync);
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let progress = isyncyou_app_host::SharedProgress::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let thread_cfg = cfg.clone();
        let thread_gate = Arc::clone(&gate);
        let thread_progress = progress.clone();

        let handle = std::thread::spawn(move || {
            let changed = run_onedrive_scoped_pass(&thread_cfg, &thread_gate, &thread_progress);
            tx.send(changed).unwrap();
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "scoped pass must not pass token lookup while the store gate is held"
        );
        drop(held);
        let changed = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("scoped pass returns after gate release");
        assert!(
            !changed,
            "without a cached sync/write token the pass returns false"
        );
        handle.join().unwrap();
    }

    #[test]
    fn standalone_full_node_serves_ui_and_gates_restore_backup() {
        // #89 P7 / #0A (host slice): the embedded engine — the exact code that runs on the
        // phone — serves the UI shell and fully session-token gates the data API **entirely
        // in-process**, with NO loopback TCP port. `asset_request` serves the shell (as the
        // WebView's shouldInterceptRequest does); `bridge_request` carries the data API.
        let _guard = mobile_key_test_guard();
        install_test_mobile_encryption();
        let dir = tempfile::tempdir().unwrap();
        restore_enabled_mobile_config(dir.path());
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
        // Restore/backup are now present in the mobile full-node profile, but still gated by
        // the injected cap token and then the native per-action biometric token.
        let restore_no_cap = bridge_request(&format!(
            r#"{{"method":"POST","path":"/api/v1/restore?account=me&service=mail&id=x","headers":{{"X-Session-Token":"{tok}"}}}}"#
        ));
        assert!(
            restore_no_cap.contains("\"status\":401"),
            "restore must be wired and cap-gated, not absent: {restore_no_cap}"
        );
        let backup_no_cap = bridge_request(&format!(
            r#"{{"method":"POST","path":"/api/v1/backup?account=me&services=mail","headers":{{"X-Session-Token":"{tok}"}}}}"#
        ));
        assert!(
            backup_no_cap.contains("\"status\":401"),
            "backup must be wired and cap-gated, not absent: {backup_no_cap}"
        );
    }

    #[test]
    fn mobile_full_node_router_exposes_gated_restore_and_backup() {
        let _guard = mobile_key_test_guard();
        install_test_mobile_encryption();
        let dir = tempfile::tempdir().unwrap();
        restore_enabled_mobile_config(dir.path());
        let (tok, router, _) = start_inner(dir.path().to_str().unwrap()).expect("engine starts");

        for path in [
            "/api/v1/restore?account=me&service=mail&id=x",
            "/api/v1/backup?account=me&services=mail",
            "/api/v1/jobs?account=me",
            "/api/v1/jobs/cancel?account=me&job_id=job-1",
        ] {
            let method = if path == "/api/v1/jobs?account=me" {
                "GET"
            } else {
                "POST"
            };
            let no_session = router.route(&isyncyou_webui::ApiRequest::new(method, path));
            assert_eq!(no_session.status, 401, "session-gated: {path}");
            let with_session = router.route(
                &isyncyou_webui::ApiRequest::new(method, path)
                    .with_session_token(Some(tok.clone())),
            );
            assert_eq!(with_session.status, 401, "cap-gated, not absent: {path}");
        }

        assert!(!cap_from_app_js(&router, "restore").is_empty());
        assert!(!cap_from_app_js(&router, "backup").is_empty());
        assert!(!cap_from_app_js(&router, "mobileJobs").is_empty());
    }

    #[test]
    fn mobile_full_node_restore_backup_do_not_run_without_biometric_token() {
        let _guard = mobile_key_test_guard();
        install_test_mobile_encryption();
        let dir = tempfile::tempdir().unwrap();
        restore_enabled_mobile_config(dir.path());
        let (tok, router, _) = start_inner(dir.path().to_str().unwrap()).expect("engine starts");
        let restore_cap = cap_from_app_js(&router, "restore");
        let backup_cap = cap_from_app_js(&router, "backup");
        let jobs_cap = cap_from_app_js(&router, "mobileJobs");

        let list_jobs = || {
            api_json(
                router.route(
                    &isyncyou_webui::ApiRequest::get("/api/v1/jobs?account=me")
                        .with_session_token(Some(tok.clone()))
                        .with_cap_token(Some(jobs_cap.clone())),
                ),
            )
        };

        let restore_challenge = api_json(
            router.route(
                &isyncyou_webui::ApiRequest::new(
                    "POST",
                    "/api/v1/restore?account=me&service=mail&id=restore-src-1",
                )
                .with_session_token(Some(tok.clone()))
                .with_cap_token(Some(restore_cap.clone())),
            ),
        );
        assert_eq!(
            restore_challenge["status"].as_str(),
            Some("confirmation_required")
        );
        assert_eq!(
            list_jobs()["jobs"].as_array().unwrap().len(),
            0,
            "restore must not enqueue before biometric token"
        );
        let restore_pat = restore_challenge["pending_action_id"].as_str().unwrap();
        assert!(router.confirm_biometric(restore_pat));
        let restore_ok = api_json(
            router.route(
                &isyncyou_webui::ApiRequest::new(
                    "POST",
                    &format!(
                    "/api/v1/restore?account=me&service=mail&id=restore-src-1&_pat={restore_pat}"
                ),
                )
                .with_session_token(Some(tok.clone()))
                .with_cap_token(Some(restore_cap)),
            ),
        );
        assert_eq!(restore_ok["queued"].as_bool(), Some(true));
        assert_eq!(restore_ok["kind"].as_str(), Some("restore-cloud"));
        assert_eq!(restore_ok["state"].as_str(), Some("queued"));

        let backup_challenge = api_json(
            router.route(
                &isyncyou_webui::ApiRequest::new("POST", "/api/v1/backup?account=me&services=mail")
                    .with_session_token(Some(tok.clone()))
                    .with_cap_token(Some(backup_cap.clone())),
            ),
        );
        assert_eq!(
            backup_challenge["status"].as_str(),
            Some("confirmation_required")
        );
        assert_eq!(
            list_jobs()["jobs"].as_array().unwrap().len(),
            1,
            "backup must not enqueue before biometric token"
        );
        let backup_pat = backup_challenge["pending_action_id"].as_str().unwrap();
        assert!(router.confirm_biometric(backup_pat));
        let backup_ok = api_json(
            router.route(
                &isyncyou_webui::ApiRequest::new(
                    "POST",
                    &format!("/api/v1/backup?account=me&services=mail&_pat={backup_pat}"),
                )
                .with_session_token(Some(tok.clone()))
                .with_cap_token(Some(backup_cap)),
            ),
        );
        assert_eq!(backup_ok["queued"].as_bool(), Some(true));
        assert_eq!(backup_ok["kind"].as_str(), Some("backup"));
        assert_eq!(backup_ok["state"].as_str(), Some("queued"));
        assert_eq!(
            list_jobs()["jobs"].as_array().unwrap().len(),
            2,
            "both confirmed jobs are visible in the mobile job list"
        );
    }

    #[test]
    fn mobile_full_node_job_recovery_runs_on_start() {
        let _guard = mobile_key_test_guard();
        install_test_mobile_encryption();
        let dir = tempfile::tempdir().unwrap();
        restore_enabled_mobile_config(dir.path());
        let cfg = Config::load(dir.path().join("isyncyou.toml")).unwrap();
        let archive = dir.path().join("archive");
        std::fs::create_dir_all(&archive).unwrap();
        {
            let store = isyncyou_store::Store::open(archive.join(".isyncyou-store.db")).unwrap();
            store
                .create_mobile_job(
                    "start-recovery-job",
                    "me",
                    isyncyou_store::MobileJobKind::Backup,
                    None,
                    None,
                    "backup:me:mail",
                    r#"{"op":"backup","account":"me","services":["mail"]}"#,
                    1,
                )
                .unwrap();
        }

        let runtime = isyncyou_app_host::MobileJobRuntime::new(
            cfg,
            Arc::new(Mutex::new(())),
            Arc::new(isyncyou_webui::EventBus::new()),
        );
        let (jobs, truncated) = runtime
            .mobile_worker_plan("me")
            .expect("worker plan should list queued jobs");
        assert!(!truncated);
        assert_eq!(
            jobs,
            vec![(
                "start-recovery-job".to_string(),
                isyncyou_store::MobileJobKind::Backup
            )]
        );

        let store = isyncyou_store::Store::open(archive.join(".isyncyou-store.db")).unwrap();
        let job = store
            .get_mobile_job("start-recovery-job")
            .unwrap()
            .expect("job remains recorded");
        assert_eq!(job.state, isyncyou_store::MobileJobState::Queued);
    }

    #[test]
    fn bridge_request_routes_against_the_running_engine_without_a_port() {
        // #0A: the in-process bridge answers against the same router as loopback and
        // enforces the same session gate — proving the phone needs no TCP port to serve
        // its own UI's data calls.
        let _guard = mobile_key_test_guard();
        install_test_mobile_encryption();
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
        let _guard = mobile_key_test_guard();
        install_test_mobile_encryption();
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
    fn asset_request_with_session_uses_trusted_session_not_cookie_or_query() {
        let _guard = mobile_key_test_guard();
        install_test_mobile_encryption();
        let dir = tempfile::tempdir().unwrap();
        start_engine(dir.path().to_str().unwrap()).expect("engine starts");
        let tok = session_token().expect("token");

        assert_eq!(
            frame_status(&asset_request_with_session("/", Some(&tok))),
            200,
            "trusted native session serves the shell"
        );
        assert_eq!(
            frame_status(&asset_request_with_session("/", None)),
            401,
            "missing trusted session must not serve a half-open shell"
        );
        assert_eq!(
            frame_status(&asset_request_with_session("/", Some(""))),
            401,
            "empty trusted session must not serve a half-open shell"
        );
        assert_eq!(
            frame_status(&asset_request_with_session("/", Some("wrong"))),
            401,
            "wrong trusted session must not serve even static app-origin assets"
        );
        assert_eq!(
            frame_status(&asset_request_with_session(
                &format!("/api/v1/status?_st={tok}"),
                None,
            )),
            401,
            "_st query must not authorize MainActivity asset requests"
        );
        assert_ne!(
            frame_status(&asset_request_with_session(
                "/api/v1/status?_st=wrong",
                Some(&tok),
            )),
            401,
            "trusted native session, not _st, authorizes app-origin API GETs"
        );
    }

    #[test]
    fn asset_request_with_session_frames_not_ready_as_503() {
        assert_eq!(
            frame_status(&asset_request_with_session_for_router(
                None,
                "/",
                Some("session")
            )),
            503,
            "not-ready engine must frame 503 rather than return an empty response"
        );
    }

    #[test]
    fn stream_registry_opens_gated_and_closes() {
        // #0A: the push-stream FFI plumbing — gating + open/close registry. Event delivery
        // semantics are proven in webui's open_bridge_stream test; the full push round-trip
        // is device-verified.
        let _guard = mobile_key_test_guard();
        install_test_mobile_encryption();
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
