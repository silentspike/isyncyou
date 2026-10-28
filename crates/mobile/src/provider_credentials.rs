//! Android provider grants in the existing Keystore-wrapped credential envelope.
//! No local or environment-key fallback is allowed for this store.

use isyncyou_agent::{
    CredentialKeySource, CredentialStoreConfig, CredentialStoreResolver, Secret, SecretClass,
};
use isyncyou_engine::provider_credential::{ProviderCredential, ProviderCredentialStore};
use std::path::Path;

pub(crate) struct AndroidProviderCredentialStore {
    resolver: CredentialStoreResolver,
    #[cfg(test)]
    allow_test_key: bool,
}

impl AndroidProviderCredentialStore {
    pub(crate) fn new(base: &Path) -> Result<Self, String> {
        let resolver = CredentialStoreResolver::new(CredentialStoreConfig::with_paths(
            base.join("provider-credentials"),
            base.join("provider-credentials-never-local.key"),
        ));
        if resolver.key_source() != CredentialKeySource::ProcessInstalled {
            return Err("Android credential key unavailable".into());
        }
        Ok(Self {
            resolver,
            #[cfg(test)]
            allow_test_key: false,
        })
    }

    fn store(&self) -> Result<isyncyou_agent::AgentCredentialStore, String> {
        let source = self.resolver.key_source();
        #[cfg(test)]
        let permitted = source == CredentialKeySource::ProcessInstalled
            || (self.allow_test_key && source == CredentialKeySource::Provided);
        #[cfg(not(test))]
        let permitted = source == CredentialKeySource::ProcessInstalled;
        if !permitted {
            return Err("Android credential key unavailable".into());
        }
        self.resolver
            .resolve()
            .map_err(|_| "Android credential store unavailable".into())
    }

    fn id(account: &str, origin: &str) -> Result<String, String> {
        if account.is_empty()
            || account.len() > 256
            || origin.is_empty()
            || origin.len() > 2048
            || account.chars().any(char::is_control)
            || origin.chars().any(char::is_control)
        {
            return Err("Invalid credential scope".into());
        }
        Ok(format!(
            "self-hosted-v1:{}:{}:{}",
            account.len(),
            account,
            origin
        ))
    }
}

#[cfg(target_os = "android")]
pub(crate) fn install(base: &Path) -> Result<(), String> {
    static INSTALLED: std::sync::OnceLock<std::sync::Mutex<Option<std::path::PathBuf>>> =
        std::sync::OnceLock::new();
    let mut current = INSTALLED
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .map_err(|_| "Android credential store unavailable")?;
    if let Some(existing) = current.as_ref() {
        return if existing == base {
            Ok(())
        } else {
            Err("Android credential store scope changed".into())
        };
    }
    let store = std::sync::Arc::new(AndroidProviderCredentialStore::new(base)?);
    isyncyou_engine::provider_credential::install_platform_store(store)?;
    *current = Some(base.to_path_buf());
    Ok(())
}

