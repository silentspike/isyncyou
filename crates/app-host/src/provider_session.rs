//! Account-bound session transport using the encrypted provider store.

use isyncyou_agent::session_v2::TrustedServerTimeSample;
use isyncyou_agent::{
    OneDriveSessionV2Transport, SessionManifestV1, SessionObjectClass, SessionObjectCrypto,
    SessionV2Error, SessionV2Transport, VersionedManifest,
};
use isyncyou_core::{config::ProviderConfig, Config};
use isyncyou_store::ProviderScope;
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
pub(crate) enum ProductSessionTransport {
    OneDrive(OneDriveSessionV2Transport),
    Local(LocalSessionTransport),
}

#[derive(Clone)]
pub(crate) struct LocalSessionTransport {
    cfg: Config,
    scope: ProviderScope,
    session_id: String,
    crypto: Arc<Mutex<Option<SessionObjectCrypto>>>,
    reader: Arc<
        Mutex<
            Option<(
                (u64, u64, u32, u32, u32, i64, i64, [u8; 32]),
                isyncyou_store::Store,
            )>,
        >,
    >,
    projection_writer: Arc<
        Mutex<
            Option<(
                (u64, u64, u32, u32, u32, i64, i64, [u8; 32]),
                isyncyou_store::Store,
            )>,
        >,
    >,
    projection_gate: Arc<Mutex<()>>,
    _exclusive_session_lock: Arc<isyncyou_agent::FileLock>,
    #[cfg(all(test, feature = "encrypted-store"))]
    test_store: Option<(std::path::PathBuf, Vec<u8>)>,
    #[cfg(all(test, feature = "encrypted-store"))]
    test_read_opens: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(all(test, feature = "encrypted-store"))]
    test_write_opens: Arc<std::sync::atomic::AtomicUsize>,
}

impl ProductSessionTransport {
    pub(crate) fn new(
        cfg: Option<&Config>,
        account: &str,
        token: &str,
        session_id: &str,
    ) -> Result<Self, SessionV2Error> {
        isyncyou_agent::SessionId::new(session_id).map_err(|_| SessionV2Error::InvalidRecord)?;
        if let Some(cfg) = cfg {
            if !cfg.accounts.iter().any(|a| a.id == account) {
                return Err(SessionV2Error::InvalidRecord);
            }
            if let Some(ProviderConfig::SelfHosted { origin, .. }) =
                cfg.provider_accounts.get(account)
            {
                let root = &cfg
                    .accounts
                    .iter()
                    .find(|a| a.id == account)
                    .ok_or(SessionV2Error::InvalidRecord)?
                    .archive_root;
                let lock_path = root
                    .join(".isyncyou-agent")
                    .join(format!("session-{session_id}.lock"));
                let lock = isyncyou_agent::FileLock::try_acquire_exclusive(&lock_path)
                    .map_err(|_| SessionV2Error::TransportUnavailable)?
                    .ok_or(SessionV2Error::LeaseLost)?;
                return Ok(Self::Local(LocalSessionTransport {
                    cfg: cfg.clone(),
                    scope: ProviderScope {
                        account_id: account.into(),
                        origin: origin.clone(),
                        service: "agent-session".into(),
                        collection: "local".into(),
                    },
                    session_id: session_id.into(),
                    crypto: Arc::new(Mutex::new(None)),
                    reader: Arc::new(Mutex::new(None)),
                    projection_writer: Arc::new(Mutex::new(None)),
                    projection_gate: Arc::new(Mutex::new(())),
                    _exclusive_session_lock: Arc::new(lock),
                    #[cfg(all(test, feature = "encrypted-store"))]
                    test_store: None,
                    #[cfg(all(test, feature = "encrypted-store"))]
                    test_read_opens: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                    #[cfg(all(test, feature = "encrypted-store"))]
                    test_write_opens: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                }));
            }
        }
        Ok(Self::OneDrive(OneDriveSessionV2Transport::new(
            token, session_id,
        )?))
    }
    pub(crate) fn bind_manifest_crypto(
        &self,
        crypto: SessionObjectCrypto,
    ) -> Result<(), SessionV2Error> {
        match self {
            Self::OneDrive(t) => t.bind_manifest_crypto(crypto),
            Self::Local(t) => {
                *t.crypto
                    .lock()
                    .map_err(|_| SessionV2Error::TransportUnavailable)? = Some(crypto);
                Ok(())
            }
        }
    }
    /// Reuse a writer only during one synchronous local projection. Other
    /// transports retain their existing behavior and there is no idle cache.
    pub(crate) fn with_projection_writer<T>(
        &self,
        run: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        #[cfg(not(unix))]
        {
            return run();
        }
        #[cfg(unix)]
        match self {
            Self::OneDrive(_) => run(),
            Self::Local(local) => local.with_projection_writer(run),
        }
    }
    pub(crate) fn create_session(&self) -> Result<VersionedManifest, SessionV2Error> {
        match self {
            Self::OneDrive(t) => t.create_session(),
            Self::Local(t) => {
                let empty = SessionManifestV1::empty(&t.session_id);
                let sealed = t.seal(&empty)?;
                let store = t.store()?;
                if store
                    .local_session_create(&t.scope, &t.session_id, &sealed)
                    .is_err()
                {
                    drop(store);
                    return t.load_manifest(&t.session_id);
                }
                drop(store);
                t.load_manifest(&t.session_id)
            }
        }
    }
}

