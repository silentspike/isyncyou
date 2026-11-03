//! Native Android OAuth entry points. Authorization secrets never cross the WebView bridge.

use super::{jni_get_string, jni_new_string, prepare_mobile_config_for_files_dir};
use crate::provider_credentials::AndroidProviderCredentialStore;
use isyncyou_engine::provider_auth::{self, AuthAttempt};
use isyncyou_engine::provider_credential::{ProviderCredential, ProviderCredentialStore};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

static ATTEMPT: OnceLock<Mutex<Option<AuthAttempt>>> = OnceLock::new();
struct UnknownRecovery {
    account: String,
    origin: String,
    journal: Vec<u8>,
    credential: Option<ProviderCredential>,
    subject: Option<String>,
}
static UNKNOWN_RECOVERY: OnceLock<Mutex<Option<UnknownRecovery>>> = OnceLock::new();
fn unknown_recovery() -> &'static Mutex<Option<UnknownRecovery>> {
    UNKNOWN_RECOVERY.get_or_init(|| Mutex::new(None))
}
fn attempts() -> &'static Mutex<Option<AuthAttempt>> {
    ATTEMPT.get_or_init(|| Mutex::new(None))
}

fn same_credential(a: &Option<ProviderCredential>, b: &Option<ProviderCredential>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.access_token == b.access_token && a.refresh_token == b.refresh_token &&
            a.expires_at == b.expires_at && a.subject == b.subject,
        _ => false,
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    fn credential() -> ProviderCredential {
        ProviderCredential { access_token: "a".into(), refresh_token: Some("r".into()), expires_at: 1, subject: Some("s".into()) }
    }
    #[test]
    fn recovery_snapshot_rejects_changed_grant() {
        assert!(same_credential(&None, &None));
        assert!(same_credential(&Some(credential()), &Some(credential())));
        let mut changed = credential();
        changed.refresh_token = Some("other".into());
        assert!(!same_credential(&Some(credential()), &Some(changed)));
        assert!(!same_credential(&Some(credential()), &None));
    }
}

fn prepare_unknown_recovery(base: &Path, account: &str) -> Result<bool, String> {
    let _guard = attempts().lock().map_err(|_| "Authorization unavailable")?;
    *unknown_recovery().lock().map_err(|_| "Authorization unavailable")? = None;
    let store = AndroidProviderCredentialStore::new(base)?;
    let journal = store.read_auth_journal()?.ok_or("No unknown authorization")?;
    let record: serde_json::Value = serde_json::from_slice(&journal).map_err(|_| "Invalid authorization journal")?;
    if record["stage"] != "exchange_started" || record["account"].as_str() != Some(account) {
        return Err("Authorization state changed".into());
    }
    let bytes: Vec<u8> = serde_json::from_value(record["attempt"].clone()).map_err(|_| "Invalid authorization journal")?;
    let saved: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| "Invalid authorization journal")?;
    let origin = saved["origin"].as_str().ok_or("Invalid authorization journal")?.to_owned();
    isyncyou_core::Config::with_config_lock(base.join("isyncyou.toml"), || {
    let cfg = config(base)?;
    if !matches!(cfg.provider_accounts.get(account), Some(isyncyou_core::config::ProviderConfig::SelfHosted { origin: current, client_id })
        if current == &origin && saved["client_id"].as_str() == Some(client_id.as_str())) ||
        cfg.accounts.iter().find(|value| value.id == account).map(|value| value.username.as_str()) != saved["expected_username"].as_str() {
        return Err("Account or origin changed".into());
    }
    provider_auth::with_locked_provider_subject_with_store(&cfg, account, &store, |subject| {
        let credential = store.load(account, &origin)?;
        let connected = subject.is_some();
        *unknown_recovery().lock().map_err(|_| "Authorization unavailable")? = Some(UnknownRecovery {
            account: account.into(), origin, journal, credential, subject: subject.map(str::to_owned),
        });
        Ok(connected)
    })
    })
}

