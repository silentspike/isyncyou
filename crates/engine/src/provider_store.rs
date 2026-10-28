//! Separate encrypted store for self-hosted provider data.

use isyncyou_core::Config;
#[cfg(feature = "encrypted-store")]
use isyncyou_core::config::ProviderConfig;
use isyncyou_store::Store;
#[cfg(feature = "encrypted-store")]
use sha2::{Digest, Sha256};

/// Open the account/origin-bound provider store. Existing data is never replaced
/// when its platform-protected key is unavailable.
#[cfg(feature = "encrypted-store")]
pub fn open(cfg: &Config, account: &str) -> Result<Store, String> {
    let path = location(cfg,account)?;
    let mut wal = path.as_os_str().to_os_string(); wal.push("-wal");
    let mut shm = path.as_os_str().to_os_string(); shm.push("-shm");
    let existing = path.exists() || std::path::Path::new(&wal).exists() || std::path::Path::new(&shm).exists();
    let key = crate::provider_auth::provider_database_key(cfg,account,existing)?;
    let store = Store::open_encrypted(path,&key).map_err(|e| e.to_string())?;
    store.recover_abandoned_provider_staging().map_err(|e| e.to_string())?;
    Ok(store)
}

#[cfg(feature = "encrypted-store")]
fn location(cfg: &Config, account: &str) -> Result<std::path::PathBuf, String> {
    let origin = match cfg.provider_accounts.get(account) {
        Some(ProviderConfig::SelfHosted { origin, .. }) => origin,
        None => return Err("Not a self-hosted account".into()),
    };
    let acc = cfg.accounts.iter().find(|a| a.id == account).ok_or("Account missing")?;
    let mut digest = Sha256::new(); digest.update(account.as_bytes()); digest.update([0]); digest.update(origin.as_bytes());
    Ok(acc.archive_root.join(format!(".isyncyou-provider-{:x}.db",digest.finalize())))
}

/// Open an already-existing encrypted provider DB without blocking its writer.
#[cfg(feature = "encrypted-store")]
pub fn open_readonly(cfg: &Config, account: &str) -> Result<Store, String> {
    let path = location(cfg,account)?;
    if !path.exists() { return Err("Provider cache is not initialized".into()); }
    let key = crate::provider_auth::provider_database_key(cfg,account,true)?;
    Store::open_readonly_encrypted(path,&key).map_err(|e| e.to_string())
}

#[cfg(not(feature = "encrypted-store"))]
pub fn open(_cfg: &Config, _account: &str) -> Result<Store, String> {
    Err("Encrypted provider storage is unavailable".into())
}

#[cfg(not(feature = "encrypted-store"))]
pub fn open_readonly(_cfg: &Config, _account: &str) -> Result<Store, String> {
    Err("Encrypted provider storage is unavailable".into())
}
