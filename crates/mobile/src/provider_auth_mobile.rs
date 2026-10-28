//! Native Android OAuth entry points. Authorization secrets never cross the WebView bridge.

use super::{jni_get_string, jni_new_string, prepare_mobile_config_for_files_dir};
use crate::provider_credentials::AndroidProviderCredentialStore;
use isyncyou_engine::provider_auth::{self, AuthAttempt};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

static ATTEMPT: OnceLock<Mutex<Option<AuthAttempt>>> = OnceLock::new();
fn attempts() -> &'static Mutex<Option<AuthAttempt>> {
    ATTEMPT.get_or_init(|| Mutex::new(None))
}

fn config(base: &Path) -> Result<isyncyou_core::Config, String> {
    let mut cfg = isyncyou_core::Config::load(base.join("isyncyou.toml"))?;
    prepare_mobile_config_for_files_dir(&mut cfg, base)?;
    Ok(cfg)
}

fn begin(base: &Path, account: &str, redirect: &str) -> Result<String, String> {
    let mut guard = attempts().lock().map_err(|_| "Authorization unavailable")?;
    let cfg = config(base)?;
    let _ = AndroidProviderCredentialStore::new(base)?;
    let attempt = provider_auth::start_self_hosted_auth(&cfg, account, redirect)?;
    let uri = attempt.authorization_uri.clone();
    *guard = Some(attempt);
    Ok(uri)
}

fn finish(base: &Path, callback: &str) -> Result<(), String> {
    let mut guard = attempts().lock().map_err(|_| "Authorization unavailable")?;
    let attempt = guard.as_ref().ok_or("No authorization pending")?;
    // An unrelated externally delivered URI must not consume the genuine attempt.
    provider_auth::validate_callback(attempt, callback)?;
    let store = AndroidProviderCredentialStore::new(base)?;
    let attempt = guard.take().ok_or("No authorization pending")?;
    // Keep the operation lock through token exchange and replacement so logout
    // cannot race an in-flight callback and resurrect a revoked grant.
    provider_auth::finish_self_hosted_auth_with_store(attempt, callback, &store)
}

fn status(base: &Path, account: &str) -> Result<bool, String> {
    let cfg = config(base)?;
    let store = AndroidProviderCredentialStore::new(base)?;
    provider_auth::self_hosted_auth_status_with_store(&cfg, account, &store)
}

fn logout(base: &Path, account: &str) -> Result<(), String> {
    let mut attempt = attempts().lock().map_err(|_| "Authorization unavailable")?;
    *attempt = None;
    let cfg = config(base)?;
    let store = AndroidProviderCredentialStore::new(base)?;
    provider_auth::logout_self_hosted_with_store(&cfg, account, &store)
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeProviderBegin<'a>(
    mut env: jni::EnvUnowned<'a>,
    _class: jni::objects::JClass<'a>,
    base: jni::objects::JString<'a>,
    account: jni::objects::JString<'a>,
    redirect: jni::objects::JString<'a>,
) -> jni::sys::jstring {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let base = jni_get_string(&mut env, &base)?;
        let account = jni_get_string(&mut env, &account)?;
        let redirect = jni_get_string(&mut env, &redirect)?;
        begin(Path::new(&base), &account, &redirect).ok()
    }))
    .ok()
    .flatten()
    .unwrap_or_default();
    jni_new_string(&mut env, result)
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeProviderFinish<'a>(
    mut env: jni::EnvUnowned<'a>,
    _class: jni::objects::JClass<'a>,
    base: jni::objects::JString<'a>,
    callback: jni::objects::JString<'a>,
) -> jni::sys::jint {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(base) = jni_get_string(&mut env, &base) else {
            return 0;
        };
        let Some(callback) = jni_get_string(&mut env, &callback) else {
            return 0;
        };
        i32::from(finish(Path::new(&base), &callback).is_ok())
    }))
    .unwrap_or(0)
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeProviderStatus<'a>(
    mut env: jni::EnvUnowned<'a>,
    _class: jni::objects::JClass<'a>,
    base: jni::objects::JString<'a>,
    account: jni::objects::JString<'a>,
) -> jni::sys::jint {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(base) = jni_get_string(&mut env, &base) else {
            return -1;
        };
        let Some(account) = jni_get_string(&mut env, &account) else {
            return -1;
        };
        match status(Path::new(&base), &account) {
            Ok(true) => 1,
            Ok(false) => 0,
            Err(_) => -1,
        }
    }))
    .unwrap_or(-1)
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeProviderLogout<'a>(
    mut env: jni::EnvUnowned<'a>,
    _class: jni::objects::JClass<'a>,
    base: jni::objects::JString<'a>,
    account: jni::objects::JString<'a>,
) -> jni::sys::jint {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(base) = jni_get_string(&mut env, &base) else {
            return 0;
        };
        let Some(account) = jni_get_string(&mut env, &account) else {
            return 0;
        };
        i32::from(logout(Path::new(&base), &account).is_ok())
    }))
    .unwrap_or(0)
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeProviderCancel<'a>(
    _env: jni::EnvUnowned<'a>,
    _class: jni::objects::JClass<'a>,
) {
    if let Ok(mut attempt) = attempts().lock() {
        *attempt = None;
    }
}