fn abort_unknown_recovery(base: &Path, account: &str) -> Result<(), String> {
    let _guard = attempts().lock().map_err(|_| "Authorization unavailable")?;
    let confirmation = unknown_recovery().lock().map_err(|_| "Authorization unavailable")?.take()
        .ok_or("No authorization recovery confirmation")?;
    if confirmation.account != account { return Err("Account changed".into()); }
    isyncyou_core::Config::with_config_lock(base.join("isyncyou.toml"), || {
    let cfg = config(base)?;
    let journal_value: serde_json::Value = serde_json::from_slice(&confirmation.journal).map_err(|_| "Invalid authorization journal")?;
    let saved_bytes: Vec<u8> = serde_json::from_value(journal_value["attempt"].clone()).map_err(|_| "Invalid authorization journal")?;
    let saved: serde_json::Value = serde_json::from_slice(&saved_bytes).map_err(|_| "Invalid authorization journal")?;
    if !matches!(cfg.provider_accounts.get(account), Some(isyncyou_core::config::ProviderConfig::SelfHosted { origin, client_id })
        if origin == &confirmation.origin && saved["client_id"].as_str() == Some(client_id.as_str())) ||
        cfg.accounts.iter().find(|value| value.id == account).map(|value| value.username.as_str()) != saved["expected_username"].as_str() {
        return Err("Account or origin changed".into());
    }
    let store = AndroidProviderCredentialStore::new(base)?;
    provider_auth::with_locked_provider_subject_with_store(&cfg, account, &store, |subject| {
        if store.read_auth_journal()?.as_deref() != Some(confirmation.journal.as_slice()) ||
            !same_credential(&store.load(account, &confirmation.origin)?, &confirmation.credential) ||
            subject != confirmation.subject.as_deref() {
            return Err("Authorization state changed".into());
        }
        store.delete_auth_journal()
    })
    })
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeProviderAuthRecoveryPrepare<'a>(
    mut env: jni::EnvUnowned<'a>, _class: jni::objects::JClass<'a>, base: jni::objects::JString<'a>, account: jni::objects::JString<'a>,
) -> jni::sys::jint {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(base) = jni_get_string(&mut env, &base) else { return -1 };
        let Some(account) = jni_get_string(&mut env, &account) else { return -1 };
        match prepare_unknown_recovery(Path::new(&base), &account) { Ok(true) => 1, Ok(false) => 0, Err(_) => -1 }
    })).unwrap_or(-1)
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeProviderAuthRecoveryAbort<'a>(
    mut env: jni::EnvUnowned<'a>, _class: jni::objects::JClass<'a>, base: jni::objects::JString<'a>, account: jni::objects::JString<'a>,
) -> jni::sys::jint {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(base) = jni_get_string(&mut env, &base) else { return 0 };
        let Some(account) = jni_get_string(&mut env, &account) else { return 0 };
        i32::from(abort_unknown_recovery(Path::new(&base), &account).is_ok())
    })).unwrap_or(0)
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeProviderAuthRecoveryCancel<'a>(
    _env: jni::EnvUnowned<'a>, _class: jni::objects::JClass<'a>,
) {
    if let Ok(mut confirmation) = unknown_recovery().lock() { *confirmation = None; }
}

pub(crate) fn config(base: &Path) -> Result<isyncyou_core::Config, String> {
    let mut cfg = isyncyou_core::Config::load(base.join("isyncyou.toml"))?;
    prepare_mobile_config_for_files_dir(&mut cfg, base)?;
    Ok(cfg)
}

