//! `isyncyou-dbus-status` — the host-side DBus **FileStatus provider** (plan §13).
//!
//! On Linux/KDE the daemon publishes a session-bus service that answers, for any
//! filesystem path, the sync **overlay status** (synced / syncing / error /
//! ignored / unknown). Two consumers query it:
//!
//! * the Dolphin **`KOverlayIconPlugin`** (C++/KIO bridge in `packaging/dolphin/`)
//!   — to paint per-file overlay emblems, and
//! * the Dolphin **ServiceMenu** entries / the CLI — for the "fallback without
//!   overlays" path the plan mandates: status + actions work even where the
//!   overlay plugin is not installed.
//!
//! The [`OverlayStatus`] mapping and the [`StatusProvider`] trait are
//! cross-platform and unit-tested; the zbus service ([`serve`]/[`serve_blocking`])
//! is compiled on Linux only.
//!
//! Bus name `org.silentspike.iSyncYou`, object `/org/silentspike/iSyncYou/FileStatus`,
//! interface `org.silentspike.iSyncYou.FileStatus`:
//! `Status(path: s) -> status: s` and `Roots() -> as`.

use std::path::{Path, PathBuf};

/// The overlay emblem a path should show in Dolphin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayStatus {
    /// In sync with the cloud.
    Synced,
    /// A transfer/state change is in flight (dirty/staged/pending).
    Syncing,
    /// Conflict or fatal/retryable error — needs attention.
    Error,
    /// Tracked but intentionally not synced (trashed/ignored).
    Ignored,
    /// Not tracked by iSyncYou (outside a sync root, or unknown to the store).
    Unknown,
}

impl OverlayStatus {
    /// Map a store `sync_state` string onto an overlay emblem. The store uses the
    /// sync-state-automaton names (plan §5.1); unknown/transient names are treated
    /// as "syncing" rather than silently "synced", so a path is never shown as
    /// clean while work is pending.
    pub fn from_sync_state(state: &str) -> Self {
        match state.trim().to_ascii_lowercase().as_str() {
            "clean" | "synced" => OverlayStatus::Synced,
            "deleted" | "trashpending" | "deletepending" | "ignored" => OverlayStatus::Ignored,
            "conflict" | "errorfatal" | "errorretryable" | "error" => OverlayStatus::Error,
            "" => OverlayStatus::Unknown,
            // localdirty / remotedirty / bothdirty / uploadstaged / downloadstaged / …
            _ => OverlayStatus::Syncing,
        }
    }

    /// The wire string returned over DBus and consumed by the KIO plugin.
    pub fn as_str(self) -> &'static str {
        match self {
            OverlayStatus::Synced => "synced",
            OverlayStatus::Syncing => "syncing",
            OverlayStatus::Error => "error",
            OverlayStatus::Ignored => "ignored",
            OverlayStatus::Unknown => "unknown",
        }
    }

    /// Parse a wire string (the inverse of [`as_str`](Self::as_str)) back into a
    /// status; anything unrecognised is [`OverlayStatus::Unknown`].
    pub fn from_wire(s: &str) -> Self {
        match s {
            "synced" => OverlayStatus::Synced,
            "syncing" => OverlayStatus::Syncing,
            "error" => OverlayStatus::Error,
            "ignored" => OverlayStatus::Ignored,
            _ => OverlayStatus::Unknown,
        }
    }
}

/// Answers overlay-status queries by path. Implemented by [`StoreStatusProvider`]
/// for production; mockable in tests.
pub trait StatusProvider: Send + Sync + 'static {
    /// Overlay status for an absolute on-disk path.
    fn status(&self, path: &Path) -> OverlayStatus;
    /// The configured sync roots — lets a consumer cheaply skip paths that are not
    /// under any synced folder before issuing per-file queries.
    fn roots(&self) -> Vec<PathBuf>;
}

/// One account's mapping from its on-disk sync root to its SQLite store file.
#[derive(Debug, Clone)]
pub struct AccountRoot {
    /// The OneDrive folder shown in Dolphin (`AccountConfig.sync_root`).
    pub sync_root: PathBuf,
    /// The account's store DB (`archive_root/.isyncyou-store.db`).
    pub store_db: PathBuf,
}