/// Native-only broker call. The WebView must never receive this response: it may
/// contain a short-lived distributor credential or an endpoint capability.
#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeProviderBroker<'a>(
    mut env: jni::EnvUnowned<'a>,
    _class: jni::objects::JClass<'a>,
    base: jni::objects::JString<'a>,
    account: jni::objects::JString<'a>,
    operation: jni::objects::JString<'a>,
    request: jni::objects::JString<'a>,
) -> jni::sys::jstring {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
        || -> Result<serde_json::Value, &'static str> {
            let base = jni_get_string(&mut env, &base).ok_or("invalid_request")?;
            let account = jni_get_string(&mut env, &account).ok_or("invalid_request")?;
            let operation = jni_get_string(&mut env, &operation).ok_or("invalid_request")?;
            let request = jni_get_string(&mut env, &request).ok_or("invalid_request")?;
            if request.len() > 8192 {
                return Err("invalid_request");
            }
            let request: serde_json::Value =
                serde_json::from_str(&request).map_err(|_| "invalid_request")?;
            let cfg = config(Path::new(&base)).map_err(|_| "config_unavailable")?;
            let data =
                isyncyou_engine::provider_push_broker::post(&cfg, &account, &operation, &request)?;
            Ok(serde_json::json!({"ok":true,"data":data}))
        },
    ))
    .unwrap_or(Err("unavailable"));
    let response = match result {
        Ok(value) => value.to_string(),
        Err(reason) => serde_json::json!({"ok":false,"reason":reason}).to_string(),
    };
    jni_new_string(&mut env, response)
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeProviderPush<'a>(
    mut env: jni::EnvUnowned<'a>,
    _class: jni::objects::JClass<'a>,
    base: jni::objects::JString<'a>,
    account: jni::objects::JString<'a>,
    operation: jni::objects::JString<'a>,
    request: jni::objects::JString<'a>,
) -> jni::sys::jstring {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
        || -> Result<serde_json::Value, &'static str> {
            let base = jni_get_string(&mut env, &base).ok_or("invalid_request")?;
            let account = jni_get_string(&mut env, &account).ok_or("invalid_request")?;
            let operation = jni_get_string(&mut env, &operation).ok_or("invalid_request")?;
            let request = jni_get_string(&mut env, &request).ok_or("invalid_request")?;
            if request.len() > 8192 {
                return Err("invalid_request");
            }
            let request: serde_json::Value =
                serde_json::from_str(&request).map_err(|_| "invalid_request")?;
            let cfg = config(Path::new(&base)).map_err(|_| "config_unavailable")?;
            let data =
                isyncyou_engine::provider_push::perform(&cfg, &account, &operation, &request)?;
            Ok(serde_json::json!({"ok":true,"data":data}))
        },
    ))
    .unwrap_or(Err("unavailable"));
    let response = match result {
        Ok(value) => value.to_string(),
        Err(reason) => serde_json::json!({"ok":false,"reason":reason}).to_string(),
    };
    jni_new_string(&mut env, response)
}