impl ProviderCredentialStore for AndroidProviderCredentialStore {
    fn load(&self, account: &str, origin: &str) -> Result<Option<ProviderCredential>, String> {
        let id = Self::id(account, origin)?;
        let secret = self
            .store()?
            .get_bounded(SecretClass::ProviderOAuthRefresh, &id, 131072, 65536)
            .map_err(|_| "Android credential read failed")?;
        let Some(secret) = secret else {
            return Ok(None);
        };
        let value: serde_json::Value =
            serde_json::from_slice(secret.expose()).map_err(|_| "Invalid Android credential")?;
        if value["v"].as_u64() != Some(1) {
            return Err("Invalid Android credential".into());
        }
        let access = value["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("Invalid Android credential")?;
        let refresh = match &value["refresh_token"] {
            serde_json::Value::Null => None,
            serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
            _ => return Err("Invalid Android credential".into()),
        };
        let expires_at = value["expires_at"]
            .as_u64()
            .ok_or("Invalid Android credential")?;
        let subject = match &value["subject"] {
            serde_json::Value::Null => None,
            serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
            _ => return Err("Invalid Android credential".into()),
        };
        Ok(Some(ProviderCredential {
            access_token: access.into(),
            refresh_token: refresh,
            expires_at,
            subject,
        }))
    }

    fn replace(
        &self,
        account: &str,
        origin: &str,
        credential: &ProviderCredential,
    ) -> Result<(), String> {
        let id = Self::id(account, origin)?;
        if credential.access_token.is_empty() || credential.refresh_token.as_deref() == Some("") || credential.subject.as_deref() == Some("") {
            return Err("Invalid Android credential".into());
        }
        let value = serde_json::json!({"v":1,"access_token":credential.access_token,
            "refresh_token":credential.refresh_token,"expires_at":credential.expires_at,"subject":credential.subject});
        let bytes = serde_json::to_vec(&value).map_err(|_| "Android credential encoding failed")?;
        if bytes.len() > 65536 {
            return Err("Android credential too large".into());
        }
        self.store()?
            .put_bounded(
                SecretClass::ProviderOAuthRefresh,
                &id,
                &Secret::new(bytes),
                131072,
            )
            .map_err(|_| "Android credential write failed".into())
    }

    fn delete(&self, account: &str, origin: &str) -> Result<(), String> {
        let id = Self::id(account, origin)?;
        self.store()?
            .delete_durable(SecretClass::ProviderOAuthRefresh, &id)
            .map_err(|_| "Android credential deletion failed".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_and_origin_scopes_are_unambiguous() {
        assert_ne!(
            AndroidProviderCredentialStore::id("a", "bc").unwrap(),
            AndroidProviderCredentialStore::id("ab", "c").unwrap()
        );
        assert!(AndroidProviderCredentialStore::id("", "https://example.test").is_err());
        assert!(AndroidProviderCredentialStore::id("a\n", "https://example.test").is_err());
    }

    #[test]
    fn encrypted_credentials_survive_reopen_and_are_scoped_and_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let make = || AndroidProviderCredentialStore {
            resolver: CredentialStoreResolver::new(CredentialStoreConfig::new(dir.path()))
                .with_provided_key([23; 32]),
            allow_test_key: true,
        };
        let store = make();
        let credential = ProviderCredential {
            access_token: "access-sentinel".into(),
            refresh_token: Some("refresh-sentinel".into()),
            expires_at: 123,
            subject: Some("subject-sentinel".into()),
        };
        store
            .replace("one", "https://example.test", &credential)
            .unwrap();
        let reopened = make();
        assert_eq!(
            reopened
                .load("one", "https://example.test")
                .unwrap()
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("refresh-sentinel")
        );
        assert_eq!(reopened.load("one", "https://example.test").unwrap().unwrap().subject.as_deref(), Some("subject-sentinel"));
        assert!(reopened
            .load("other", "https://example.test")
            .unwrap()
            .is_none());
        let internal = ProviderCredential { access_token: "database-key".into(), refresh_token: None, expires_at: 0, subject: None };
        reopened.replace("isyncyou-internal:store:a", "https://example.test", &internal).unwrap();
        assert!(reopened.load("isyncyou-internal:store:a", "https://example.test").unwrap().unwrap().subject.is_none());
        let raw = std::fs::read_dir(dir.path().join("agent-credentials"))
            .unwrap()
            .map(|entry| std::fs::read(entry.unwrap().path()).unwrap())
            .collect::<Vec<_>>();
        assert!(raw.iter().all(|bytes| !bytes
            .windows(b"refresh-sentinel".len())
            .any(|w| w == b"refresh-sentinel")));
        assert!(raw.iter().all(|bytes| !bytes
            .windows(b"subject-sentinel".len())
            .any(|w| w == b"subject-sentinel")));
        reopened.delete("one", "https://example.test").unwrap();
        reopened.delete("isyncyou-internal:store:a", "https://example.test").unwrap();
        assert!(reopened
            .load("one", "https://example.test")
            .unwrap()
            .is_none());
    }
}