/// Production [`StatusProvider`]: resolves a path to the owning account by sync-root
/// prefix, then reads that account's store. Opens the store read-only per query
/// (matching the daemon's per-request open policy, so it never holds the store's
/// single-instance lock across calls). Any open/query error degrades to
/// [`OverlayStatus::Unknown`] — overlays are advisory, never authoritative.
pub struct StoreStatusProvider {
    accounts: Vec<AccountRoot>,
}

impl StoreStatusProvider {
    pub fn new(accounts: Vec<AccountRoot>) -> Self {
        Self { accounts }
    }

    /// The account whose `sync_root` is an ancestor of `path` (longest match wins,
    /// so nested roots resolve to the most specific one).
    fn owning_account(&self, path: &Path) -> Option<&AccountRoot> {
        self.accounts
            .iter()
            .filter(|a| path.starts_with(&a.sync_root))
            .max_by_key(|a| a.sync_root.as_os_str().len())
    }
}

impl StatusProvider for StoreStatusProvider {
    fn status(&self, path: &Path) -> OverlayStatus {
        let Some(acct) = self.owning_account(path) else {
            return OverlayStatus::Unknown;
        };
        let Ok(store) = isyncyou_store::Store::open(&acct.store_db) else {
            return OverlayStatus::Unknown;
        };
        match store.sync_state_for_local_path(&path.to_string_lossy()) {
            Ok(Some(state)) => OverlayStatus::from_sync_state(&state),
            Ok(None) => OverlayStatus::Unknown,
            Err(_) => OverlayStatus::Unknown,
        }
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.accounts.iter().map(|a| a.sync_root.clone()).collect()
    }
}

#[cfg(target_os = "linux")]
mod dbus {
    use super::StatusProvider;
    use std::path::Path;
    use std::sync::Arc;
    use zbus::interface;

    /// Well-known bus name, object path and interface — shared with the KIO plugin.
    pub const BUS_NAME: &str = "org.silentspike.iSyncYou";
    pub const OBJECT_PATH: &str = "/org/silentspike/iSyncYou/FileStatus";

    struct FileStatus {
        provider: Arc<dyn StatusProvider>,
    }

    #[interface(name = "org.silentspike.iSyncYou.FileStatus")]
    impl FileStatus {
        /// Overlay status for a path: `synced` | `syncing` | `error` | `ignored` |
        /// `unknown`.
        async fn status(&self, path: String) -> String {
            self.provider.status(Path::new(&path)).as_str().to_string()
        }

        /// The configured sync roots (absolute paths).
        async fn roots(&self) -> Vec<String> {
            self.provider
                .roots()
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect()
        }
    }

    /// Publish the FileStatus service on the session bus and run until the process
    /// exits. Returns an error if there is no session bus or the name is taken.
    pub async fn serve(provider: Arc<dyn StatusProvider>) -> zbus::Result<()> {
        let _conn = zbus::connection::Builder::session()?
            .name(BUS_NAME)?
            .serve_at(OBJECT_PATH, FileStatus { provider })?
            .build()
            .await?;
        // Keep the connection (and the published name) alive forever.
        std::future::pending::<()>().await;
        Ok(())
    }

    /// Blocking wrapper: run [`serve`] on a dedicated current-thread Tokio runtime.
    /// Intended to be called from a `std::thread::spawn` in the daemon, so a missing
    /// session bus (headless server) just logs and exits the thread, non-fatally.
    pub fn serve_blocking(provider: Arc<dyn StatusProvider>) -> Result<(), String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        rt.block_on(async move { serve(provider).await.map_err(|e| e.to_string()) })
    }

    /// Client: ask the running provider for a single path's overlay status over the
    /// session bus. Used by `isyncyou dolphin-status` (the ServiceMenu action /
    /// "fallback without overlays"). Errors if the daemon is not running / there is
    /// no session bus.
    pub fn status_via_bus(path: &Path) -> Result<super::OverlayStatus, String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        rt.block_on(async {
            let conn = zbus::Connection::session()
                .await
                .map_err(|e| e.to_string())?;
            let proxy = zbus::Proxy::new(
                &conn,
                BUS_NAME,
                OBJECT_PATH,
                "org.silentspike.iSyncYou.FileStatus",
            )
            .await
            .map_err(|e| e.to_string())?;
            let wire: String = proxy
                .call("Status", &(path.to_string_lossy().as_ref(),))
                .await
                .map_err(|e| e.to_string())?;
            Ok(super::OverlayStatus::from_wire(&wire))
        })
    }
}

