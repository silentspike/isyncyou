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
    open_inner(cfg,account,true)
}

/// Open an encrypted provider writer for local session state without running
/// sync-staging recovery on every immutable object operation.
#[cfg(feature = "encrypted-store")]
pub fn open_for_session(cfg: &Config, account: &str) -> Result<Store, String> {
    open_inner(cfg,account,false)
}

#[cfg(feature = "encrypted-store")]
fn open_inner(cfg: &Config, account: &str, recover_staging: bool) -> Result<Store, String> {
    #[cfg(debug_assertions)]
    let open_start = std::time::Instant::now();
    let path = location(cfg,account)?;
    let mut wal = path.as_os_str().to_os_string(); wal.push("-wal");
    let mut shm = path.as_os_str().to_os_string(); shm.push("-shm");
    let existing = path.exists() || std::path::Path::new(&wal).exists() || std::path::Path::new(&shm).exists();
    let key = crate::provider_auth::provider_database_key(cfg,account,existing)?;
    #[cfg(debug_assertions)]
    eprintln!("isyncyou: provider_store key_ms={}", open_start.elapsed().as_millis());
    #[cfg(debug_assertions)]
    let db_start = std::time::Instant::now();
    let store = Store::open_encrypted(path,&key).map_err(|e| e.to_string())?;
    #[cfg(debug_assertions)]
    eprintln!("isyncyou: provider_store db_open_ms={}", db_start.elapsed().as_millis());
    #[cfg(debug_assertions)]
    let recovery_start = std::time::Instant::now();
    if recover_staging {
        use fs2::FileExt;
        let guard = staging_lock(cfg, account)?;
        if guard.try_lock_exclusive().is_ok() {
            store.recover_abandoned_provider_staging().map_err(|e| e.to_string())?;
        }
    }
    #[cfg(debug_assertions)]
    eprintln!("isyncyou: provider_store recovery_ms={} total_ms={}", recovery_start.elapsed().as_millis(), open_start.elapsed().as_millis());
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

#[cfg(feature = "encrypted-store")]
fn staging_lock(cfg: &Config, account: &str) -> Result<std::fs::File, String> {
    let path = location(cfg, account)?.with_extension("staging.lock");
    std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).map_err(|_| "Provider staging lease unavailable".into())
}

/// Shared lease prevents ordinary opens from reclaiming an active generation.
#[cfg(feature = "encrypted-store")]
pub(crate) fn sync_staging_lease(cfg: &Config, account: &str) -> Result<std::fs::File, String> {
    use fs2::FileExt;
    let guard = staging_lock(cfg, account)?;
    guard.lock_shared().map_err(|_| "Provider staging lease unavailable")?;
    Ok(guard)
}

#[cfg(feature = "encrypted-store")]
pub(crate) fn open_sync(cfg: &Config, account: &str) -> Result<Store, String> {
    let path = location(cfg, account)?;
    let key = crate::provider_auth::provider_database_key(cfg, account, true)?;
    Store::open_provider_sync_encrypted(path, &key).map_err(|e| e.to_string())
}

#[cfg(not(feature = "encrypted-store"))]
pub(crate) fn sync_staging_lease(_cfg: &Config, _account: &str) -> Result<std::fs::File, String> { Err("Encrypted provider storage is unavailable".into()) }

#[cfg(not(feature = "encrypted-store"))]
pub(crate) fn open_sync(_cfg: &Config, _account: &str) -> Result<Store, String> { Err("Encrypted provider storage is unavailable".into()) }

/// Open an already-existing encrypted provider DB without blocking its writer.
#[cfg(feature = "encrypted-store")]
pub fn open_readonly(cfg: &Config, account: &str) -> Result<Store, String> {
    let path = location(cfg,account)?;
    if !path.exists() { return Err("Provider cache is not initialized".into()); }
    let key = crate::provider_auth::provider_database_key(cfg,account,true)?;
    Store::open_readonly_encrypted(path,&key).map_err(|e| e.to_string())
}

/// Identity used to invalidate an idle session reader on replacement, permissions or key changes.
#[cfg(all(feature = "encrypted-store", unix))]
pub fn readonly_cache_identity(cfg: &Config, account: &str) -> Result<(u64, u64, u32, u32, u32, i64, i64, [u8; 32]), String> {
    use std::os::unix::fs::MetadataExt;
    let path = location(cfg, account)?;
    let metadata = std::fs::symlink_metadata(&path).map_err(|_| "Provider cache unavailable")?;
    if !metadata.file_type().is_file() { return Err("Provider cache unavailable".into()); }
    let key = crate::provider_auth::provider_database_key(cfg, account, true)?;
    Ok((metadata.dev(), metadata.ino(), metadata.mode(), metadata.uid(), metadata.gid(), metadata.ctime(), metadata.ctime_nsec(), Sha256::digest(&key).into()))
}

#[cfg(not(all(feature = "encrypted-store", unix)))]
pub fn readonly_cache_identity(_cfg: &Config, _account: &str) -> Result<(u64, u64, u32, u32, u32, i64, i64, [u8; 32]), String> {
    Err("Encrypted provider storage is unavailable".into())
}

#[cfg(feature = "encrypted-store")]
pub(crate) fn has_cached_data(cfg: &Config, account: &str) -> Result<bool, String> {
    let path = location(cfg,account)?;
    if !path.exists() { return Ok(false); }
    open_readonly(cfg,account)?.provider_contains_data().map_err(|_| "Provider cache identity check failed".into())
}

#[cfg(not(feature = "encrypted-store"))]
pub(crate) fn has_cached_data(_cfg: &Config, _account: &str) -> Result<bool, String> {
    Err("Encrypted provider storage is unavailable".into())
}

#[cfg(not(feature = "encrypted-store"))]
pub fn open(_cfg: &Config, _account: &str) -> Result<Store, String> {
    Err("Encrypted provider storage is unavailable".into())
}

#[cfg(not(feature = "encrypted-store"))]
pub fn open_for_session(_cfg: &Config, _account: &str) -> Result<Store, String> {
    Err("Encrypted provider storage is unavailable".into())
}

#[cfg(not(feature = "encrypted-store"))]
pub fn open_readonly(_cfg: &Config, _account: &str) -> Result<Store, String> {
    Err("Encrypted provider storage is unavailable".into())
}