impl LocalSessionTransport {
    fn with_projection_writer<T>(
        &self,
        run: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let _gate = self
            .projection_gate
            .lock()
            .map_err(|_| "session_store_unavailable")?;
        let before = self
            .reader_identity()
            .map_err(|_| "session_store_unavailable")?;
        let store = self.store().map_err(|_| "session_store_unavailable")?;
        let opened = self
            .reader_identity()
            .map_err(|_| "session_store_unavailable")?;
        if (opened.0, opened.1, opened.2, opened.3, opened.4, opened.7)
            != (before.0, before.1, before.2, before.3, before.4, before.7)
        {
            return Err("session_store_unavailable".into());
        }
        *self
            .projection_writer
            .lock()
            .map_err(|_| "session_store_unavailable")? = Some((opened, store));
        struct Cleanup<'a> {
            writer: &'a Mutex<
                Option<(
                    (u64, u64, u32, u32, u32, i64, i64, [u8; 32]),
                    isyncyou_store::Store,
                )>,
            >,
            reader: &'a Mutex<
                Option<(
                    (u64, u64, u32, u32, u32, i64, i64, [u8; 32]),
                    isyncyou_store::Store,
                )>,
            >,
        }
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                *self.writer.lock().unwrap_or_else(|e| e.into_inner()) = None;
                *self.reader.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
        }
        let _cleanup = Cleanup {
            writer: &self.projection_writer,
            reader: &self.reader,
        };
        run()
    }
    fn with_writer<T>(
        &self,
        f: impl FnOnce(&isyncyou_store::Store) -> Result<T, SessionV2Error>,
    ) -> Result<T, SessionV2Error> {
        let mut writer = self
            .projection_writer
            .lock()
            .map_err(|_| SessionV2Error::TransportUnavailable)?;
        if let Some((expected, store)) = writer.as_mut() {
            if &self.reader_identity()? != expected {
                return Err(SessionV2Error::TransportUnavailable);
            }
            let value = f(store)?;
            let after = self.reader_identity()?;
            if (after.0, after.1, after.2, after.3, after.4, after.7)
                != (
                    expected.0, expected.1, expected.2, expected.3, expected.4, expected.7,
                )
            {
                return Err(SessionV2Error::TransportUnavailable);
            }
            *expected = after;
            return Ok(value);
        }
        drop(writer);
        f(&self.store()?)
    }
    fn store(&self) -> Result<isyncyou_store::Store, SessionV2Error> {
        #[cfg(all(test, feature = "encrypted-store"))]
        if let Some((path, key)) = &self.test_store {
            self.test_write_opens
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return isyncyou_store::Store::open_encrypted(path, key)
                .map_err(|_| SessionV2Error::TransportUnavailable);
        }
        isyncyou_engine::provider_store::open_for_session(&self.cfg, &self.scope.account_id)
            .map_err(|_| SessionV2Error::TransportUnavailable)
    }
    fn read_store(&self) -> Result<isyncyou_store::Store, SessionV2Error> {
        #[cfg(all(test, feature = "encrypted-store"))]
        if let Some((path, key)) = &self.test_store {
            self.test_read_opens
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return isyncyou_store::Store::open_readonly_encrypted(path, key)
                .map_err(|_| SessionV2Error::TransportUnavailable);
        }
        isyncyou_engine::provider_store::open_readonly(&self.cfg, &self.scope.account_id)
            .map_err(|_| SessionV2Error::TransportUnavailable)
    }
    fn reader_identity(
        &self,
    ) -> Result<(u64, u64, u32, u32, u32, i64, i64, [u8; 32]), SessionV2Error> {
        #[cfg(all(test, feature = "encrypted-store", unix))]
        if let Some((path, _)) = &self.test_store {
            use std::os::unix::fs::MetadataExt;
            let m = std::fs::symlink_metadata(path)
                .map_err(|_| SessionV2Error::TransportUnavailable)?;
            if !m.file_type().is_file() {
                return Err(SessionV2Error::TransportUnavailable);
            }
            return Ok((
                m.dev(),
                m.ino(),
                m.mode(),
                m.uid(),
                m.gid(),
                m.ctime(),
                m.ctime_nsec(),
                [0; 32],
            ));
        }
        isyncyou_engine::provider_store::readonly_cache_identity(&self.cfg, &self.scope.account_id)
            .map_err(|_| SessionV2Error::TransportUnavailable)
    }
    fn with_reader<T>(
        &self,
        f: impl FnOnce(&isyncyou_store::Store) -> Result<T, SessionV2Error>,
    ) -> Result<T, SessionV2Error> {
        #[cfg(not(unix))]
        {
            return f(&self.read_store()?);
        }
        #[cfg(unix)]
        {
            let mut reader = self
                .reader
                .lock()
                .map_err(|_| SessionV2Error::TransportUnavailable)?;
            let identity = match self.reader_identity() {
                Ok(identity) => identity,
                Err(error) => {
                    *reader = None;
                    return Err(error);
                }
            };
            if reader.as_ref().is_none_or(|(old, _)| *old != identity) {
                *reader = None;
                let store = self.read_store()?;
                if self.reader_identity()? != identity {
                    return Err(SessionV2Error::TransportUnavailable);
                }
                *reader = Some((identity, store));
            }
            let result = f(&reader
                .as_ref()
                .ok_or(SessionV2Error::TransportUnavailable)?
                .1)?;
            if self.reader_identity()? != identity {
                *reader = None;
                return Err(SessionV2Error::TransportUnavailable);
            }
            Ok(result)
        }
    }
    fn seal(&self, manifest: &SessionManifestV1) -> Result<Vec<u8>, SessionV2Error> {
        manifest.validate()?;
        let bytes = serde_json::to_vec(manifest).map_err(|_| SessionV2Error::InvalidRecord)?;
        self.crypto
            .lock()
            .map_err(|_| SessionV2Error::TransportUnavailable)?
            .as_ref()
            .ok_or(SessionV2Error::InvalidRecord)?
            .seal(
                &self.session_id,
                SessionObjectClass::Manifest,
                "manifest",
                &bytes,
            )
            .map_err(|_| SessionV2Error::InvalidRecord)
    }
    fn open(&self, sealed: &[u8]) -> Result<SessionManifestV1, SessionV2Error> {
        let bytes = self
            .crypto
            .lock()
            .map_err(|_| SessionV2Error::TransportUnavailable)?
            .as_ref()
            .ok_or(SessionV2Error::InvalidRecord)?
            .open(
                &self.session_id,
                SessionObjectClass::Manifest,
                "manifest",
                sealed,
            )
            .map_err(|_| SessionV2Error::InvalidRecord)?;
        let manifest: SessionManifestV1 =
            serde_json::from_slice(&bytes).map_err(|_| SessionV2Error::InvalidRecord)?;
        manifest.validate()?;
        if manifest.session_id != self.session_id {
            return Err(SessionV2Error::InvalidRecord);
        }
        Ok(manifest)
    }
}