fn create_account(base: &Path, request: &str) -> Result<(), String> {
    if request.len() > 8192 { return Err("Invalid account setup".into()); }
    let value: serde_json::Value = serde_json::from_str(request).map_err(|_| "Invalid account setup")?;
    let object = value.as_object().ok_or("Invalid account setup")?;
    if object.len() != 4 { return Err("Invalid account setup".into()); }
    let field = |key: &str, max: usize| -> Result<&str, String> {
        object.get(key).and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control))
            .ok_or_else(|| "Invalid account setup".into())
    };
    isyncyou_engine::provider_setup::create_self_hosted_account(
        &base.join("isyncyou.toml"), field("account", 64)?, field("username", 256)?,
        field("origin", 2048)?, field("client_id", 256)?, base,
    )?;
    super::wake_provider_refresh();
    Ok(())
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeProviderOrigin<'a>(
    mut env: jni::EnvUnowned<'a>, _class: jni::objects::JClass<'a>,
    base: jni::objects::JString<'a>, account: jni::objects::JString<'a>,
) -> jni::sys::jstring {
    let value = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let base = jni_get_string(&mut env, &base)?;
        let account = jni_get_string(&mut env, &account)?;
        let cfg = config(Path::new(&base)).ok()?;
        match cfg.provider_accounts.get(&account)? {
            isyncyou_core::config::ProviderConfig::SelfHosted { origin, .. } => Some(origin.clone()),
        }
    })).ok().flatten().unwrap_or_default();
    jni_new_string(&mut env, value)
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeProviderBrokerConfigured<'a>(
    mut env: jni::EnvUnowned<'a>, _class: jni::objects::JClass<'a>,
    base: jni::objects::JString<'a>, account: jni::objects::JString<'a>,
) -> jni::sys::jint {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(base) = jni_get_string(&mut env, &base) else { return -1 };
        let Some(account) = jni_get_string(&mut env, &account) else { return -1 };
        match config(Path::new(&base)) { Ok(cfg) => i32::from(cfg.push_broker_bases.contains_key(&account)), Err(_) => -1 }
    })).unwrap_or(-1)
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeProviderSetBroker<'a>(
    mut env: jni::EnvUnowned<'a>, _class: jni::objects::JClass<'a>,
    base: jni::objects::JString<'a>, account: jni::objects::JString<'a>, broker: jni::objects::JString<'a>,
) -> jni::sys::jint {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(base) = jni_get_string(&mut env, &base) else { return 0 };
        let Some(account) = jni_get_string(&mut env, &account) else { return 0 };
        let Some(broker) = jni_get_string(&mut env, &broker) else { return 0 };
        i32::from(isyncyou_engine::provider_setup::set_push_broker_base(
            &Path::new(&base).join("isyncyou.toml"), &account, Some(&broker),
        ).is_ok())
    })).unwrap_or(0)
}

fn begin(base: &Path, account: &str, redirect: &str) -> Result<String, String> {
    let mut guard = attempts().lock().map_err(|_| "Authorization unavailable")?;
    let cfg = config(base)?;
    let store = AndroidProviderCredentialStore::new(base)?;
    if let Some(raw) = store.read_auth_journal()? {
        let prior: serde_json::Value = serde_json::from_slice(&raw).map_err(|_| "Authorization recovery unavailable")?;
        if prior["stage"] != "pending" { return Err("Authorization outcome unknown".into()); }
    }
    let attempt = provider_auth::start_self_hosted_auth(&cfg, account, redirect)?;
    let serialized = provider_auth::export_android_auth_attempt(&attempt)?;
    let journal = serde_json::to_vec(&serde_json::json!({"stage":"pending", "account":account,
        "redirect":redirect, "attempt":serialized})).map_err(|_| "Authorization unavailable")?;
    store.write_auth_journal(&journal)?;
    let uri = attempt.authorization_uri.clone();
    *guard = Some(attempt);
    Ok(uri)
}