#[cfg(target_os = "linux")]
pub use dbus::{serve, serve_blocking, status_via_bus, BUS_NAME, OBJECT_PATH};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_store_states_to_overlays() {
        assert_eq!(
            OverlayStatus::from_sync_state("clean"),
            OverlayStatus::Synced
        );
        assert_eq!(
            OverlayStatus::from_sync_state("Synced"),
            OverlayStatus::Synced
        );
        assert_eq!(
            OverlayStatus::from_sync_state("localDirty"),
            OverlayStatus::Syncing
        );
        assert_eq!(
            OverlayStatus::from_sync_state("uploadStaged"),
            OverlayStatus::Syncing
        );
        assert_eq!(
            OverlayStatus::from_sync_state("conflict"),
            OverlayStatus::Error
        );
        assert_eq!(
            OverlayStatus::from_sync_state("errorFatal"),
            OverlayStatus::Error
        );
        assert_eq!(
            OverlayStatus::from_sync_state("deleted"),
            OverlayStatus::Ignored
        );
        assert_eq!(OverlayStatus::from_sync_state(""), OverlayStatus::Unknown);
    }

    #[test]
    fn overlay_wire_strings_are_stable() {
        for (s, w) in [
            (OverlayStatus::Synced, "synced"),
            (OverlayStatus::Syncing, "syncing"),
            (OverlayStatus::Error, "error"),
            (OverlayStatus::Ignored, "ignored"),
            (OverlayStatus::Unknown, "unknown"),
        ] {
            assert_eq!(s.as_str(), w);
            // The client side parses the same wire string back (round-trip).
            assert_eq!(OverlayStatus::from_wire(w), s);
        }
        // Anything unrecognised parses as Unknown.
        assert_eq!(OverlayStatus::from_wire("bogus"), OverlayStatus::Unknown);
    }

    #[test]
    fn store_provider_resolves_by_sync_root_and_reads_state() {
        // Two accounts; a path is resolved to the owning account by root prefix,
        // then its sync_state is read from that account's store and mapped.
        let dir = std::env::temp_dir().join(format!("isy-dbus-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("acct.db");
        let _ = std::fs::remove_file(&db);
        let store = isyncyou_store::Store::open(&db).unwrap();
        let mut item = isyncyou_store::Item::new("acct", "onedrive", "r1", "IMG.jpg", "file");
        let synced_path = dir.join("OneDrive").join("IMG.jpg");
        item.local_path = Some(synced_path.to_string_lossy().into_owned());
        item.sync_state = "clean".into();
        store.upsert_item(&item).unwrap();

        let mut dirty = isyncyou_store::Item::new("acct", "onedrive", "r2", "Draft.txt", "file");
        let dirty_path = dir.join("OneDrive").join("Draft.txt");
        dirty.local_path = Some(dirty_path.to_string_lossy().into_owned());
        dirty.sync_state = "localDirty".into();
        store.upsert_item(&dirty).unwrap();
        drop(store);

        let provider = StoreStatusProvider::new(vec![AccountRoot {
            sync_root: dir.join("OneDrive"),
            store_db: db.clone(),
        }]);

        assert_eq!(provider.status(&synced_path), OverlayStatus::Synced);
        assert_eq!(provider.status(&dirty_path), OverlayStatus::Syncing);
        // A path the store has never seen, but under a sync root → Unknown.
        assert_eq!(
            provider.status(&dir.join("OneDrive").join("nope.bin")),
            OverlayStatus::Unknown
        );
        // A path outside every sync root → Unknown (no owning account).
        assert_eq!(
            provider.status(Path::new("/etc/hosts")),
            OverlayStatus::Unknown
        );
        assert_eq!(provider.roots(), vec![dir.join("OneDrive")]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
