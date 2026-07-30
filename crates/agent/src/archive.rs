//! Read-only access to the user's M365 archive, abstracted behind [`ArchiveSource`].
//!
//! The retrieval executor is generic over this trait, so its logic (merge/dedup/limit/
//! source-tagging/byte-budget/truncation) is tested with an in-memory fake — no store,
//! no SQLCipher. The real [`StoreArchive`] (feature `retrieval`) binds it to
//! `isyncyou-store` + the on-disk body files, replicating the engine's archived-body
//! path logic (which is private there).

use crate::AgentError;
use std::fmt;
#[cfg(test)]
use std::path::PathBuf;
use std::path::{Component, Path};
use std::sync::Arc;

const SEARCH_SERVICES: [&str; 6] = [
    "mail", "calendar", "contacts", "todo", "onenote", "onedrive",
];

/// Private archive locator. It deliberately has no serialization implementation.
#[derive(Clone, PartialEq, Eq)]
pub struct ValidatedArchiveRelativePath(String);

impl ValidatedArchiveRelativePath {
    pub fn parse(value: impl Into<String>) -> Result<Self, AgentError> {
        let value = value.into();
        if value.is_empty() || value.len() > 4_096 || value.as_bytes().contains(&0) {
            return Err(AgentError::Provider("archive_body_locator_invalid".into()));
        }
        let mut components = 0usize;
        for component in Path::new(&value).components() {
            match component {
                Component::Normal(part) if !part.is_empty() => components += 1,
                _ => {
                    return Err(AgentError::Provider("archive_body_locator_invalid".into()));
                }
            }
        }
        if components == 0 || components > 64 {
            return Err(AgentError::Provider("archive_body_locator_invalid".into()));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ValidatedArchiveRelativePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ValidatedArchiveRelativePath([redacted])")
    }
}

/// Private result row used only between StoreArchive and the progressive executor.
///
/// `body_rel_path` cannot enter serde-based provider/public output because neither this
/// type nor the locator implements `Serialize`.
#[derive(Clone, PartialEq, Eq)]
pub struct ArchiveItemPrivateV1 {
    pub service: String,
    pub item_id: String,
    pub name: String,
    pub item_type: String,
    pub sender: Option<String>,
    pub remote_mtime: Option<String>,
    pub size: Option<u64>,
    pub body_rel_path: Option<ValidatedArchiveRelativePath>,
    pub display_path: Option<String>,
}

impl fmt::Debug for ArchiveItemPrivateV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArchiveItemPrivateV1")
            .field("service", &self.service)
            .field("item_id", &"[redacted]")
            .field("name", &"[redacted]")
            .field("item_type", &self.item_type)
            .field("sender", &self.sender.as_ref().map(|_| "[redacted]"))
            .field("remote_mtime", &self.remote_mtime)
            .field("size", &self.size)
            .field(
                "body_rel_path",
                &self.body_rel_path.as_ref().map(|_| "[redacted]"),
            )
            .field(
                "display_path",
                &self.display_path.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyFtsHit {
    pub item: ArchiveItemPrivateV1,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchPage<T> {
    pub items: Vec<T>,
    pub has_more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedSearchScope {
    account: String,
    services: Vec<String>,
}

impl NormalizedSearchScope {
    pub fn new(account: impl Into<String>, services: Vec<String>) -> Result<Self, AgentError> {
        let account = account.into();
        if account.is_empty() || account.len() > 128 {
            return Err(AgentError::ToolArgs("invalid account binding".into()));
        }
        let mut selected = if services.is_empty() {
            SEARCH_SERVICES.iter().map(ToString::to_string).collect()
        } else {
            let mut selected = Vec::with_capacity(services.len());
            for allowed in SEARCH_SERVICES {
                if services.iter().any(|service| service == allowed) {
                    selected.push(allowed.to_string());
                }
            }
            if selected.len()
                != services
                    .iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len()
                || services
                    .iter()
                    .any(|service| !SEARCH_SERVICES.contains(&service.as_str()))
            {
                return Err(AgentError::ToolArgs("invalid service scope".into()));
            }
            selected
        };
        selected.dedup();
        if selected.is_empty() {
            return Err(AgentError::ToolArgs("invalid service scope".into()));
        }
        Ok(Self {
            account,
            services: selected,
        })
    }

    pub fn account(&self) -> &str {
        &self.account
    }

    pub fn services(&self) -> &[String] {
        &self.services
    }
}

/// Injected cancellation/deadline predicate used by SQLite and body reads.
#[derive(Clone)]
pub struct StoreSearchDeadline {
    should_interrupt: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl StoreSearchDeadline {
    pub fn new(should_interrupt: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self {
            should_interrupt: Arc::new(should_interrupt),
        }
    }

    pub fn should_interrupt(&self) -> bool {
        (self.should_interrupt)()
    }
}

pub trait ArchiveSearchSnapshot {
    fn search_names_page(
        &self,
        query: &str,
        limit: u32,
        offset: u32,
    ) -> Result<SearchPage<ArchiveItemPrivateV1>, AgentError>;
    fn search_bodies_page(
        &self,
        query: &str,
        limit: u32,
        offset: u32,
    ) -> Result<SearchPage<BodyFtsHit>, AgentError>;
    fn metadata_page(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<SearchPage<ArchiveItemPrivateV1>, AgentError>;
}

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

    /// Open one account/service-bound read snapshot for progressive Search/DeepSearch.
    fn begin_search_snapshot(
        &self,
        _scope: &NormalizedSearchScope,
        _deadline: &StoreSearchDeadline,
    ) -> Result<Box<dyn ArchiveSearchSnapshot>, AgentError> {
        Err(AgentError::Provider(
            "progressive_search_unavailable".into(),
        ))
    }

    fn read_private_body(
        &self,
        _locator: &ValidatedArchiveRelativePath,
        _deadline: &StoreSearchDeadline,
    ) -> Result<Vec<u8>, AgentError> {
        Err(AgentError::Provider("archive_body_unavailable".into()))
    }
}

/// Join `rel` under `root`, rejecting any path that escapes `root` (no `..` past the
/// root, no absolute paths). Pure — does not touch the filesystem, so it is testable
/// without a real archive and guards the read path (REQ-AGENT — traversal-safety).
/// Used by the real `StoreArchive` (feature `retrieval`) and the unit tests.
#[cfg(test)]
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
    use super::{
        ArchiveItemPrivateV1, ArchiveSearchSnapshot, ArchiveSource, BodyFtsHit, ItemRef,
        NormalizedSearchScope, SearchPage, StoreSearchDeadline, ValidatedArchiveRelativePath,
    };
    use crate::AgentError;
    use isyncyou_store::{Item, Store};
    use std::path::PathBuf;

    fn literal_fts_query(query: &str) -> Option<String> {
        let terms = query
            .split_whitespace()
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect::<Vec<_>>();
        (!terms.is_empty()).then(|| terms.join(" OR "))
    }

    fn to_ref(it: Item) -> ItemRef {
        ItemRef {
            service: it.service,
            id: it.remote_id,
            name: it.name,
            item_type: it.item_type,
            path: it.local_path,
        }
    }

    fn to_private(it: Item) -> Result<ArchiveItemPrivateV1, AgentError> {
        if it.service.is_empty()
            || it.service.len() > 32
            || it.remote_id.is_empty()
            || it.remote_id.len() > 512
            || it.name.len() > 2_048
            || it.item_type.len() > 64
        {
            return Err(AgentError::Provider("archive_item_invalid".into()));
        }
        Ok(ArchiveItemPrivateV1 {
            service: it.service,
            item_id: it.remote_id,
            name: it.name,
            item_type: it.item_type,
            sender: it.sender,
            remote_mtime: it.remote_mtime,
            size: it.size.and_then(|size| u64::try_from(size).ok()),
            body_rel_path: it
                .local_path
                .map(ValidatedArchiveRelativePath::parse)
                .transpose()?,
            // The current store has no reviewed logical M365 path field.
            display_path: None,
        })
    }

    struct StoreArchiveSearchSnapshot {
        snapshot: isyncyou_store::ProgressiveSearchSnapshot,
    }

    impl ArchiveSearchSnapshot for StoreArchiveSearchSnapshot {
        fn search_names_page(
            &self,
            query: &str,
            limit: u32,
            offset: u32,
        ) -> Result<SearchPage<ArchiveItemPrivateV1>, AgentError> {
            let Some(query) = literal_fts_query(query) else {
                return Ok(SearchPage {
                    items: Vec::new(),
                    has_more: false,
                });
            };
            let page = self
                .snapshot
                .search_names_page(&query, limit, offset)
                .map_err(|_| AgentError::Provider("archive_query_failed".into()))?;
            Ok(SearchPage {
                items: page
                    .items
                    .into_iter()
                    .map(to_private)
                    .collect::<Result<_, _>>()?,
                has_more: page.has_more,
            })
        }

        fn search_bodies_page(
            &self,
            query: &str,
            limit: u32,
            offset: u32,
        ) -> Result<SearchPage<BodyFtsHit>, AgentError> {
            let Some(query) = literal_fts_query(query) else {
                return Ok(SearchPage {
                    items: Vec::new(),
                    has_more: false,
                });
            };
            let page = self
                .snapshot
                .search_bodies_page(&query, limit, offset)
                .map_err(|_| AgentError::Provider("archive_query_failed".into()))?;
            Ok(SearchPage {
                items: page
                    .items
                    .into_iter()
                    .map(|hit| {
                        Ok(BodyFtsHit {
                            item: to_private(hit.item)?,
                            snippet: hit.snippet,
                        })
                    })
                    .collect::<Result<_, AgentError>>()?,
                has_more: page.has_more,
            })
        }

        fn metadata_page(
            &self,
            limit: u32,
            offset: u32,
        ) -> Result<SearchPage<ArchiveItemPrivateV1>, AgentError> {
            let page = self
                .snapshot
                .metadata_page(limit, offset)
                .map_err(|_| AgentError::Provider("archive_query_failed".into()))?;
            Ok(SearchPage {
                items: page
                    .items
                    .into_iter()
                    .map(to_private)
                    .collect::<Result<_, _>>()?,
                has_more: page.has_more,
            })
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
    }

    impl ArchiveSource for StoreArchive {
        fn account(&self) -> &str {
            &self.account
        }

        fn search_names(&self, query: &str) -> Result<Vec<ItemRef>, AgentError> {
            let Some(query) = literal_fts_query(query) else {
                return Ok(Vec::new());
            };
            let store = self.open_readonly()?;
            Ok(store
                .search_names(&self.account, &query)
                .map_err(|_| AgentError::Provider("archive_query_failed".into()))?
                .into_iter()
                .map(to_ref)
                .collect())
        }

        fn search_bodies(&self, query: &str) -> Result<Vec<(String, String)>, AgentError> {
            let Some(query) = literal_fts_query(query) else {
                return Ok(Vec::new());
            };
            let store = self.open_readonly()?;
            store
                .search_bodies(&self.account, &query)
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
            let locator = ValidatedArchiveRelativePath::parse(rel)?;
            self.read_private_body(&locator, &StoreSearchDeadline::new(|| false))
        }

        fn read_private_body(
            &self,
            locator: &ValidatedArchiveRelativePath,
            deadline: &StoreSearchDeadline,
        ) -> Result<Vec<u8>, AgentError> {
            isyncyou_core::bounded_archive_body::read_bounded_archive_body(
                &self.archive_root,
                std::path::Path::new(locator.as_str()),
                &|| deadline.should_interrupt(),
            )
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

        fn begin_search_snapshot(
            &self,
            scope: &NormalizedSearchScope,
            deadline: &StoreSearchDeadline,
        ) -> Result<Box<dyn ArchiveSearchSnapshot>, AgentError> {
            if scope.account() != self.account {
                return Err(AgentError::Provider(
                    "archive_account_binding_mismatch".into(),
                ));
            }
            let deadline = deadline.clone();
            let snapshot = self
                .open_readonly()?
                .begin_progressive_search(
                    scope.account().to_string(),
                    scope.services().to_vec(),
                    move || deadline.should_interrupt(),
                )
                .map_err(|_| AgentError::Provider("archive_query_failed".into()))?;
            Ok(Box::new(StoreArchiveSearchSnapshot { snapshot }))
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
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            err.to_string().contains("archive_body_unavailable"),
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
    fn store_archive_treats_model_search_as_literal_fts_terms() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join(".isyncyou-store.db")).unwrap();
        store
            .upsert_item(&Item::new(
                "me",
                "mail",
                "m1",
                "Quarterly report",
                "message",
            ))
            .unwrap();
        drop(store);

        let archive = StoreArchive::new("me", dir.path());
        assert_eq!(archive.search_names("quarterly report").unwrap().len(), 1);
        assert!(archive.search_names("*").unwrap().is_empty());
        assert!(archive.search_names("\"").unwrap().is_empty());
        assert!(archive.search_names("   ").unwrap().is_empty());
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
        assert!(matches!(
            err,
            AgentError::Provider(code) if code == "archive_body_locator_invalid"
        ));
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
        assert!(err.to_string().contains("archive_body_unavailable"));
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
