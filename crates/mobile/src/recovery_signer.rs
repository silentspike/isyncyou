//! Android-only recovery signer. The callback is installed from Application,
//! and is reachable only through the engine's typed recovery signer factory.

use isyncyou_core::{config::ProviderConfig, Config};
use isyncyou_engine::self_hosted_mail_restore::{RecoveryDeviceSigner, RecoverySignerFactory};
use jni::{
    jni_sig, jni_str,
    objects::{JObject, JString},
    refs::Global,
    JValue, JavaVM,
};
use std::sync::Arc;

struct AndroidRecoveryFactory {
    vm: JavaVM,
    bridge: Arc<Global<JObject<'static>>>,
}

struct AndroidRecoverySigner {
    vm: JavaVM,
    bridge: Arc<Global<JObject<'static>>>,
    account: String,
    origin: String,
    installation: String,
}

impl RecoverySignerFactory for AndroidRecoveryFactory {
    fn load(&self, cfg: &Config, account: &str) -> Result<Box<dyn RecoveryDeviceSigner>, String> {
        let origin = match cfg.provider_accounts.get(account) {
            Some(ProviderConfig::SelfHosted { origin, .. }) => origin.clone(),
            _ => return Err("Recovery account unavailable".into()),
        };
        if !cfg.accounts.iter().any(|entry| entry.id == account) {
            return Err("Recovery account unavailable".into());
        }
        let installation = self
            .vm
            .attach_current_thread_for_scope(|env| -> jni::errors::Result<String> {
                let account = env.new_string(account)?;
                let origin = env.new_string(&origin)?;
                let result = env
                    .call_method(
                        &**self.bridge,
                        jni_str!("existingId"),
                        jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"),
                        &[JValue::Object(&account), JValue::Object(&origin)],
                    )?
                    .l()?;
                env.cast_local::<JString>(result)?.try_to_string(env)
            })
            .map_err(|_| "Recovery device unavailable")?;
        if installation.len() < 16
            || installation.len() > 128
            || !installation
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err("Recovery device is not registered".into());
        }
        Ok(Box::new(AndroidRecoverySigner {
            vm: self.vm.clone(),
            bridge: self.bridge.clone(),
            account: account.into(),
            origin,
            installation,
        }))
    }
}

impl RecoveryDeviceSigner for AndroidRecoverySigner {
    fn device_id(&self) -> &str {
        &self.installation
    }

    fn sign(&self, challenge: &[u8]) -> Result<String, String> {
        if challenge.is_empty() || challenge.len() > 8192 {
            return Err("Invalid recovery challenge".into());
        }
        let signature = self.vm.attach_current_thread_for_scope(|env| -> jni::errors::Result<String> {
            let account = env.new_string(&self.account)?;
            let origin = env.new_string(&self.origin)?;
            let installation = env.new_string(&self.installation)?;
            let bytes = env.byte_array_from_slice(challenge)?;
            let result = env.call_method(&**self.bridge, jni_str!("signExisting"),
                jni_sig!("(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;[B)Ljava/lang/String;"),
                &[JValue::Object(&account), JValue::Object(&origin), JValue::Object(&installation), JValue::Object(&bytes)])?.l()?;
            env.cast_local::<JString>(result)?.try_to_string(env)
        }).map_err(|_| "Recovery signing unavailable")?;
        if signature.is_empty()
            || signature.len() > 1024
            || !signature
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err("Recovery signing unavailable".into());
        }
        Ok(signature)
    }
}

#[no_mangle]
pub extern "system" fn Java_com_silentspike_isyncyou_NativeEngine_nativeInstallRecoverySigner<
    'a,
>(
    mut env: jni::EnvUnowned<'a>,
    _class: jni::objects::JClass<'a>,
    bridge: JObject<'a>,
) -> jni::sys::jint {
    match env
        .with_env(|env| -> jni::errors::Result<bool> {
            let vm = env.get_java_vm()?;
            let bridge = Arc::new(env.new_global_ref(&bridge)?);
            Ok(
                isyncyou_engine::self_hosted_mail_restore::install_recovery_signer_factory(
                    Arc::new(AndroidRecoveryFactory { vm, bridge }),
                )
                .is_ok(),
            )
        })
        .into_outcome()
    {
        jni::Outcome::Ok(true) => 1,
        _ => 0,
    }
}
