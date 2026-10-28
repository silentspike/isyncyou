//! Serialized, non-destructive setup for a self-hosted account.

use isyncyou_core::{config::ProviderConfig, AccountConfig, Config};
use std::path::{Path, PathBuf};

/// Create one account under an existing application data root and atomically
/// update the latest config. Existing paths and accounts are never overwritten.
pub fn create_self_hosted_account(config_path: &Path, account_id: &str, username: &str, origin: &str, client_id: &str, app_data_root: &Path) -> Result<Config, String> {
    if account_id.is_empty() || account_id.len() > 64 || !account_id.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c,b'-'|b'_')) || username.trim().is_empty() || client_id.trim().is_empty() {
        return Err("Invalid account setup fields".into());
    }
    let root = std::fs::canonicalize(app_data_root).map_err(|_| "Application data directory unavailable")?;
    if !root.is_dir() { return Err("Application data directory unavailable".into()); }
    let home = root.join(format!("provider-{account_id}"));
    let archive = home.join("archive");
    let cache = home.join("cache");
    let mut created = false;
    let result = Config::update_atomically(config_path, |cfg| {
        if cfg.accounts.iter().any(|a| a.id == account_id) || cfg.provider_accounts.contains_key(account_id) { return Err("Account already exists".into()); }
        cfg.accounts.push(AccountConfig { id:account_id.into(),username:username.into(),sync_root:PathBuf::new(),archive_root:archive.clone(),cache_root:cache.clone(),mount_point:None });
        cfg.provider_accounts.insert(account_id.into(),ProviderConfig::SelfHosted { origin:origin.into(),client_id:client_id.into() });
        cfg.validate().map_err(|errors| errors.join("; "))?;
        std::fs::create_dir(&home).map_err(|_| "Account data directory already exists or cannot be created")?;
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn setup_creates_valid_account_and_preserves_existing_data() {
        let dir = tempfile::tempdir().unwrap(); let config = dir.path().join("config.toml");
        let cfg = create_self_hosted_account(&config,"work","user","https://example.test","client",dir.path()).unwrap();
        assert!(cfg.validate().is_ok());
        assert!(dir.path().join("provider-work/archive").is_dir());
        std::fs::write(dir.path().join("provider-work/archive/keep"),b"keep").unwrap();
        assert!(create_self_hosted_account(&config,"work","user","https://example.test","client",dir.path()).is_err());
        assert_eq!(std::fs::read(dir.path().join("provider-work/archive/keep")).unwrap(),b"keep");
        assert!(create_self_hosted_account(&config,"other","user","http://example.test","client",dir.path()).is_err());
        assert!(!dir.path().join("provider-other").exists());
    }
}
