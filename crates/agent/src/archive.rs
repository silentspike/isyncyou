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
    /// FTS over names/subjects/filenames, best-first.
    fn search_names(&self, query: &str) -> Result<Vec<ItemRef>, AgentError>;
    /// FTS over indexed bodies → `(service, remote_id)` pairs, best-first.
    fn search_bodies(&self, query: &str) -> Result<Vec<(String, String)>, AgentError>;
    /// Resolve one item.
    fn get(&self, service: &str, id: &str) -> Result<Option<ItemRef>, AgentError>;
    /// Read an item's archived body bytes (traversal-safe).
    fn read_body(&self, service: &str, id: &str) -> Result<Vec<u8>, AgentError>;
    /// List items under a parent (`None` = service root).
    fn list(&self, service: &str, parent: Option<&str>) -> Result<Vec<ItemRef>, AgentError>;
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

        fn open(&self) -> Result<Store, AgentError> {
            // Store::open auto-applies SQLCipher when a store key is configured.
            Store::open(self.archive_root.join(".isyncyou-store.db"))
                .map_err(|e| AgentError::Provider(format!("store open: {e}")))
        }
    }

    impl ArchiveSource for StoreArchive {
        fn search_names(&self, query: &str) -> Result<Vec<ItemRef>, AgentError> {
            let store = self.open()?;
            Ok(store
                .search_names(&self.account, query)
                .map_err(|e| AgentError::Provider(e.to_string()))?
                .into_iter()
                .map(to_ref)
                .collect())
        }

        fn search_bodies(&self, query: &str) -> Result<Vec<(String, String)>, AgentError> {
            let store = self.open()?;
            store
                .search_bodies(&self.account, query)
                .map_err(|e| AgentError::Provider(e.to_string()))
        }

        fn get(&self, service: &str, id: &str) -> Result<Option<ItemRef>, AgentError> {
            let store = self.open()?;
            Ok(store
                .get_item(&self.account, service, id)
                .map_err(|e| AgentError::Provider(e.to_string()))?
                .map(to_ref))
        }

        fn read_body(&self, service: &str, id: &str) -> Result<Vec<u8>, AgentError> {
            let item = self
                .get(service, id)?
                .ok_or_else(|| AgentError::ToolArgs(format!("no item {service}/{id}")))?;
            let rel = item.path.ok_or_else(|| {
                AgentError::ToolArgs(format!("{service}/{id} has no archived body"))
            })?;
            let path = super::safe_join(&self.archive_root, &rel)?;
            std::fs::read(&path).map_err(|e| AgentError::Provider(format!("read body: {e}")))
        }

        fn list(&self, service: &str, parent: Option<&str>) -> Result<Vec<ItemRef>, AgentError> {
            let store = self.open()?;
            let items = match parent {
                Some(_) => store.children(&self.account, service, parent),
                None => store.items_by_service(&self.account, service),
            }
            .map_err(|e| AgentError::Provider(e.to_string()))?;
            Ok(items.into_iter().map(to_ref).collect())
        }

        fn count(&self, service: &str) -> Result<u64, AgentError> {
            let store = self.open()?;
            store
                .count_by_service(&self.account, service)
                .map_err(|e| AgentError::Provider(e.to_string()))
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
