//! Platform-owned credential persistence for one installation and account.

/// OAuth secrets are deliberately omitted from `Debug` and must never be logged.
#[derive(Clone)]
pub struct ProviderCredential {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: u64,
}

/// Implementations must use secure platform storage and atomically replace the
/// entire credential. Callers serialize the whole load/refresh/replace sequence.
pub trait ProviderCredentialStore: Send + Sync {
    fn load(&self, account: &str, origin: &str) -> Result<Option<ProviderCredential>, String>;
    fn replace(
        &self,
        account: &str,
        origin: &str,
        credential: &ProviderCredential,
    ) -> Result<(), String>;
    fn delete(&self, account: &str, origin: &str) -> Result<(), String>;
}

static PLATFORM_STORE: std::sync::OnceLock<std::sync::Arc<dyn ProviderCredentialStore>> =
    std::sync::OnceLock::new();

/// Install the platform credential owner once during native process bootstrap.
pub fn install_platform_store(
    store: std::sync::Arc<dyn ProviderCredentialStore>,
) -> Result<(), String> {
    PLATFORM_STORE
        .set(store)
        .map_err(|_| "Provider credential store already installed".into())
}

#[cfg(target_os = "android")]
pub(crate) fn platform_store() -> Result<&'static dyn ProviderCredentialStore, String> {
    PLATFORM_STORE
        .get()
        .map(|store| store.as_ref())
        .ok_or_else(|| "Platform credential store unavailable".into())
}