fn finish(base: &Path, callback: &str) -> Result<(), provider_auth::AuthFinishError> {
    let mut guard = attempts().lock().map_err(|_| "Authorization unavailable")?;
    let store = AndroidProviderCredentialStore::new(base)?;
    let raw = store.read_auth_journal()?.ok_or("No authorization pending")?;
    let mut journal: serde_json::Value = serde_json::from_slice(&raw).map_err(|_| "Authorization recovery unavailable")?;
    if journal["stage"] != "pending" { return Err("Authorization outcome unknown".into()); }
    let account = journal["account"].as_str().ok_or("Invalid authorization journal")?;
    let redirect = journal["redirect"].as_str().ok_or("Invalid authorization journal")?;
    let saved: Vec<u8> = serde_json::from_value(journal["attempt"].clone()).map_err(|_| "Invalid authorization journal")?;
    let cfg = config(base)?;
    let attempt = provider_auth::import_android_auth_attempt(&cfg, account, redirect, &saved)?;
    // An unrelated externally delivered URI must not consume the genuine attempt.
    if provider_auth::classify_callback(&attempt, callback)? == provider_auth::AuthCallbackKind::Rejected {
        store.delete_auth_journal()?;
        *guard = None;
        return Err("Authorization rejected".into());
    }
    journal["stage"] = serde_json::Value::String("exchange_started".into());
    store.write_auth_journal(&serde_json::to_vec(&journal).map_err(|_| "Authorization unavailable")?)?;
    *guard = None;
    // Keep the operation lock through token exchange and replacement so logout
    // cannot race an in-flight callback and resurrect a revoked grant.
    provider_auth::finish_self_hosted_auth_detailed_with_store(attempt, callback, &store)?;
    store.delete_auth_journal()?;
    Ok(())
}

fn status(base: &Path, account: &str) -> Result<bool, String> {
    let cfg = config(base)?;
    let store = AndroidProviderCredentialStore::new(base)?;
    provider_auth::self_hosted_auth_status_with_store(&cfg, account, &store)
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeProviderAuthJournalStatus<'a>(
    mut env: jni::EnvUnowned<'a>, _class: jni::objects::JClass<'a>, base: jni::objects::JString<'a>,
) -> jni::sys::jint {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<i32, String> {
        let base = jni_get_string(&mut env, &base).ok_or("Invalid request")?;
        let store = AndroidProviderCredentialStore::new(Path::new(&base))?;
        let Some(raw) = store.read_auth_journal()? else { return Ok(0) };
        let value: serde_json::Value = serde_json::from_slice(&raw).map_err(|_| "Invalid authorization journal")?;
        match value["stage"].as_str() {
            Some("pending") => Ok(1),
            Some("exchange_started") => Ok(2),
            _ => Err("Invalid authorization journal".into()),
        }
    })).ok().and_then(Result::ok).unwrap_or(-1)
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeProviderAuthJournalAccountStatus<'a>(
    mut env: jni::EnvUnowned<'a>, _class: jni::objects::JClass<'a>, base: jni::objects::JString<'a>, account: jni::objects::JString<'a>,
) -> jni::sys::jint {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<i32, String> {
        let base = jni_get_string(&mut env, &base).ok_or("Invalid request")?;
        let account = jni_get_string(&mut env, &account).ok_or("Invalid request")?;
        let store = AndroidProviderCredentialStore::new(Path::new(&base))?;
        let Some(raw) = store.read_auth_journal()? else { return Ok(0) };
        let value: serde_json::Value = serde_json::from_slice(&raw).map_err(|_| "Invalid authorization journal")?;
        let recorded = value["account"].as_str().filter(|value| !value.is_empty()).ok_or("Invalid authorization journal")?;
        let state = match value["stage"].as_str() { Some("pending") => 1, Some("exchange_started") => 2, _ => return Err("Invalid authorization journal".into()) };
        Ok(if recorded == account { state } else { 0 })
    })).ok().and_then(Result::ok).unwrap_or(-1)
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeConnectedProviderAccounts<'a>(
    mut env: jni::EnvUnowned<'a>, _class: jni::objects::JClass<'a>,
    base: jni::objects::JString<'a>,
) -> jni::sys::jstring {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<Vec<String>, String> {
        let base = jni_get_string(&mut env, &base).ok_or("Invalid request")?;
        let base = Path::new(&base);
        let cfg = config(base)?;
        let store = AndroidProviderCredentialStore::new(base)?;
        let mut accounts = Vec::new();
        for account in cfg.provider_accounts.keys() {
            if provider_auth::self_hosted_auth_status_with_store(&cfg, account, &store)? {
                accounts.push(account.clone());
            }
        }
        Ok(accounts)
    })).ok().and_then(Result::ok);
    let value = match result {
        Some(accounts) => serde_json::json!({"ok":true,"accounts":accounts}),
        None => serde_json::json!({"ok":false}),
    };
    jni_new_string(&mut env, value.to_string())
}

