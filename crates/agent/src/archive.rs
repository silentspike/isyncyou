//! Read-only access to the user's M365 archive, abstracted behind [`ArchiveSource`].
//!
//! The retrieval executor is generic over this trait, so its logic (merge/dedup/limit/
//! source-tagging/byte-budget/truncation) is tested with an in-memory fake — no store,
//! no SQLCipher. The real [`StoreArchive`] (feature `retrieval`) binds it to
//! `isyncyou-store` + the on-disk body files, replicating the engine's archived-body
//! path logic (which is private there).

use crate::AgentError;
use std::path::{Component, Path, PathBuf};

/// A source-tagged reference to one archived item. Agent-side and decoupled from
/// `isyncyou_store::Item`; `path` is the item's archived-body path, relative to the
/// account's `archive_root` (the source citation).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ItemRef {
    pub service: String,
    /// The item's `remote_id`.
    pub id: String,
    pub name: String,
    pub item_type: String,
    /// Relative archived-body path (`local_path`), if the body is archived.
    pub path: Option<String>,
}

/// Read-only retrieval over the archive. Account scope is fixed by the implementation.
pub trait ArchiveSource {
    /// Account id this archive is bound to.
    fn account(&self) -> &str;
    /// FTS over names/subjects/filenames, best-first.
    fn search_names(&self, query: &str) -> Result<Vec<ItemRef>, AgentError>;
    /// FTS over indexed bodies → `(service, remote_id)` pairs, best-first.
    fn search_bodies(&self, query: &str) -> Result<Vec<(String, String)>, AgentError>;
    /// Resolve one item.
    fn get(&self, service: &str, id: &str) -> Result<Option<ItemRef>, AgentError>;
    /// Read an item's archived body bytes (traversal-safe).
    fn read_body(&self, service: &str, id: &str) -> Result<Vec<u8>, AgentError>;
    /// One bounded flat page of a service's archived items.
    fn list_page(&self, service: &str, limit: u32, offset: u32)
        -> Result<Vec<ItemRef>, AgentError>;
    /// Top-level items of a service.
    fn roots(&self, service: &str) -> Result<Vec<ItemRef>, AgentError>;
    /// Direct children of a parent item.
    fn children(&self, service: &str, parent: &str) -> Result<Vec<ItemRef>, AgentError>;
    /// Count items in a service.
    fn count(&self, service: &str) -> Result<u64, AgentError>;
}

/// Join `rel` under `root`, rejecting any path that escapes `root` (no `..` past the
/// root, no absolute paths). Pure — does not touch the filesystem, so it is testable
/// without a real archive and guards the read path (REQ-AGENT — traversal-safety).
/// Used by the real `StoreArchive` (feature `retrieval`) and the unit tests.
#[cfg_attr(not(feature = "retrieval"), allow(dead_code))]
pub(crate) fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, AgentError> {
    let mut depth: i32 = 0;
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Err(AgentError::ToolArgs(format!(
                        "path traversal rejected: {rel}"
                    )));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(AgentError::ToolArgs(format!(
                    "absolute path rejected: {rel}"
                )));
            }
        }
    }
    Ok(root.join(rel))
}

#[cfg(feature = "retrieval")]
mod store_backed {
    use super::{ArchiveSource, ItemRef};
    use crate::AgentError;
    use isyncyou_store::{Item, Store};
    use std::path::PathBuf;

    fn to_ref(it: Item) -> ItemRef {
        ItemRef {
            service: it.service,
            id: it.remote_id,
            name: it.name,
            item_type: it.item_type,
            path: it.local_path,
        }
    }

    /// Real archive backed by `isyncyou-store` + the on-disk body files for one account.
    pub struct StoreArchive {
        account: String,
        archive_root: PathBuf,
    }

    impl StoreArchive {
        /// `archive_root` holds both `.isyncyou-store.db` and the relative body files.
        pub fn new(account: impl Into<String>, archive_root: impl Into<PathBuf>) -> Self {
            Self {
                account: account.into(),
                archive_root: archive_root.into(),
            }
        }

        fn open_readonly(&self) -> Result<Store, AgentError> {
            // Repo-specific WAL read-query handle: no .lock, no create/migration.
            // It is intentionally not a raw SQLite READ_ONLY connection.
            Store::open_readonly(self.archive_root.join(".isyncyou-store.db"))
                .map_err(|_| AgentError::Provider("archive_store_unavailable".into()))
        }

        fn body_path(&self, rel: &str) -> Result<PathBuf, AgentError> {
            let joined = super::safe_join(&self.archive_root, rel)?;
            let root = self
                .archive_root
                .canonicalize()
                .map_err(|_| AgentError::Provider("archive_body_unavailable".into()))?;
            let path = joined
                .canonicalize()
                .map_err(|_| AgentError::Provider("archive_body_unavailable".into()))?;
            if !path.starts_with(&root) {
                return Err(AgentError::ToolArgs(format!("path escape rejected: {rel}")));
            }
            Ok(path)
        }
    }