fn class_name(class: SessionObjectClass) -> &'static str {
    match class {
        SessionObjectClass::Manifest => "manifest",
        SessionObjectClass::VisibleRecord => "visible_record",
        SessionObjectClass::VisibleIndex => "visible_index",
        SessionObjectClass::RequestState => "request_state",
        SessionObjectClass::RequestIndex => "request_index",
        SessionObjectClass::UuidBinding => "uuid_binding",
        SessionObjectClass::UuidBindingIndex => "uuid_binding_index",
    }
}

impl SessionV2Transport for LocalSessionTransport {
    fn server_time_sample(&self) -> Result<TrustedServerTimeSample, SessionV2Error> {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| SessionV2Error::TransportUnavailable)?
            .as_millis();
        Ok(TrustedServerTimeSample {
            server_unix_ms: u64::try_from(ms).map_err(|_| SessionV2Error::TransportUnavailable)?,
            received_at_monotonic: Instant::now(),
        })
    }
    fn load_manifest(&self, session_id: &str) -> Result<VersionedManifest, SessionV2Error> {
        if session_id != self.session_id {
            return Err(SessionV2Error::InvalidRecord);
        }
        let (etag, bytes) = self.with_reader(|store| {
            store
                .local_session_manifest(&self.scope, session_id)
                .map_err(|_| SessionV2Error::TransportUnavailable)?
                .ok_or(SessionV2Error::ManifestConflict)
        })?;
        Ok(VersionedManifest {
            etag,
            storage_id: None,
            manifest: self.open(&bytes)?,
        })
    }
    fn stage_immutable(
        &self,
        class: SessionObjectClass,
        id: &str,
        bytes: &[u8],
    ) -> Result<(), SessionV2Error> {
        if class == SessionObjectClass::Manifest || bytes.len() > 5 * 1024 * 1024 {
            return Err(SessionV2Error::SessionLimit);
        }
        self.with_writer(|store| {
            store
                .local_session_stage(&self.scope, &self.session_id, class_name(class), id, bytes)
                .map_err(|_| SessionV2Error::ManifestConflict)
        })
    }
    fn load_immutable(
        &self,
        class: SessionObjectClass,
        id: &str,
    ) -> Result<Vec<u8>, SessionV2Error> {
        self.load_immutable_optional(class, id)?
            .ok_or(SessionV2Error::InvalidJournal)
    }
    fn load_immutable_batch(
        &self,
        class: SessionObjectClass,
        ids: &[String],
    ) -> Result<Vec<Vec<u8>>, SessionV2Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.with_reader(|store| {
            ids.iter()
                .map(|id| {
                    store
                        .local_session_load(&self.scope, &self.session_id, class_name(class), id)
                        .map_err(|_| SessionV2Error::TransportUnavailable)?
                        .ok_or(SessionV2Error::InvalidJournal)
                })
                .collect()
        })
    }
    fn load_immutable_optional(
        &self,
        class: SessionObjectClass,
        id: &str,
    ) -> Result<Option<Vec<u8>>, SessionV2Error> {
        self.with_reader(|store| {
            store
                .local_session_load(&self.scope, &self.session_id, class_name(class), id)
                .map_err(|_| SessionV2Error::TransportUnavailable)
        })
    }
    fn reap_unreachable(
        &self,
        _reachable: &BTreeSet<(SessionObjectClass, String)>,
        _older: u64,
        _limit: usize,
        _deadline: Instant,
    ) -> Result<usize, SessionV2Error> {
        Ok(0)
    }
    fn compare_and_swap_manifest(
        &self,
        current: &VersionedManifest,
        next: &SessionManifestV1,
    ) -> Result<Option<VersionedManifest>, SessionV2Error> {
        if next.session_id != self.session_id
            || current.manifest.session_id != self.session_id
            || next.generation
                != current
                    .manifest
                    .generation
                    .checked_add(1)
                    .ok_or(SessionV2Error::SessionLimit)?
        {
            return Err(SessionV2Error::InvalidRecord);
        }
        let sealed = self.seal(next)?;
        let etag = self.with_writer(|store| {
            store
                .local_session_cas(&self.scope, &self.session_id, &current.etag, &sealed)
                .map_err(|_| SessionV2Error::TransportUnavailable)
        })?;
        Ok(etag.map(|etag| VersionedManifest {
            etag,
            storage_id: None,
            manifest: next.clone(),
        }))
    }
}