fn logout(base: &Path, account: &str) -> Result<(), String> {
    let mut attempt = attempts().lock().map_err(|_| "Authorization unavailable")?;
    let cfg = config(base)?;
    let store = AndroidProviderCredentialStore::new(base)?;
    if let Some(raw) = store.read_auth_journal()? {
        let journal: serde_json::Value = serde_json::from_slice(&raw).map_err(|_| "Authorization recovery unavailable")?;
        if journal["account"].as_str() == Some(account) {
            if journal["stage"] != "pending" { return Err("Authorization outcome unknown".into()); }
            store.delete_auth_journal()?;
            *attempt = None;
        }
    }
    provider_auth::logout_self_hosted_with_store(&cfg, account, &store)
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeProviderCreateAccount<'a>(
    mut env: jni::EnvUnowned<'a>, _class: jni::objects::JClass<'a>,
    base: jni::objects::JString<'a>, request: jni::objects::JString<'a>,
) -> jni::sys::jint {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(base) = jni_get_string(&mut env, &base) else { return 0 };
        let Some(request) = jni_get_string(&mut env, &request) else { return 0 };
        i32::from(create_account(Path::new(&base), &request).is_ok())
    })).unwrap_or(0)
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
            match finish(Path::new(&base), &callback) {
                Ok(()) => 1,
                Err(provider_auth::AuthFinishError::RevocationPending) => 2,
                Err(provider_auth::AuthFinishError::RevocationUnrecoverable) => 3,
                Err(_) => 0,
            }
    }))
    .unwrap_or(0)
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeProviderQuarantineList<'a>(
    mut env: jni::EnvUnowned<'a>, _class: jni::objects::JClass<'a>,
    base: jni::objects::JString<'a>, account: jni::objects::JString<'a>,
) -> jni::sys::jstring {
    let value = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<serde_json::Value, String> {
        let base = jni_get_string(&mut env, &base).ok_or("Invalid request")?;
        let account = jni_get_string(&mut env, &account).ok_or("Invalid request")?;
        let cfg = config(Path::new(&base))?;
        let store = AndroidProviderCredentialStore::new(Path::new(&base))?;
        let ids = provider_auth::quarantined_grants_with_store(&cfg, &account, &store)?;
        Ok(serde_json::json!({"ok":true,"ids":ids}))
    })).ok().and_then(Result::ok).unwrap_or_else(|| serde_json::json!({"ok":false}));
    jni_new_string(&mut env, value.to_string())
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeProviderQuarantineRevoke<'a>(
    mut env: jni::EnvUnowned<'a>, _class: jni::objects::JClass<'a>,
    base: jni::objects::JString<'a>, account: jni::objects::JString<'a>, id: jni::objects::JString<'a>,
) -> jni::sys::jint {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(base) = jni_get_string(&mut env, &base) else { return 0 };
        let Some(account) = jni_get_string(&mut env, &account) else { return 0 };
        let Some(id) = jni_get_string(&mut env, &id) else { return 0 };
        let Ok(cfg) = config(Path::new(&base)) else { return 0 };
        let Ok(store) = AndroidProviderCredentialStore::new(Path::new(&base)) else { return 0 };
        i32::from(provider_auth::revoke_quarantined_grant_with_store(&cfg, &account, &id, &store).is_ok())
    })).unwrap_or(0)
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