    impl ArchiveSource for StoreArchive {
        fn account(&self) -> &str {
            &self.account
        }

        fn search_names(&self, query: &str) -> Result<Vec<ItemRef>, AgentError> {
            let store = self.open_readonly()?;
            Ok(store
                .search_names(&self.account, query)
                .map_err(|_| AgentError::Provider("archive_query_failed".into()))?
                .into_iter()
                .map(to_ref)
                .collect())
        }

        fn search_bodies(&self, query: &str) -> Result<Vec<(String, String)>, AgentError> {
            let store = self.open_readonly()?;
            store
                .search_bodies(&self.account, query)
                .map_err(|_| AgentError::Provider("archive_query_failed".into()))
        }

        fn get(&self, service: &str, id: &str) -> Result<Option<ItemRef>, AgentError> {
            let store = self.open_readonly()?;
            Ok(store
                .get_item(&self.account, service, id)
                .map_err(|_| AgentError::Provider("archive_query_failed".into()))?
                .map(to_ref))
        }

        fn read_body(&self, service: &str, id: &str) -> Result<Vec<u8>, AgentError> {
            let item = self
                .get(service, id)?
                .ok_or_else(|| AgentError::ToolArgs(format!("no item {service}/{id}")))?;
            let rel = item.path.ok_or_else(|| {
                AgentError::ToolArgs(format!("{service}/{id} has no archived body"))
            })?;
            let path = self.body_path(&rel)?;
            isyncyou_core::envelope::read_body(&path)
                .map_err(|_| AgentError::Provider("archive_body_unavailable".into()))
        }

        fn list_page(
            &self,
            service: &str,
            limit: u32,
            offset: u32,
        ) -> Result<Vec<ItemRef>, AgentError> {
            let store = self.open_readonly()?;
            let items = store
                .items_by_service_page(&self.account, service, limit, offset)
                .map_err(|_| AgentError::Provider("archive_query_failed".into()))?;
            Ok(items.into_iter().map(to_ref).collect())
        }

        fn roots(&self, service: &str) -> Result<Vec<ItemRef>, AgentError> {
            let store = self.open_readonly()?;
            Ok(store
                .roots(&self.account, service)
                .map_err(|_| AgentError::Provider("archive_query_failed".into()))?
                .into_iter()
                .map(to_ref)
                .collect())
        }

        fn children(&self, service: &str, parent: &str) -> Result<Vec<ItemRef>, AgentError> {
            let store = self.open_readonly()?;
            Ok(store
                .children(&self.account, service, Some(parent))
                .map_err(|_| AgentError::Provider("archive_query_failed".into()))?
                .into_iter()
                .map(to_ref)
                .collect())
        }

        fn count(&self, service: &str) -> Result<u64, AgentError> {
            let store = self.open_readonly()?;
            store
                .count_by_service(&self.account, service)
                .map_err(|_| AgentError::Provider("archive_query_failed".into()))
        }
    }
}

#[cfg(feature = "retrieval")]
pub use store_backed::StoreArchive;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_allows_normal_relative_paths() {
        let root = Path::new("/archive");
        assert_eq!(
            safe_join(root, "mail/2024/m1.eml").unwrap(),
            Path::new("/archive/mail/2024/m1.eml")
        );
        assert_eq!(safe_join(root, "a/./b").unwrap(), Path::new("/archive/a/b"));
        // A descent then ascent that stays within root is fine.
        assert!(safe_join(root, "a/b/../c").is_ok());
    }

    #[test]
    fn safe_join_rejects_escapes_and_absolutes() {
        let root = Path::new("/archive");
        assert!(safe_join(root, "../etc/passwd").is_err());
        assert!(safe_join(root, "a/../../secret").is_err());
        assert!(safe_join(root, "/etc/passwd").is_err());
    }
}

#[cfg(all(test, feature = "retrieval"))]
pub(crate) struct BodyKeyTestGuard {
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(all(test, feature = "retrieval"))]
impl BodyKeyTestGuard {
    pub(crate) fn new() -> Self {
        static BODY_KEY_TEST_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
            std::sync::OnceLock::new();
        let guard = BODY_KEY_TEST_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap();
        isyncyou_core::envelope::reset_body_keys_for_tests();
        Self { _guard: guard }
    }
}

#[cfg(all(test, feature = "retrieval"))]
impl Drop for BodyKeyTestGuard {
    fn drop(&mut self) {
        isyncyou_core::envelope::reset_body_keys_for_tests();
    }
}

#[cfg(all(test, feature = "retrieval"))]
mod store_archive_tests {
    use super::*;
    use isyncyou_store::{Item, Store};

