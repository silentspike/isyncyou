//! Serialized, non-destructive setup for a self-hosted account.

use isyncyou_core::{config::ProviderConfig, AccountConfig, Config};
use std::path::{Path, PathBuf};

/// Create one account under an existing application data root and atomically
/// update the latest config. Existing paths and accounts are never overwritten.
pub fn create_self_hosted_account(
    config_path: &Path,
    account_id: &str,
    username: &str,
    origin: &str,
    client_id: &str,
    app_data_root: &Path,
) -> Result<Config, String> {
    if account_id.is_empty()
        || account_id.len() > 64
        || !account_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        || username.trim().is_empty()
        || username.len() > 320
        || username.chars().any(char::is_control)
        || client_id.trim().is_empty()
        || client_id.len() > 4096
        || client_id.chars().any(char::is_control)
        || origin.len() > 2048
    {
        return Err("Invalid account setup fields".into());
    }
    let parsed = reqwest::Url::parse(origin).map_err(|_| "Invalid HTTPS provider origin")?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err("Invalid HTTPS provider origin".into());
    }
    let canonical_origin = parsed.origin().ascii_serialization();
    let root = std::fs::canonicalize(app_data_root)
        .map_err(|_| "Application data directory unavailable")?;
    if !root.is_dir() {
        return Err("Application data directory unavailable".into());
    }
    let home = root.join(format!("provider-{account_id}"));
    let archive = home.join("archive");
    let cache = home.join("cache");
    let mut created = false;
    let result = Config::update_atomically(config_path, |cfg| {
        if cfg.accounts.iter().any(|a| a.id == account_id)
            || cfg.provider_accounts.contains_key(account_id)
        {
            return Err("Account already exists".into());
        }
        cfg.accounts.push(AccountConfig {
            id: account_id.into(),
            username: username.into(),
            sync_root: PathBuf::new(),
            archive_root: archive.clone(),
            cache_root: cache.clone(),
            mount_point: None,
        });
        cfg.provider_accounts.insert(
            account_id.into(),
            ProviderConfig::SelfHosted {
                origin: canonical_origin,
                client_id: client_id.into(),
            },
        );
        cfg.validate().map_err(|errors| errors.join("; "))?;
        std::fs::create_dir(&home)
            .map_err(|_| "Account data directory already exists or cannot be created")?;
        created = true;
        std::fs::create_dir(&archive).map_err(|_| "Account archive directory cannot be created")?;
        std::fs::create_dir(&cache).map_err(|_| "Account cache directory cannot be created")?;
        Ok(cfg.clone())
    });
    if result.is_err() && created {
        let _ = std::fs::remove_dir(&cache);
        let _ = std::fs::remove_dir(&archive);
        let _ = std::fs::remove_dir(&home);
    }
    result
}

/// Set or clear a same-origin push broker base using the shared config gate.
pub fn set_push_broker_base(
    config_path: &Path,
    account_id: &str,
    base: Option<&str>,
) -> Result<Config, String> {
    Config::update_atomically(config_path, |cfg| {
        if !cfg.provider_accounts.contains_key(account_id) {
            return Err("Not a self-hosted account".into());
        }
        match base {
            Some(value) => {
                cfg.push_broker_bases
                    .insert(account_id.into(), value.into());
            }
            None => {
                cfg.push_broker_bases.remove(account_id);
            }
        }
        cfg.validate().map_err(|errors| errors.join("; "))?;
        Ok(cfg.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn setup_creates_valid_account_and_preserves_existing_data() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let cfg = create_self_hosted_account(
            &config,
            "work",
            "user",
            "https://example.test",
            "client",
            dir.path(),
        )
        .unwrap();
        assert!(cfg.validate().is_ok());
        assert!(dir.path().join("provider-work/archive").is_dir());
        std::fs::write(dir.path().join("provider-work/archive/keep"), b"keep").unwrap();
        assert!(create_self_hosted_account(
            &config,
            "work",
            "user",
            "https://example.test",
            "client",
            dir.path()
        )
        .is_err());
        assert_eq!(
            std::fs::read(dir.path().join("provider-work/archive/keep")).unwrap(),
            b"keep"
        );
        assert!(create_self_hosted_account(
            &config,
            "other",
            "user",
            "http://example.test",
            "client",
            dir.path()
        )
        .is_err());
        assert!(!dir.path().join("provider-other").exists());
        assert!(set_push_broker_base(&config, "work", Some("https://other.test/push/")).is_err());
        assert!(Config::load(&config).unwrap().push_broker_bases.is_empty());
        set_push_broker_base(&config, "work", Some("https://example.test/push/")).unwrap();
        assert_eq!(
            Config::load(&config).unwrap().push_broker_bases["work"],
            "https://example.test/push/"
        );
        let cfg = create_self_hosted_account(
            &config,
            "second",
            "user",
            "https://EXAMPLE.TEST/",
            "client",
            dir.path(),
        )
        .unwrap();
        assert_eq!(
            cfg.provider_accounts["second"],
            ProviderConfig::SelfHosted {
                origin: "https://example.test".into(),
                client_id: "client".into()
            }
        );
    }
}