impl SessionV2Transport for ProductSessionTransport {
    fn complete_interactive_admission(&self) -> Result<(), SessionV2Error> {
        match self {
            Self::OneDrive(t) => t.complete_interactive_admission(),
            Self::Local(t) => t.complete_interactive_admission(),
        }
    }
    fn server_time_sample(&self) -> Result<TrustedServerTimeSample, SessionV2Error> {
        match self {
            Self::OneDrive(t) => t.server_time_sample(),
            Self::Local(t) => t.server_time_sample(),
        }
    }
    fn load_manifest(&self, id: &str) -> Result<VersionedManifest, SessionV2Error> {
        match self {
            Self::OneDrive(t) => t.load_manifest(id),
            Self::Local(t) => t.load_manifest(id),
        }
    }
    fn load_manifest_and_server_time(
        &self,
        id: &str,
    ) -> Result<(VersionedManifest, TrustedServerTimeSample), SessionV2Error> {
        match self {
            Self::OneDrive(t) => t.load_manifest_and_server_time(id),
            Self::Local(t) => t.load_manifest_and_server_time(id),
        }
    }
    fn stage_immutable(
        &self,
        class: SessionObjectClass,
        id: &str,
        bytes: &[u8],
    ) -> Result<(), SessionV2Error> {
        match self {
            Self::OneDrive(t) => t.stage_immutable(class, id, bytes),
            Self::Local(t) => t.stage_immutable(class, id, bytes),
        }
    }
    fn load_immutable(
        &self,
        class: SessionObjectClass,
        id: &str,
    ) -> Result<Vec<u8>, SessionV2Error> {
        match self {
            Self::OneDrive(t) => t.load_immutable(class, id),
            Self::Local(t) => t.load_immutable(class, id),
        }
    }
    fn load_immutable_before(
        &self,
        class: SessionObjectClass,
        id: &str,
        deadline: Instant,
    ) -> Result<Vec<u8>, SessionV2Error> {
        match self {
            Self::OneDrive(t) => t.load_immutable_before(class, id, deadline),
            Self::Local(t) => t.load_immutable_before(class, id, deadline),
        }
    }
    fn load_immutable_batch(
        &self,
        class: SessionObjectClass,
        ids: &[String],
    ) -> Result<Vec<Vec<u8>>, SessionV2Error> {
        match self {
            Self::OneDrive(t) => t.load_immutable_batch(class, ids),
            Self::Local(t) => t.load_immutable_batch(class, ids),
        }
    }
    fn load_immutable_optional(
        &self,
        class: SessionObjectClass,
        id: &str,
    ) -> Result<Option<Vec<u8>>, SessionV2Error> {
        match self {
            Self::OneDrive(t) => t.load_immutable_optional(class, id),
            Self::Local(t) => t.load_immutable_optional(class, id),
        }
    }
    fn finish_admission(&self) {
        match self {
            Self::OneDrive(t) => t.finish_admission(),
            Self::Local(t) => t.finish_admission(),
        }
    }
    fn reap_unreachable(
        &self,
        reachable: &BTreeSet<(SessionObjectClass, String)>,
        older: u64,
        limit: usize,
        deadline: Instant,
    ) -> Result<usize, SessionV2Error> {
        match self {
            Self::OneDrive(t) => t.reap_unreachable(reachable, older, limit, deadline),
            Self::Local(t) => t.reap_unreachable(reachable, older, limit, deadline),
        }
    }
    fn compare_and_swap_manifest(
        &self,
        current: &VersionedManifest,
        next: &SessionManifestV1,
    ) -> Result<Option<VersionedManifest>, SessionV2Error> {
        match self {
            Self::OneDrive(t) => t.compare_and_swap_manifest(current, next),
            Self::Local(t) => t.compare_and_swap_manifest(current, next),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_session_lock_is_shared_by_clones_and_fences_other_openers() {
        let root = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.accounts.push(isyncyou_core::AccountConfig {
            id: "a".into(),
            username: "a".into(),
            sync_root: root.path().join("sync"),
            archive_root: root.path().into(),
            cache_root: root.path().join("cache"),
            mount_point: None,
        });
        cfg.provider_accounts.insert(
            "a".into(),
            ProviderConfig::SelfHosted {
                origin: "https://example.invalid".into(),
                client_id: "client".into(),
            },
        );
        let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let first = ProductSessionTransport::new(Some(&cfg), "a", "", id).unwrap();
        let retained = first.clone();
        assert!(matches!(
            ProductSessionTransport::new(Some(&cfg), "a", "", id),
            Err(SessionV2Error::LeaseLost)
        ));
        drop(first);
        assert!(matches!(
            ProductSessionTransport::new(Some(&cfg), "a", "", id),
            Err(SessionV2Error::LeaseLost)
        ));
        drop(retained);
        assert!(ProductSessionTransport::new(Some(&cfg), "a", "", id).is_ok());
        assert!(ProductSessionTransport::new(Some(&cfg), "missing", "", id).is_err());
    }

    #[cfg(feature = "encrypted-store")]
    #[test]
    fn encrypted_local_session_survives_reopen_and_enforces_manifest_cas() {
        let root = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.accounts.push(isyncyou_core::AccountConfig {
            id: "a".into(),
            username: "a".into(),
            sync_root: root.path().join("sync"),
            archive_root: root.path().into(),
            cache_root: root.path().join("cache"),
            mount_point: None,
        });
        cfg.provider_accounts.insert(
            "a".into(),
            ProviderConfig::SelfHosted {
                origin: "https://example.invalid".into(),
                client_id: "client".into(),
            },
        );
        let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let pairing =
            isyncyou_agent::PairingPayload::generate(isyncyou_agent::SessionId::new(id).unwrap())
                .unwrap();
        let crypto =
            SessionObjectCrypto::new(pairing.pairing_secret(), pairing.crypto_config().unwrap())
                .unwrap();
        let db = root.path().join("session.db");
        let mut first = ProductSessionTransport::new(Some(&cfg), "a", "", id).unwrap();
        if let ProductSessionTransport::Local(local) = &mut first {
            local.test_store = Some((db.clone(), b"test-only-sqlcipher-key".to_vec()));
        }
        first.bind_manifest_crypto(crypto.clone()).unwrap();
        first.create_session().unwrap();
        let before = if let ProductSessionTransport::Local(local) = &first {
            local
                .test_write_opens
                .load(std::sync::atomic::Ordering::Relaxed)
        } else {
            panic!("local fixture required")
        };
        first
            .with_projection_writer(|| {
                first
                    .stage_immutable(SessionObjectClass::VisibleRecord, "scoped-1", b"first")
                    .map_err(|_| "stage")?;
                first
                    .stage_immutable(SessionObjectClass::VisibleRecord, "scoped-2", b"second")
                    .map_err(|_| "stage")?;
                Ok(())
            })
            .unwrap();
        if let ProductSessionTransport::Local(local) = &first {
            assert_eq!(
                local
                    .test_write_opens
                    .load(std::sync::atomic::Ordering::Relaxed)
                    - before,
                1
            );
            assert!(local.projection_writer.lock().unwrap().is_none());
        }
        let before = if let ProductSessionTransport::Local(local) = &first {
            local
                .test_write_opens
                .load(std::sync::atomic::Ordering::Relaxed)
        } else {
            unreachable!()
        };
        first
            .with_projection_writer(|| {
                let session =
                    isyncyou_agent::SessionV2Store::new(first.clone(), &[9; 32], crypto.clone());
                let mut lease = session
                    .acquire_lease(id, "scoped-lease", "holder")
                    .map_err(|_| "lease")?;
                lease.release().map_err(|_| "release")?;
                Ok(())
            })
            .unwrap();
        if let ProductSessionTransport::Local(local) = &first {
            assert_eq!(
                local
                    .test_write_opens
                    .load(std::sync::atomic::Ordering::Relaxed)
                    - before,
                1
            );
            assert!(local.projection_writer.lock().unwrap().is_none());
        }
        assert!(first
            .with_projection_writer::<()>(|| Err("fixture".into()))
            .is_err());
        if let ProductSessionTransport::Local(local) = &first {
            assert!(local.projection_writer.lock().unwrap().is_none());
        }
        #[cfg(unix)]
        first
            .with_projection_writer(|| {
                use std::os::unix::fs::PermissionsExt;
                let original = std::fs::metadata(&db).unwrap().permissions();
                std::fs::set_permissions(
                    &db,
                    std::fs::Permissions::from_mode(original.mode() ^ 0o040),
                )
                .unwrap();
                let result = first.stage_immutable(
                    SessionObjectClass::VisibleRecord,
                    "identity-change",
                    b"denied",
                );
                std::fs::set_permissions(&db, original).unwrap();
                assert!(result.is_err());
                Ok(())
            })
            .unwrap();
        let session = isyncyou_agent::SessionV2Store::new(first.clone(), &[9; 32], crypto.clone());
        let mut lease = session.acquire_lease(id, "lease-a", "holder-a").unwrap();
        assert!(!lease.is_lost());
        lease.release().unwrap();
        drop(lease);
        drop(session);
        let manifest = first.load_manifest(id).unwrap();
        if let ProductSessionTransport::Local(local) = &first {
            let identity = local.reader.lock().unwrap().as_ref().unwrap().0;
            let opens = local
                .test_read_opens
                .load(std::sync::atomic::Ordering::Relaxed);
            first.load_manifest(id).unwrap();
            assert_eq!(
                local
                    .test_read_opens
                    .load(std::sync::atomic::Ordering::Relaxed),
                opens
            );
            assert_eq!(local.reader.lock().unwrap().as_ref().unwrap().0, identity);
            let moved = root.path().join("session.moved");
            std::fs::rename(&db, &moved).unwrap();
            assert!(first.load_manifest(id).is_err());
            assert!(local.reader.lock().unwrap().is_none());
            std::fs::rename(&moved, &db).unwrap();
        }
        first
            .stage_immutable(
                SessionObjectClass::VisibleRecord,
                "item",
                b"synthetic-ciphertext",
            )
            .unwrap();
        first
            .stage_immutable(
                SessionObjectClass::VisibleRecord,
                "item-2",
                b"second-ciphertext",
            )
            .unwrap();
        assert_eq!(
            first
                .load_immutable_batch(
                    SessionObjectClass::VisibleRecord,
                    &["item".into(), "item-2".into()]
                )
                .unwrap(),
            vec![
                b"synthetic-ciphertext".to_vec(),
                b"second-ciphertext".to_vec()
            ]
        );
        assert!(first
            .load_immutable_batch(
                SessionObjectClass::VisibleRecord,
                &["item".into(), "missing".into()]
            )
            .is_err());
        assert!(first
            .stage_immutable(SessionObjectClass::VisibleRecord, "item", b"different")
            .is_err());
        let mut next = manifest.manifest.clone();
        next.generation += 1;
        let updated = first
            .compare_and_swap_manifest(&manifest, &next)
            .unwrap()
            .unwrap();
        assert!(first
            .compare_and_swap_manifest(&manifest, &next)
            .unwrap()
            .is_none());
        drop(first);
        let mut reopened = ProductSessionTransport::new(Some(&cfg), "a", "", id).unwrap();
        if let ProductSessionTransport::Local(local) = &mut reopened {
            local.test_store = Some((db.clone(), b"test-only-sqlcipher-key".to_vec()));
        }
        reopened.bind_manifest_crypto(crypto).unwrap();
        assert_eq!(reopened.load_manifest(id).unwrap().etag, updated.etag);
        assert_eq!(
            reopened
                .load_immutable(SessionObjectClass::VisibleRecord, "item")
                .unwrap(),
            b"synthetic-ciphertext"
        );
        assert!(!std::fs::read(db)
            .unwrap()
            .windows(b"synthetic-ciphertext".len())
            .any(|w| w == b"synthetic-ciphertext"));
    }
}