    fn upsert_body_item(root: &Path, service: &str, id: &str, rel: &str, body: &[u8]) {
        let store = Store::open(root.join(".isyncyou-store.db")).unwrap();
        let mut item = Item::new("me", service, id, format!("{id} name"), "message");
        item.local_path = Some(rel.into());
        store.upsert_item(&item).unwrap();
        drop(store);

        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        isyncyou_core::envelope::write_body_atomic(&path, body).unwrap();
    }

    #[test]
    fn store_archive_reads_sealed_body_with_envelope_reader() {
        let _guard = BodyKeyTestGuard::new();
        isyncyou_core::envelope::reset_body_keys_for_tests();
        isyncyou_core::envelope::set_body_key(618_001, [18u8; 32]);

        let dir = tempfile::tempdir().unwrap();
        upsert_body_item(dir.path(), "mail", "m1", "mail/aa/m1.eml", b"sealed text");
        let raw = std::fs::read(dir.path().join("mail/aa/m1.eml")).unwrap();
        assert_ne!(raw, b"sealed text");
        assert!(raw.starts_with(b"ISYE"));

        let archive = StoreArchive::new("me", dir.path());
        assert_eq!(archive.read_body("mail", "m1").unwrap(), b"sealed text");
        isyncyou_core::envelope::reset_body_keys_for_tests();
    }

    #[test]
    fn store_archive_fails_closed_when_sealed_body_key_is_missing() {
        let _guard = BodyKeyTestGuard::new();
        isyncyou_core::envelope::reset_body_keys_for_tests();
        isyncyou_core::envelope::set_body_key(618_002, [19u8; 32]);

        let dir = tempfile::tempdir().unwrap();
        upsert_body_item(dir.path(), "mail", "m1", "mail/aa/m1.eml", b"sealed text");
        isyncyou_core::envelope::reset_body_keys_for_tests();

        let archive = StoreArchive::new("me", dir.path());
        let err = archive.read_body("mail", "m1").unwrap_err();
        assert!(
            err.to_string().contains("no body key"),
            "missing key must fail closed, got {err}"
        );
    }

    #[test]
    fn store_archive_uses_readonly_handle_while_writer_holds_lock() {
        let dir = tempfile::tempdir().unwrap();
        let writer = Store::open(dir.path().join(".isyncyou-store.db")).unwrap();
        writer
            .upsert_item(&Item::new("me", "mail", "m1", "Mail item", "message"))
            .unwrap();

        let archive = StoreArchive::new("me", dir.path());
        let item = archive.get("mail", "m1").unwrap().unwrap();
        assert_eq!(item.name, "Mail item");
    }

    #[test]
    fn store_archive_read_does_not_create_missing_store() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".isyncyou-store.db");
        let archive = StoreArchive::new("me", dir.path());

        assert!(!db.exists());
        assert!(archive.search_names("anything").is_err());
        assert!(!db.exists(), "read-only archive open must not create a DB");
    }

    #[test]
    fn store_archive_rejects_traversal_local_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join(".isyncyou-store.db")).unwrap();
        let mut item = Item::new("me", "mail", "m1", "Bad path", "message");
        item.local_path = Some("../secret.eml".into());
        store.upsert_item(&item).unwrap();
        drop(store);

        let archive = StoreArchive::new("me", dir.path());
        let err = archive.read_body("mail", "m1").unwrap_err();
        assert!(err.to_string().contains("path traversal rejected"));
    }

    #[cfg(unix)]
    #[test]
    fn store_archive_rejects_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"outside").unwrap();

        let body_dir = dir.path().join("mail");
        std::fs::create_dir_all(&body_dir).unwrap();
        std::os::unix::fs::symlink(outside.path(), body_dir.join("link.eml")).unwrap();

        let store = Store::open(dir.path().join(".isyncyou-store.db")).unwrap();
        let mut item = Item::new("me", "mail", "m1", "Link path", "message");
        item.local_path = Some("mail/link.eml".into());
        store.upsert_item(&item).unwrap();
        drop(store);

        let archive = StoreArchive::new("me", dir.path());
        let err = archive.read_body("mail", "m1").unwrap_err();
        assert!(err.to_string().contains("path escape rejected"));
    }

    #[test]
    fn store_archive_split_list_methods_cover_page_roots_and_children() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join(".isyncyou-store.db")).unwrap();
        let folder = Item::new("me", "onedrive", "folder", "Folder", "folder");
        let mut child = Item::new("me", "onedrive", "child", "Child.txt", "file");
        child.parent_remote_id = Some("folder".into());
        store.upsert_item(&folder).unwrap();
        store.upsert_item(&child).unwrap();
        drop(store);

        let archive = StoreArchive::new("me", dir.path());
        assert_eq!(archive.list_page("onedrive", 10, 0).unwrap().len(), 2);
        assert_eq!(archive.roots("onedrive").unwrap()[0].id, "folder");
        assert_eq!(
            archive.children("onedrive", "folder").unwrap()[0].id,
            "child"
        );
    }
}
