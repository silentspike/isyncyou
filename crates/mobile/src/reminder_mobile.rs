//! Native-only reminder dispatch seam. No scheduler is enabled until account
//! coverage and the complete engine plan have been independently qualified.

use super::{jni_get_string, jni_new_string};
use crate::provider_auth_mobile::config;
use crate::provider_credentials::AndroidProviderCredentialStore;
use std::path::Path;

fn record(raw: &str) -> Result<(serde_json::Value, String), ()> {
    if raw.len() > 16384 {
        return Err(());
    }
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| ())?;
    let account = value
        .get("account")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .ok_or(())?
        .to_owned();
    Ok((value, account))
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeReminderPlan<'a>(
    mut env: jni::EnvUnowned<'a>,
    _class: jni::objects::JClass<'a>,
    base: jni::objects::JString<'a>,
    raw: jni::objects::JString<'a>,
) -> jni::sys::jstring {
    let output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<serde_json::Value, ()> {
        let base = jni_get_string(&mut env, &base).ok_or(())?;
        let raw = jni_get_string(&mut env, &raw).ok_or(())?;
        if raw.len() > 4096 { return Err(()); }
        let request: serde_json::Value = serde_json::from_str(&raw).map_err(|_| ())?;
        let text = |key| request.get(key).and_then(serde_json::Value::as_str).filter(|v| !v.is_empty() && v.len() <= 128).ok_or(());
        let account = text("account")?;
        let start = text("occurrence_start")?;
        let end = text("occurrence_end")?;
        let timezone = text("timezone")?;
        let window_start = request.get("window_start").and_then(serde_json::Value::as_i64).ok_or(())?;
        let window_end = request.get("window_end").and_then(serde_json::Value::as_i64).ok_or(())?;
        let cfg = config(Path::new(&base)).map_err(|_| ())?;
        let credentials = AndroidProviderCredentialStore::new(Path::new(&base)).map_err(|_| ())?;
        let plan = match isyncyou_engine::dav_live::plan_account_reminders_with_store(
            &cfg, account, "/.well-known/caldav", start, end, window_start, window_end,
            Some(timezone), &credentials,
        ) {
            Ok(records) => records,
            Err(error) if error == "unsupported_alarm_coverage" => return Ok(serde_json::json!({"ok":false,"reason":"unsupported_alarm_coverage"})),
            Err(error) if ["reminder_window", "reminder_changed", "reminder_config", "reminder_client", "reminder_discovery", "reminder_sync", "reminder_cache", "reminder_events_report", "reminder_tasks_report", "reminder_read", "reminder_access_check", "reminder_access_denied", "reminder_resource_get", "reminder_plan"].contains(&error.as_str()) => return Ok(serde_json::json!({"ok":false,"reason":error})),
            Err(_) => return Err(()),
        };
        Ok(serde_json::json!({"ok":true,"records":plan.records,"unsupported_count":plan.unsupported_count,"completed_collections":plan.completed_collections,"failed_collections":plan.failed_collections,"failure_reason":plan.failure_reason}))
    })).ok().and_then(Result::ok).unwrap_or_else(|| serde_json::json!({"ok":false}));
    jni_new_string(&mut env, output.to_string())
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeReminderClaim<'a>(
    mut env: jni::EnvUnowned<'a>,
    _class: jni::objects::JClass<'a>,
    base: jni::objects::JString<'a>,
    raw: jni::objects::JString<'a>,
) -> jni::sys::jstring {
    let output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
        || -> Result<serde_json::Value, ()> {
            let base = jni_get_string(&mut env, &base).ok_or(())?;
            let raw = jni_get_string(&mut env, &raw).ok_or(())?;
            let (record, account) = match record(&raw) {
                Ok(value) => value,
                Err(()) => {
                    return Ok(serde_json::json!({"status":"unavailable","reason":"record"}))
                }
            };
            let cfg = match config(Path::new(&base)) {
                Ok(value) => value,
                Err(_) => {
                    return Ok(serde_json::json!({"status":"unavailable","reason":"credentials"}))
                }
            };
            let credentials = match AndroidProviderCredentialStore::new(Path::new(&base)) {
                Ok(value) => value,
                Err(_) => {
                    return Ok(serde_json::json!({"status":"unavailable","reason":"credentials"}))
                }
            };
            Ok(
                match isyncyou_engine::dav_live::claim_reminder_record_diagnostic(
                    &cfg,
                    &account,
                    "/.well-known/caldav",
                    &record,
                    &credentials,
                ) {
                    Ok(value) => value,
                    Err(phase) => serde_json::json!({"status":"unavailable","reason":phase}),
                },
            )
        },
    ))
    .ok()
    .and_then(Result::ok)
    .unwrap_or_else(|| serde_json::json!({"status":"unavailable"}));
    jni_new_string(&mut env, output.to_string())
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeReminderAcknowledge<'a>(
    mut env: jni::EnvUnowned<'a>,
    _class: jni::objects::JClass<'a>,
    base: jni::objects::JString<'a>,
    raw: jni::objects::JString<'a>,
    tag: jni::objects::JString<'a>,
) -> jni::sys::jint {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(base) = jni_get_string(&mut env, &base) else {
            return 0;
        };
        let Some(raw) = jni_get_string(&mut env, &raw) else {
            return 0;
        };
        let Some(tag) = jni_get_string(&mut env, &tag) else {
            return 0;
        };
        let Ok((record, account)) = record(&raw) else {
            return 0;
        };
        let Ok(cfg) = config(Path::new(&base)) else {
            return 0;
        };
        i32::from(
            isyncyou_engine::dav_live::acknowledge_reminder_record(&cfg, &account, &record, &tag)
                .unwrap_or(false),
        )
    }))
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::record;
    #[test]
    fn reminder_record_is_bounded_and_requires_account() {
        assert!(record("{}").is_err());
        assert!(record(&format!("{{\"account\":\"{}\"}}", "x".repeat(16385))).is_err());
        assert_eq!(record("{\"account\":\"a\"}").unwrap().1, "a");
    }
}
