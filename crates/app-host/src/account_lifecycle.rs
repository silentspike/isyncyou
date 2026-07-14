// Task 2 freezes the durable record contract; Task 3 wires it into the host state machine.
#![allow(dead_code)]

use isyncyou_agent::oauth::{RevokeRequestTarget, RevokeScopeGuarantee};
use isyncyou_agent::ProductProviderId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub(crate) const INSTALLATION_PLAINTEXT_MAX: usize = 4 * 1024;
pub(crate) const AUTHORITY_PLAINTEXT_MAX: usize = 32 * 1024;
pub(crate) const JOURNAL_PLAINTEXT_MAX: usize = 16 * 1024;
pub(crate) const EXCHANGE_PLAINTEXT_MAX: usize = 16 * 1024;
pub(crate) const CANDIDATE_PLAINTEXT_MAX: usize = 64 * 1024;
pub(crate) const RECEIPT_INDEX_PLAINTEXT_MAX: usize = 128 * 1024;

pub(crate) const INSTALLATION_ENVELOPE_MAX: usize = 7_532;
pub(crate) const AUTHORITY_ENVELOPE_MAX: usize = 45_760;
pub(crate) const JOURNAL_ENVELOPE_MAX: usize = 23_916;
pub(crate) const EXCHANGE_ENVELOPE_MAX: usize = 23_916;
pub(crate) const CANDIDATE_ENVELOPE_MAX: usize = 89_452;
pub(crate) const RECEIPT_INDEX_ENVELOPE_MAX: usize = 176_832;

const SCHEMA_VERSION: u32 = 1;
const MAX_RECEIPTS: usize = 64;
const MAX_RETIRED_ETAGS: usize = 64;
const MAX_RECEIPT_BYTES: usize = 1_536;
const MAX_ATTEMPTS: u8 = 8;
const OPERATION_ID_LEN: usize = 32;
const DIGEST_LEN: usize = 43;
const NONCE_LEN: usize = 22;
const MAX_TOKEN_BYTES: usize = 32 * 1024;
const MAX_PROVIDER_ACCOUNT_ID_BYTES: usize = 512;
const MAX_CLOSED_CODE_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AccountLifecycleRoute {
    Logout,
    LifecycleResume,
    OAuthStart,
}

impl AccountLifecycleRoute {
    fn wire(self) -> &'static str {
        match self {
            Self::Logout => "logout",
            Self::LifecycleResume => "lifecycle_resume",
            Self::OAuthStart => "oauth_start",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AccountLifecycleMode {
    Connect,
    Disconnect,
    Reconnect,
    Switch,
}

impl AccountLifecycleMode {
    fn wire(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Disconnect => "disconnect",
            Self::Reconnect => "reconnect",
            Self::Switch => "switch",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AccountLifecyclePhase {
    Prepared,
    RevokeInFlight,
    RevokeOutcomeUnknown,
    RevokedPendingCleanup,
    Disconnected,
    AwaitingOAuthLogin,
    ExchangeInFlight,
    ExchangeOutcomeUnknown,
    OAuthCandidateStored,
    CandidateValidation,
    OAuthCandidateCleanup,
    Completed,
    FailedTerminal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub(crate) enum RevokedGrantRef {
    ActiveCredential { generation: String },
    OAuthCandidate { record_id: String },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparedOperationV1 {
    pub version: u32,
    pub provider: ProductProviderId,
    pub operation_id: String,
    pub route: AccountLifecycleRoute,
    pub request_id_hash: String,
    pub idempotency_key: String,
    pub payload_digest: String,
    pub mode: AccountLifecycleMode,
    pub lifecycle_epoch: u64,
    pub fence_epoch: u64,
    pub lifecycle_key_version: u32,
    pub credential_etag: Option<String>,
    pub prior_generation: Option<String>,
    pub prior_subject_digest: Option<String>,
    pub revoke_spec_version: u32,
    pub initial_revoke_request_target: Option<RevokeRequestTarget>,
    pub initial_revoke_scope_guarantee: Option<RevokeScopeGuarantee>,
    pub prepared_at_ms: u64,
}

impl std::fmt::Debug for PreparedOperationV1 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedOperationV1")
            .field("provider", &self.provider)
            .field("mode", &self.mode)
            .field("route", &self.route)
            .field("lifecycle_epoch", &self.lifecycle_epoch)
            .field("fence_epoch", &self.fence_epoch)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActiveOperationRefV1 {
    pub prepared: PreparedOperationV1,
    pub operation_etag: String,
    pub journal_record_id: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountLifecycleInstallationV1 {
    pub version: u32,
    pub installation_principal_initialized: bool,
    pub installation_principal: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountLifecycleAuthorityV1 {
    pub version: u32,
    pub installation_principal_initialized: bool,
    pub lifecycle_epoch: u64,
    pub fence_epoch: u64,
    pub lifecycle_key_version: u32,
    pub current_credential_etags: BTreeMap<ProductProviderId, String>,
    pub retired_credential_etags: BTreeMap<ProductProviderId, Vec<String>>,
    pub active_operations: BTreeMap<ProductProviderId, ActiveOperationRefV1>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountLifecycleJournalV1 {
    pub version: u32,
    pub prepared: PreparedOperationV1,
    pub lease_owner_nonce: String,
    pub operation_etag: String,
    pub phase: AccountLifecyclePhase,
    pub revoke_leg: u32,
    pub revoked_grant: Option<RevokedGrantRef>,
    pub revoke_request_target: Option<RevokeRequestTarget>,
    pub revoke_scope_guarantee: Option<RevokeScopeGuarantee>,
    pub attempt_count: u8,
    pub in_flight_until_ms: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub closed_code: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountLifecycleReceiptV1 {
    pub version: u32,
    pub operation_id: String,
    pub operation_etag: String,
    pub route: AccountLifecycleRoute,
    pub mode: AccountLifecycleMode,
    pub idempotency_key: String,
    pub payload_digest: String,
    pub prior_generation: Option<String>,
    pub result_generation: Option<String>,
    pub completed_revoke_legs: u32,
    pub lifecycle_epoch: u64,
    pub fence_epoch: u64,
    pub lifecycle_key_version: u32,
    pub terminal_code: String,
    pub completed_at_ms: u64,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountLifecycleReceiptIndexV1 {
    pub version: u32,
    pub provider: ProductProviderId,
    pub lifecycle_epoch: u64,
    pub receipts: Vec<AccountLifecycleReceiptV1>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OAuthExchangeIntentV1 {
    pub version: u32,
    pub provider: ProductProviderId,
    pub operation_id: String,
    pub attempt_id: String,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OAuthCandidateV1 {
    pub version: u32,
    pub provider: ProductProviderId,
    pub operation_id: String,
    pub record_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_ms: u64,
    pub provider_account_id: Option<String>,
    pub subject_digest: Option<String>,
    pub session_id_digest: Option<String>,
    pub state: OAuthCandidateState,
    pub created_at_ms: u64,
    pub terminal_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OAuthCandidateState {
    GrantBearing,
    RevokeUnknown,
    RevokedCleaned,
    Promoted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderOperationKind {
    Turn,
    Refresh,
    Lifecycle,
    OAuthActivation,
    Maintenance,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProviderLeaseCounts {
    pub shared: usize,
    pub exclusive: usize,
}

#[derive(Debug, Default)]
pub(crate) struct ProviderLeaseRegistry {
    counts: Mutex<BTreeMap<ProductProviderId, ProviderLeaseCounts>>,
}

impl ProviderLeaseRegistry {
    pub(crate) fn acquire_shared(
        self: &Arc<Self>,
        root: &Path,
        provider: ProductProviderId,
        operation_id: String,
        kind: ProviderOperationKind,
    ) -> Result<ProviderOperationLease, LifecycleRecordError> {
        validate_operation_id(&operation_id)?;
        let lock =
            isyncyou_agent::FileLock::try_acquire_shared(&provider_lock_path(root, provider))
                .map_err(|_| LifecycleRecordError::Store)?
                .ok_or(LifecycleRecordError::Busy)?;
        let mut counts = self
            .counts
            .lock()
            .map_err(|_| LifecycleRecordError::Store)?;
        let value = counts.entry(provider).or_default();
        value.shared = value
            .shared
            .checked_add(1)
            .ok_or(LifecycleRecordError::CountLimit)?;
        drop(counts);
        Ok(ProviderOperationLease {
            provider,
            operation_id,
            kind,
            exclusive: false,
            registry: Arc::clone(self),
            _file_lock: lock,
        })
    }

    pub(crate) fn acquire_exclusive(
        self: &Arc<Self>,
        root: &Path,
        provider: ProductProviderId,
        operation_id: String,
        kind: ProviderOperationKind,
    ) -> Result<ProviderOperationLease, LifecycleRecordError> {
        validate_operation_id(&operation_id)?;
        let lock =
            isyncyou_agent::FileLock::try_acquire_exclusive(&provider_lock_path(root, provider))
                .map_err(|_| LifecycleRecordError::Store)?
                .ok_or(LifecycleRecordError::Busy)?;
        let mut counts = self
            .counts
            .lock()
            .map_err(|_| LifecycleRecordError::Store)?;
        let value = counts.entry(provider).or_default();
        value.exclusive = value
            .exclusive
            .checked_add(1)
            .ok_or(LifecycleRecordError::CountLimit)?;
        drop(counts);
        Ok(ProviderOperationLease {
            provider,
            operation_id,
            kind,
            exclusive: true,
            registry: Arc::clone(self),
            _file_lock: lock,
        })
    }

    pub(crate) fn counts(&self, provider: ProductProviderId) -> ProviderLeaseCounts {
        self.counts
            .lock()
            .ok()
            .and_then(|counts| counts.get(&provider).copied())
            .unwrap_or_default()
    }
}

pub(crate) struct ProviderOperationLease {
    provider: ProductProviderId,
    operation_id: String,
    kind: ProviderOperationKind,
    exclusive: bool,
    registry: Arc<ProviderLeaseRegistry>,
    _file_lock: isyncyou_agent::FileLock,
}

#[derive(Debug, Default)]
pub(crate) struct LifecycleDiagnostics {
    audit_failures: std::sync::atomic::AtomicU32,
}

impl LifecycleDiagnostics {
    pub(crate) fn record_audit_failure(&self) {
        let _ = self.audit_failures.fetch_update(
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
            |value| {
                if value == u32::MAX {
                    None
                } else {
                    Some(value + 1)
                }
            },
        );
    }

    pub(crate) fn audit_failures(&self) -> u32 {
        self.audit_failures
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

impl ProviderOperationLease {
    pub(crate) fn provider(&self) -> ProductProviderId {
        self.provider
    }

    pub(crate) fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub(crate) fn kind(&self) -> ProviderOperationKind {
        self.kind
    }
}

impl std::fmt::Debug for ProviderOperationLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderOperationLease")
            .field("provider", &self.provider)
            .field("kind", &self.kind)
            .field("exclusive", &self.exclusive)
            .finish_non_exhaustive()
    }
}

impl Drop for ProviderOperationLease {
    fn drop(&mut self) {
        let Ok(mut counts) = self.registry.counts.lock() else {
            return;
        };
        let Some(value) = counts.get_mut(&self.provider) else {
            return;
        };
        if self.exclusive {
            value.exclusive = value.exclusive.saturating_sub(1);
        } else {
            value.shared = value.shared.saturating_sub(1);
        }
        if value.shared == 0 && value.exclusive == 0 {
            counts.remove(&self.provider);
        }
    }
}

fn provider_lock_path(root: &Path, provider: ProductProviderId) -> PathBuf {
    root.join(format!(".provider-lifecycle-{}.lock", provider.wire()))
}

impl std::fmt::Debug for OAuthCandidateV1 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthCandidateV1")
            .field("provider", &self.provider)
            .field("credential_present", &true)
            .field("subject_digest_present", &self.subject_digest.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleRecordError {
    Invalid,
    SizeLimit,
    CountLimit,
    EpochExhausted,
    InvalidTransition,
    Store,
    Busy,
    MissingInstallationPrincipal,
    IdempotencyConflict,
    OperationInProgress,
    StaleFence,
    ReceiptCapacityExhausted,
}

impl LifecycleRecordError {
    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::Busy | Self::OperationInProgress => "provider_busy",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::EpochExhausted => "lifecycle_epoch_exhausted",
            Self::StaleFence => "stale_lifecycle_fence",
            Self::ReceiptCapacityExhausted => "receipt_capacity_exhausted",
            Self::MissingInstallationPrincipal | Self::Store => "lifecycle_unavailable",
            Self::Invalid | Self::SizeLimit | Self::CountLimit | Self::InvalidTransition => {
                "lifecycle_invalid"
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BeginLifecycleOperation {
    pub operation_id: String,
    pub operation_etag: String,
    pub journal_record_id: String,
    pub idempotent_replay: bool,
}

pub(crate) struct InstallationContext {
    principal: String,
    pub authority: AccountLifecycleAuthorityV1,
}

impl InstallationContext {
    pub(crate) fn principal(&self) -> &str {
        &self.principal
    }
}

impl std::fmt::Debug for InstallationContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallationContext")
            .field("principal", &"[redacted]")
            .field("lifecycle_epoch", &self.authority.lifecycle_epoch)
            .field("key_version", &self.authority.lifecycle_key_version)
            .finish()
    }
}

pub(crate) struct LifecycleRepository {
    store: isyncyou_agent::AgentCredentialStore,
    store_dir: PathBuf,
    lock_path: PathBuf,
}

impl LifecycleRepository {
    pub(crate) fn new(
        store: isyncyou_agent::AgentCredentialStore,
        store_dir: impl Into<PathBuf>,
        lock_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            store,
            store_dir: store_dir.into(),
            lock_path: lock_path.into(),
        }
    }

    pub(crate) fn initialize(&self) -> Result<InstallationContext, LifecycleRecordError> {
        let _lock = isyncyou_agent::FileLock::try_acquire_exclusive(&self.lock_path)
            .map_err(|_| LifecycleRecordError::Store)?
            .ok_or(LifecycleRecordError::Busy)?;
        const INSTALLATION_ID: &str = "installation";
        let installation = self
            .store
            .get_bounded(
                isyncyou_agent::SecretClass::AccountLifecycleInstallation,
                INSTALLATION_ID,
                INSTALLATION_ENVELOPE_MAX,
                INSTALLATION_PLAINTEXT_MAX,
            )
            .map_err(|_| LifecycleRecordError::Store)?;
        let installation = match installation {
            Some(secret) => {
                let record: AccountLifecycleInstallationV1 =
                    parse_bounded(secret.expose(), INSTALLATION_PLAINTEXT_MAX)?;
                if record.version != SCHEMA_VERSION
                    || !record.installation_principal_initialized
                    || validate_b64url(&record.installation_principal, 22).is_err()
                {
                    return Err(LifecycleRecordError::MissingInstallationPrincipal);
                }
                record
            }
            None => {
                if directory_contains_class(&self.store_dir, "account-lifecycle-authority__")? {
                    return Err(LifecycleRecordError::MissingInstallationPrincipal);
                }
                let record = AccountLifecycleInstallationV1 {
                    version: SCHEMA_VERSION,
                    installation_principal_initialized: true,
                    installation_principal: random_b64url(16)?,
                };
                let bytes = serialize_bounded(&record, INSTALLATION_PLAINTEXT_MAX)?;
                self.store
                    .put_bounded(
                        isyncyou_agent::SecretClass::AccountLifecycleInstallation,
                        INSTALLATION_ID,
                        &isyncyou_agent::Secret::new(bytes),
                        INSTALLATION_ENVELOPE_MAX,
                    )
                    .map_err(|_| LifecycleRecordError::Store)?;
                record
            }
        };
        let authority_id = &installation.installation_principal;
        let authority = match self
            .store
            .get_bounded(
                isyncyou_agent::SecretClass::AccountLifecycleAuthority,
                authority_id,
                AUTHORITY_ENVELOPE_MAX,
                AUTHORITY_PLAINTEXT_MAX,
            )
            .map_err(|_| LifecycleRecordError::Store)?
        {
            Some(secret) => {
                let authority = parse_bounded(secret.expose(), AUTHORITY_PLAINTEXT_MAX)?;
                validate_authority(&authority)?;
                authority
            }
            None => {
                let authority = AccountLifecycleAuthorityV1 {
                    version: SCHEMA_VERSION,
                    installation_principal_initialized: true,
                    lifecycle_epoch: 0,
                    fence_epoch: 0,
                    lifecycle_key_version: 1,
                    current_credential_etags: BTreeMap::new(),
                    retired_credential_etags: BTreeMap::new(),
                    active_operations: BTreeMap::new(),
                };
                self.put_authority(authority_id, &authority)?;
                authority
            }
        };
        Ok(InstallationContext {
            principal: installation.installation_principal,
            authority,
        })
    }

    /// Read the existing lifecycle authority without creating, reaping, or recovering state.
    /// Status/readiness paths use this method so they remain mutation-free.
    pub(crate) fn load_existing(
        &self,
    ) -> Result<Option<InstallationContext>, LifecycleRecordError> {
        const INSTALLATION_ID: &str = "installation";
        let installation = self
            .store
            .get_bounded(
                isyncyou_agent::SecretClass::AccountLifecycleInstallation,
                INSTALLATION_ID,
                INSTALLATION_ENVELOPE_MAX,
                INSTALLATION_PLAINTEXT_MAX,
            )
            .map_err(|_| LifecycleRecordError::Store)?;
        let Some(installation) = installation else {
            if directory_contains_class(&self.store_dir, "account-lifecycle-authority__")? {
                return Err(LifecycleRecordError::MissingInstallationPrincipal);
            }
            return Ok(None);
        };
        let installation: AccountLifecycleInstallationV1 =
            parse_bounded(installation.expose(), INSTALLATION_PLAINTEXT_MAX)?;
        if installation.version != SCHEMA_VERSION
            || !installation.installation_principal_initialized
            || validate_b64url(&installation.installation_principal, NONCE_LEN).is_err()
        {
            return Err(LifecycleRecordError::MissingInstallationPrincipal);
        }
        let authority = self
            .store
            .get_bounded(
                isyncyou_agent::SecretClass::AccountLifecycleAuthority,
                &installation.installation_principal,
                AUTHORITY_ENVELOPE_MAX,
                AUTHORITY_PLAINTEXT_MAX,
            )
            .map_err(|_| LifecycleRecordError::Store)?
            .ok_or(LifecycleRecordError::MissingInstallationPrincipal)?;
        let authority = parse_bounded(authority.expose(), AUTHORITY_PLAINTEXT_MAX)?;
        validate_authority(&authority)?;
        Ok(Some(InstallationContext {
            principal: installation.installation_principal,
            authority,
        }))
    }

    pub(crate) fn provider_blocked(
        &self,
        provider: ProductProviderId,
    ) -> Result<bool, LifecycleRecordError> {
        Ok(self
            .load_existing()?
            .is_some_and(|context| context.authority.active_operations.contains_key(&provider)))
    }

    pub(crate) fn put_authority(
        &self,
        principal: &str,
        authority: &AccountLifecycleAuthorityV1,
    ) -> Result<(), LifecycleRecordError> {
        validate_authority(authority)?;
        self.put_record(
            isyncyou_agent::SecretClass::AccountLifecycleAuthority,
            principal,
            authority,
            AUTHORITY_PLAINTEXT_MAX,
            AUTHORITY_ENVELOPE_MAX,
        )
    }

    pub(crate) fn put_journal(
        &self,
        id: &str,
        journal: &AccountLifecycleJournalV1,
    ) -> Result<(), LifecycleRecordError> {
        validate_b64url(id, 43)?;
        validate_journal(journal)?;
        self.put_record(
            isyncyou_agent::SecretClass::AccountLifecycleJournal,
            id,
            journal,
            JOURNAL_PLAINTEXT_MAX,
            JOURNAL_ENVELOPE_MAX,
        )
    }

    pub(crate) fn put_candidate(
        &self,
        id: &str,
        candidate: &OAuthCandidateV1,
    ) -> Result<(), LifecycleRecordError> {
        validate_b64url(id, 43)?;
        validate_candidate(candidate, id)?;
        self.put_record(
            isyncyou_agent::SecretClass::OAuthCandidate,
            id,
            candidate,
            CANDIDATE_PLAINTEXT_MAX,
            CANDIDATE_ENVELOPE_MAX,
        )
    }

    pub(crate) fn put_exchange_intent(
        &self,
        id: &str,
        intent: &OAuthExchangeIntentV1,
    ) -> Result<(), LifecycleRecordError> {
        validate_b64url(id, DIGEST_LEN)?;
        validate_exchange_intent(intent)?;
        self.put_record(
            isyncyou_agent::SecretClass::OAuthExchangeIntent,
            id,
            intent,
            EXCHANGE_PLAINTEXT_MAX,
            EXCHANGE_ENVELOPE_MAX,
        )
    }

    pub(crate) fn put_receipt_index(
        &self,
        id: &str,
        index: &AccountLifecycleReceiptIndexV1,
    ) -> Result<(), LifecycleRecordError> {
        validate_b64url(id, DIGEST_LEN)?;
        validate_receipts(index)?;
        self.put_record(
            isyncyou_agent::SecretClass::AccountLifecycleReceiptIndex,
            id,
            index,
            RECEIPT_INDEX_PLAINTEXT_MAX,
            RECEIPT_INDEX_ENVELOPE_MAX,
        )
    }

    pub(crate) fn load_journal(
        &self,
        id: &str,
    ) -> Result<Option<AccountLifecycleJournalV1>, LifecycleRecordError> {
        validate_b64url(id, DIGEST_LEN)?;
        let Some(secret) = self
            .store
            .get_bounded(
                isyncyou_agent::SecretClass::AccountLifecycleJournal,
                id,
                JOURNAL_ENVELOPE_MAX,
                JOURNAL_PLAINTEXT_MAX,
            )
            .map_err(|_| LifecycleRecordError::Store)?
        else {
            return Ok(None);
        };
        let journal = parse_bounded(secret.expose(), JOURNAL_PLAINTEXT_MAX)?;
        validate_journal(&journal)?;
        Ok(Some(journal))
    }

    pub(crate) fn delete_journal(&self, id: &str) -> Result<(), LifecycleRecordError> {
        validate_b64url(id, DIGEST_LEN)?;
        self.store
            .delete_durable(isyncyou_agent::SecretClass::AccountLifecycleJournal, id)
            .map_err(|_| LifecycleRecordError::Store)
    }

    /// Recreate only the Prepared journal already committed in the authority. This closes the
    /// authority-before-journal crash window without consulting mutable provider configuration.
    pub(crate) fn recover_active_journal(
        &self,
        provider: ProductProviderId,
        now_ms: u64,
    ) -> Result<Option<AccountLifecycleJournalV1>, LifecycleRecordError> {
        let context = self
            .load_existing()?
            .ok_or(LifecycleRecordError::StaleFence)?;
        let Some(active) = context.authority.active_operations.get(&provider) else {
            return Ok(None);
        };
        if let Some(journal) = self.load_journal(&active.journal_record_id)? {
            return Ok(Some(journal));
        }
        validate_prepared(&active.prepared)?;
        let journal = AccountLifecycleJournalV1 {
            version: SCHEMA_VERSION,
            prepared: active.prepared.clone(),
            lease_owner_nonce: random_b64url(16)?,
            operation_etag: active.operation_etag.clone(),
            phase: AccountLifecyclePhase::Prepared,
            revoke_leg: 0,
            revoked_grant: None,
            revoke_request_target: None,
            revoke_scope_guarantee: None,
            attempt_count: 0,
            in_flight_until_ms: 0,
            created_at_ms: active.prepared.prepared_at_ms,
            updated_at_ms: now_ms.max(active.prepared.prepared_at_ms),
            closed_code: None,
        };
        self.write_journal_fenced(&active.journal_record_id, &journal)?;
        Ok(Some(journal))
    }

    pub(crate) fn begin_disconnect(
        &self,
        provider: ProductProviderId,
        request_id: &str,
        generation: &str,
        subject_digest: Option<&str>,
        target: RevokeRequestTarget,
        scope: RevokeScopeGuarantee,
        now_ms: u64,
    ) -> Result<BeginLifecycleOperation, LifecycleRecordError> {
        validate_request_id(request_id)?;
        if !is_uuid_v4(generation)
            || subject_digest.is_some_and(|value| validate_b64url(value, DIGEST_LEN).is_err())
        {
            return Err(LifecycleRecordError::Invalid);
        }
        let mut context = self.initialize()?;
        let idempotency_key = self.derive_tag(
            b"isyncyou/account-lifecycle-idempotency/v1",
            &[
                context.principal.as_bytes(),
                AccountLifecycleRoute::Logout.wire().as_bytes(),
                request_id.as_bytes(),
            ],
        )?;
        let payload_digest = self.derive_tag(
            b"isyncyou/account-lifecycle-payload/v1",
            &[
                provider.wire().as_bytes(),
                AccountLifecycleMode::Disconnect.wire().as_bytes(),
                generation.as_bytes(),
                revoke_target_wire(target).as_bytes(),
                revoke_scope_wire(scope).as_bytes(),
            ],
        )?;
        if let Some(existing) = context.authority.active_operations.get(&provider) {
            if existing.prepared.idempotency_key == idempotency_key {
                if existing.prepared.payload_digest != payload_digest {
                    return Err(LifecycleRecordError::IdempotencyConflict);
                }
                return Ok(BeginLifecycleOperation {
                    operation_id: existing.prepared.operation_id.clone(),
                    operation_etag: existing.operation_etag.clone(),
                    journal_record_id: existing.journal_record_id.clone(),
                    idempotent_replay: true,
                });
            }
            return Err(LifecycleRecordError::OperationInProgress);
        }
        if context.authority.active_operations.len() >= ProductProviderId::ALL.len() {
            return Err(LifecycleRecordError::CountLimit);
        }
        let receipt_id = self.receipt_index_id(&context.principal, provider)?;
        if let Some(index) = self.load_receipt_index(&receipt_id)? {
            for receipt in index.receipts {
                if receipt.idempotency_key == idempotency_key {
                    if receipt.payload_digest != payload_digest {
                        return Err(LifecycleRecordError::IdempotencyConflict);
                    }
                    return Ok(BeginLifecycleOperation {
                        operation_id: receipt.operation_id,
                        operation_etag: receipt.operation_etag,
                        journal_record_id: self.derive_tag(
                            b"isyncyou/account-lifecycle-journal-id/v1",
                            &[
                                context.principal.as_bytes(),
                                provider.wire().as_bytes(),
                                b"terminal",
                            ],
                        )?,
                        idempotent_replay: true,
                    });
                }
            }
        }
        let lifecycle_epoch = next_epoch(context.authority.lifecycle_epoch)?;
        let fence_epoch = next_epoch(context.authority.fence_epoch)?;
        let operation_id = mint_operation_id()?;
        let operation_etag = self.derive_tag(
            b"isyncyou/account-lifecycle-operation-etag/v1",
            &[
                context.principal.as_bytes(),
                provider.wire().as_bytes(),
                operation_id.as_bytes(),
                &lifecycle_epoch.to_be_bytes(),
                &fence_epoch.to_be_bytes(),
            ],
        )?;
        let journal_record_id = self.derive_tag(
            b"isyncyou/account-lifecycle-journal-id/v1",
            &[
                context.principal.as_bytes(),
                provider.wire().as_bytes(),
                operation_id.as_bytes(),
            ],
        )?;
        let credential_etag = self.derive_tag(
            b"isyncyou/account-lifecycle-credential-etag/v1",
            &[
                context.principal.as_bytes(),
                provider.wire().as_bytes(),
                generation.as_bytes(),
                &lifecycle_epoch.to_be_bytes(),
            ],
        )?;
        let prepared = PreparedOperationV1 {
            version: SCHEMA_VERSION,
            provider,
            operation_id: operation_id.clone(),
            route: AccountLifecycleRoute::Logout,
            request_id_hash: self.derive_tag(
                b"isyncyou/account-lifecycle-request-id/v1",
                &[request_id.as_bytes()],
            )?,
            idempotency_key,
            payload_digest,
            mode: AccountLifecycleMode::Disconnect,
            lifecycle_epoch,
            fence_epoch,
            lifecycle_key_version: context.authority.lifecycle_key_version,
            credential_etag: Some(credential_etag.clone()),
            prior_generation: Some(generation.to_string()),
            prior_subject_digest: subject_digest.map(str::to_string),
            revoke_spec_version: 1,
            initial_revoke_request_target: Some(target),
            initial_revoke_scope_guarantee: Some(scope),
            prepared_at_ms: now_ms,
        };
        let active = ActiveOperationRefV1 {
            prepared: prepared.clone(),
            operation_etag: operation_etag.clone(),
            journal_record_id: journal_record_id.clone(),
        };
        context.authority.lifecycle_epoch = lifecycle_epoch;
        context.authority.fence_epoch = fence_epoch;
        context
            .authority
            .current_credential_etags
            .insert(provider, credential_etag);
        context.authority.active_operations.insert(provider, active);
        self.put_authority(&context.principal, &context.authority)?;

        let journal = AccountLifecycleJournalV1 {
            version: SCHEMA_VERSION,
            prepared,
            lease_owner_nonce: random_b64url(16)?,
            operation_etag: operation_etag.clone(),
            phase: AccountLifecyclePhase::Prepared,
            revoke_leg: 0,
            revoked_grant: None,
            revoke_request_target: None,
            revoke_scope_guarantee: None,
            attempt_count: 0,
            in_flight_until_ms: 0,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            closed_code: None,
        };
        self.put_journal(&journal_record_id, &journal)?;
        Ok(BeginLifecycleOperation {
            operation_id,
            operation_etag,
            journal_record_id,
            idempotent_replay: false,
        })
    }

    pub(crate) fn write_journal_fenced(
        &self,
        id: &str,
        journal: &AccountLifecycleJournalV1,
    ) -> Result<(), LifecycleRecordError> {
        let context = self
            .load_existing()?
            .ok_or(LifecycleRecordError::StaleFence)?;
        let active = context
            .authority
            .active_operations
            .get(&journal.prepared.provider)
            .ok_or(LifecycleRecordError::StaleFence)?;
        if active.prepared.operation_id != journal.prepared.operation_id
            || active.operation_etag != journal.operation_etag
            || active.journal_record_id != id
            || context.authority.lifecycle_epoch != journal.prepared.lifecycle_epoch
            || context.authority.fence_epoch != journal.prepared.fence_epoch
        {
            return Err(LifecycleRecordError::StaleFence);
        }
        self.put_journal(id, journal)
    }

    pub(crate) fn start_active_revoke(
        &self,
        id: &str,
        now_ms: u64,
    ) -> Result<AccountLifecycleJournalV1, LifecycleRecordError> {
        let mut journal = self
            .load_journal(id)?
            .ok_or(LifecycleRecordError::Invalid)?;
        if !matches!(
            journal.phase,
            AccountLifecyclePhase::Prepared | AccountLifecyclePhase::RevokeOutcomeUnknown
        ) {
            return Err(LifecycleRecordError::InvalidTransition);
        }
        if journal.revoke_leg == 0 {
            journal.revoke_leg = 1;
            journal.revoked_grant = Some(RevokedGrantRef::ActiveCredential {
                generation: journal
                    .prepared
                    .prior_generation
                    .clone()
                    .ok_or(LifecycleRecordError::Invalid)?,
            });
            journal.revoke_request_target = journal.prepared.initial_revoke_request_target;
            journal.revoke_scope_guarantee = journal.prepared.initial_revoke_scope_guarantee;
        }
        journal.phase = AccountLifecyclePhase::RevokeInFlight;
        journal.attempt_count = journal
            .attempt_count
            .checked_add(1)
            .filter(|value| *value <= MAX_ATTEMPTS)
            .ok_or(LifecycleRecordError::CountLimit)?;
        journal.in_flight_until_ms = now_ms.saturating_add(2 * 60 * 1_000);
        journal.updated_at_ms = now_ms;
        journal.closed_code = None;
        self.write_journal_fenced(id, &journal)?;
        Ok(journal)
    }

    pub(crate) fn publish_revoke_outcome(
        &self,
        id: &str,
        confirmed: bool,
        code: &str,
        now_ms: u64,
    ) -> Result<AccountLifecycleJournalV1, LifecycleRecordError> {
        let mut journal = self
            .load_journal(id)?
            .ok_or(LifecycleRecordError::Invalid)?;
        if journal.phase != AccountLifecyclePhase::RevokeInFlight || !valid_closed_code(code) {
            return Err(LifecycleRecordError::InvalidTransition);
        }
        journal.phase = if confirmed {
            AccountLifecyclePhase::RevokedPendingCleanup
        } else {
            AccountLifecyclePhase::RevokeOutcomeUnknown
        };
        journal.in_flight_until_ms = 0;
        journal.updated_at_ms = now_ms;
        journal.closed_code = Some(code.to_string());
        self.write_journal_fenced(id, &journal)?;
        Ok(journal)
    }

    pub(crate) fn complete_disconnect(
        &self,
        id: &str,
        now_ms: u64,
    ) -> Result<AccountLifecycleReceiptV1, LifecycleRecordError> {
        let mut journal = self
            .load_journal(id)?
            .ok_or(LifecycleRecordError::Invalid)?;
        if !matches!(
            journal.phase,
            AccountLifecyclePhase::RevokedPendingCleanup | AccountLifecyclePhase::Disconnected
        ) {
            return Err(LifecycleRecordError::InvalidTransition);
        }
        if journal.phase == AccountLifecyclePhase::RevokedPendingCleanup {
            journal.phase = AccountLifecyclePhase::Disconnected;
            journal.in_flight_until_ms = 0;
            journal.updated_at_ms = now_ms;
            journal.closed_code = Some("disconnected".into());
            self.write_journal_fenced(id, &journal)?;
        }

        let mut context = self
            .load_existing()?
            .ok_or(LifecycleRecordError::StaleFence)?;
        let receipt = disconnect_receipt(&journal, now_ms);
        let receipt_id = self.receipt_index_id(&context.principal, journal.prepared.provider)?;
        let mut index =
            self.load_receipt_index(&receipt_id)?
                .unwrap_or(AccountLifecycleReceiptIndexV1 {
                    version: SCHEMA_VERSION,
                    provider: journal.prepared.provider,
                    lifecycle_epoch: journal.prepared.lifecycle_epoch,
                    receipts: Vec::new(),
                });
        let existing_receipt = index
            .receipts
            .iter()
            .find(|existing| existing.operation_id == receipt.operation_id);
        if let Some(existing) = existing_receipt {
            if existing.operation_etag != receipt.operation_etag
                || existing.idempotency_key != receipt.idempotency_key
                || existing.payload_digest != receipt.payload_digest
            {
                return Err(LifecycleRecordError::StaleFence);
            }
        } else {
            if index.receipts.len() >= MAX_RECEIPTS {
                return Err(LifecycleRecordError::ReceiptCapacityExhausted);
            }
            index.lifecycle_epoch = journal.prepared.lifecycle_epoch;
            index.receipts.push(receipt.clone());
            self.put_receipt_index(&receipt_id, &index)?;
        }

        let active = context
            .authority
            .active_operations
            .get(&journal.prepared.provider);
        if let Some(active) = active {
            if active.prepared.operation_id != journal.prepared.operation_id
                || active.operation_etag != journal.operation_etag
                || context.authority.lifecycle_epoch != journal.prepared.lifecycle_epoch
                || context.authority.fence_epoch != journal.prepared.fence_epoch
            {
                return Err(LifecycleRecordError::StaleFence);
            }
        } else {
            self.delete_journal(id)?;
            return Ok(receipt);
        }

        if let Some(etag) = context
            .authority
            .current_credential_etags
            .remove(&journal.prepared.provider)
        {
            let retired = context
                .authority
                .retired_credential_etags
                .entry(journal.prepared.provider)
                .or_default();
            if !retired.contains(&etag) {
                if retired.len() >= MAX_RETIRED_ETAGS {
                    return Err(LifecycleRecordError::ReceiptCapacityExhausted);
                }
                retired.push(etag);
            }
        }
        context
            .authority
            .active_operations
            .remove(&journal.prepared.provider);
        self.put_authority(&context.principal, &context.authority)?;
        self.delete_journal(id)?;
        Ok(receipt)
    }

    fn load_receipt_index(
        &self,
        id: &str,
    ) -> Result<Option<AccountLifecycleReceiptIndexV1>, LifecycleRecordError> {
        let Some(secret) = self
            .store
            .get_bounded(
                isyncyou_agent::SecretClass::AccountLifecycleReceiptIndex,
                id,
                RECEIPT_INDEX_ENVELOPE_MAX,
                RECEIPT_INDEX_PLAINTEXT_MAX,
            )
            .map_err(|_| LifecycleRecordError::Store)?
        else {
            return Ok(None);
        };
        let index = parse_bounded(secret.expose(), RECEIPT_INDEX_PLAINTEXT_MAX)?;
        validate_receipts(&index)?;
        Ok(Some(index))
    }

    fn receipt_index_id(
        &self,
        principal: &str,
        provider: ProductProviderId,
    ) -> Result<String, LifecycleRecordError> {
        self.derive_tag(
            b"isyncyou/account-lifecycle-receipt-id/v1",
            &[principal.as_bytes(), provider.wire().as_bytes()],
        )
    }

    fn derive_tag(
        &self,
        domain: &'static [u8],
        fields: &[&[u8]],
    ) -> Result<String, LifecycleRecordError> {
        use base64::Engine;
        let mut message = Vec::new();
        for field in fields {
            let len = u32::try_from(field.len()).map_err(|_| LifecycleRecordError::SizeLimit)?;
            message.extend_from_slice(&len.to_be_bytes());
            message.extend_from_slice(field);
        }
        let tag = self
            .store
            .domain_hmac(domain, &message)
            .map_err(|_| LifecycleRecordError::Store)?;
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(tag))
    }

    fn put_record<T: Serialize>(
        &self,
        class: isyncyou_agent::SecretClass,
        id: &str,
        value: &T,
        plaintext_max: usize,
        envelope_max: usize,
    ) -> Result<(), LifecycleRecordError> {
        let bytes = serialize_bounded(value, plaintext_max)?;
        self.store
            .put_bounded(class, id, &isyncyou_agent::Secret::new(bytes), envelope_max)
            .map_err(|_| LifecycleRecordError::Store)
    }
}

fn disconnect_receipt(
    journal: &AccountLifecycleJournalV1,
    completed_at_ms: u64,
) -> AccountLifecycleReceiptV1 {
    AccountLifecycleReceiptV1 {
        version: SCHEMA_VERSION,
        operation_id: journal.prepared.operation_id.clone(),
        operation_etag: journal.operation_etag.clone(),
        route: journal.prepared.route,
        mode: journal.prepared.mode,
        idempotency_key: journal.prepared.idempotency_key.clone(),
        payload_digest: journal.prepared.payload_digest.clone(),
        prior_generation: journal.prepared.prior_generation.clone(),
        result_generation: None,
        completed_revoke_legs: journal.revoke_leg,
        lifecycle_epoch: journal.prepared.lifecycle_epoch,
        fence_epoch: journal.prepared.fence_epoch,
        lifecycle_key_version: journal.prepared.lifecycle_key_version,
        terminal_code: "disconnected".into(),
        completed_at_ms,
    }
}

fn directory_contains_class(dir: &Path, prefix: &str) -> Result<bool, LifecycleRecordError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(LifecycleRecordError::Store),
    };
    for entry in entries {
        let entry = entry.map_err(|_| LifecycleRecordError::Store)?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(prefix))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn random_b64url(bytes: usize) -> Result<String, LifecycleRecordError> {
    use base64::Engine;
    use ring::rand::SecureRandom;
    let mut random = vec![0u8; bytes];
    ring::rand::SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| LifecycleRecordError::Store)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random))
}

pub(crate) fn mint_operation_id() -> Result<String, LifecycleRecordError> {
    random_b64url(24)
}

pub(crate) fn candidate_reapable(candidate: &OAuthCandidateV1, now_ms: u64) -> bool {
    matches!(
        candidate.state,
        OAuthCandidateState::RevokedCleaned | OAuthCandidateState::Promoted
    ) && candidate
        .terminal_at_ms
        .is_some_and(|terminal| now_ms.saturating_sub(terminal) >= 30 * 24 * 60 * 60 * 1_000)
}

pub(crate) fn serialize_bounded<T: Serialize>(
    value: &T,
    max_plaintext: usize,
) -> Result<Vec<u8>, LifecycleRecordError> {
    let bytes = serde_json::to_vec(value).map_err(|_| LifecycleRecordError::Invalid)?;
    if bytes.len() > max_plaintext {
        return Err(LifecycleRecordError::SizeLimit);
    }
    Ok(bytes)
}

pub(crate) fn parse_bounded<'a, T: Deserialize<'a>>(
    bytes: &'a [u8],
    max_plaintext: usize,
) -> Result<T, LifecycleRecordError> {
    if bytes.len() > max_plaintext {
        return Err(LifecycleRecordError::SizeLimit);
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = T::deserialize(&mut deserializer).map_err(|_| LifecycleRecordError::Invalid)?;
    deserializer
        .end()
        .map_err(|_| LifecycleRecordError::Invalid)?;
    Ok(value)
}

pub(crate) fn validate_authority(
    authority: &AccountLifecycleAuthorityV1,
) -> Result<(), LifecycleRecordError> {
    if authority.version != SCHEMA_VERSION
        || !authority.installation_principal_initialized
        || authority.lifecycle_key_version == 0
        || authority.current_credential_etags.len() > ProductProviderId::ALL.len()
        || authority.active_operations.len() > ProductProviderId::ALL.len()
        || authority.retired_credential_etags.len() > ProductProviderId::ALL.len()
        || authority
            .current_credential_etags
            .values()
            .any(|value| validate_b64url(value, DIGEST_LEN).is_err())
        || authority.retired_credential_etags.values().any(|values| {
            values.len() > MAX_RETIRED_ETAGS
                || values
                    .iter()
                    .any(|value| validate_b64url(value, DIGEST_LEN).is_err())
        })
    {
        return Err(LifecycleRecordError::Invalid);
    }
    for operation in authority.active_operations.values() {
        validate_prepared(&operation.prepared)?;
        validate_b64url(&operation.operation_etag, 43)?;
        validate_b64url(&operation.journal_record_id, 43)?;
    }
    Ok(())
}

pub(crate) fn next_epoch(current: u64) -> Result<u64, LifecycleRecordError> {
    current
        .checked_add(1)
        .ok_or(LifecycleRecordError::EpochExhausted)
}

pub(crate) fn validate_prepared(
    prepared: &PreparedOperationV1,
) -> Result<(), LifecycleRecordError> {
    if prepared.version != SCHEMA_VERSION
        || validate_b64url(&prepared.operation_id, OPERATION_ID_LEN).is_err()
        || prepared.lifecycle_key_version == 0
        || prepared.revoke_spec_version == 0
        || prepared
            .credential_etag
            .as_ref()
            .is_some_and(|value| validate_b64url(value, DIGEST_LEN).is_err())
        || prepared
            .prior_generation
            .as_ref()
            .is_some_and(|value| !is_uuid_v4(value))
        || prepared
            .prior_subject_digest
            .as_ref()
            .is_some_and(|value| validate_b64url(value, DIGEST_LEN).is_err())
        || prepared.initial_revoke_request_target.is_some()
            != prepared.initial_revoke_scope_guarantee.is_some()
    {
        return Err(LifecycleRecordError::Invalid);
    }
    for value in [
        &prepared.request_id_hash,
        &prepared.idempotency_key,
        &prepared.payload_digest,
    ] {
        validate_b64url(value, 43)?;
    }
    Ok(())
}

pub(crate) fn validate_journal(
    journal: &AccountLifecycleJournalV1,
) -> Result<(), LifecycleRecordError> {
    if journal.version != SCHEMA_VERSION || journal.attempt_count > MAX_ATTEMPTS {
        return Err(LifecycleRecordError::Invalid);
    }
    validate_prepared(&journal.prepared)?;
    validate_b64url(&journal.lease_owner_nonce, 22)?;
    validate_b64url(&journal.operation_etag, 43)?;
    if journal.created_at_ms > journal.updated_at_ms
        || journal.attempt_count == 0 && journal.in_flight_until_ms != 0
        || journal
            .closed_code
            .as_ref()
            .is_some_and(|value| !valid_closed_code(value))
        || journal.revoke_request_target.is_some() != journal.revoke_scope_guarantee.is_some()
        || journal.revoke_leg == 0
            && (journal.revoked_grant.is_some()
                || journal.revoke_request_target.is_some()
                || journal.revoke_scope_guarantee.is_some())
        || journal.revoke_leg > 0
            && (journal.revoked_grant.is_none()
                || journal.revoke_request_target.is_none()
                || journal.revoke_scope_guarantee.is_none())
    {
        return Err(LifecycleRecordError::Invalid);
    }
    if let Some(grant) = &journal.revoked_grant {
        match grant {
            RevokedGrantRef::ActiveCredential { generation } if !is_uuid_v4(generation) => {
                return Err(LifecycleRecordError::Invalid);
            }
            RevokedGrantRef::OAuthCandidate { record_id }
                if validate_b64url(record_id, DIGEST_LEN).is_err() =>
            {
                return Err(LifecycleRecordError::Invalid);
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn validate_exchange_intent(
    intent: &OAuthExchangeIntentV1,
) -> Result<(), LifecycleRecordError> {
    if intent.version != SCHEMA_VERSION
        || validate_b64url(&intent.operation_id, OPERATION_ID_LEN).is_err()
        || validate_b64url(&intent.attempt_id, DIGEST_LEN).is_err()
        || intent.created_at_ms >= intent.expires_at_ms
    {
        return Err(LifecycleRecordError::Invalid);
    }
    Ok(())
}

pub(crate) fn validate_candidate(
    candidate: &OAuthCandidateV1,
    expected_record_id: &str,
) -> Result<(), LifecycleRecordError> {
    if candidate.version != SCHEMA_VERSION
        || candidate.record_id != expected_record_id
        || validate_b64url(&candidate.record_id, DIGEST_LEN).is_err()
        || validate_b64url(&candidate.operation_id, OPERATION_ID_LEN).is_err()
        || candidate.access_token.is_empty()
        || candidate.access_token.len() > MAX_TOKEN_BYTES
        || candidate.refresh_token.is_empty()
        || candidate.refresh_token.len() > MAX_TOKEN_BYTES
        || candidate
            .provider_account_id
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.len() > MAX_PROVIDER_ACCOUNT_ID_BYTES)
        || candidate
            .subject_digest
            .as_ref()
            .is_some_and(|value| validate_b64url(value, DIGEST_LEN).is_err())
        || candidate
            .session_id_digest
            .as_ref()
            .is_some_and(|value| validate_b64url(value, DIGEST_LEN).is_err())
        || candidate.created_at_ms > candidate.expires_at_ms
        || matches!(
            candidate.state,
            OAuthCandidateState::GrantBearing | OAuthCandidateState::RevokeUnknown
        ) && candidate.terminal_at_ms.is_some()
        || matches!(
            candidate.state,
            OAuthCandidateState::RevokedCleaned | OAuthCandidateState::Promoted
        ) && candidate.terminal_at_ms.is_none()
    {
        return Err(LifecycleRecordError::Invalid);
    }
    Ok(())
}

pub(crate) fn validate_receipts(
    index: &AccountLifecycleReceiptIndexV1,
) -> Result<(), LifecycleRecordError> {
    if index.version != SCHEMA_VERSION || index.receipts.len() > MAX_RECEIPTS {
        return Err(LifecycleRecordError::CountLimit);
    }
    for receipt in &index.receipts {
        if receipt.version != SCHEMA_VERSION
            || validate_b64url(&receipt.operation_id, OPERATION_ID_LEN).is_err()
            || validate_b64url(&receipt.operation_etag, DIGEST_LEN).is_err()
            || validate_b64url(&receipt.idempotency_key, DIGEST_LEN).is_err()
            || validate_b64url(&receipt.payload_digest, DIGEST_LEN).is_err()
            || receipt
                .prior_generation
                .as_ref()
                .is_some_and(|value| !is_uuid_v4(value))
            || receipt
                .result_generation
                .as_ref()
                .is_some_and(|value| !is_uuid_v4(value))
            || receipt.lifecycle_key_version == 0
            || !valid_closed_code(&receipt.terminal_code)
            || serde_json::to_vec(receipt)
                .map_err(|_| LifecycleRecordError::Invalid)?
                .len()
                > MAX_RECEIPT_BYTES
        {
            return Err(LifecycleRecordError::Invalid);
        }
    }
    Ok(())
}

pub(crate) fn transition_allowed(
    mode: AccountLifecycleMode,
    from: AccountLifecyclePhase,
    to: AccountLifecyclePhase,
) -> bool {
    use AccountLifecycleMode::{Connect, Disconnect, Reconnect, Switch};
    use AccountLifecyclePhase::*;
    if matches!(from, RevokeInFlight) && matches!(to, RevokeOutcomeUnknown) {
        return true;
    }
    if matches!(from, RevokeOutcomeUnknown) && matches!(to, RevokeInFlight) {
        return true;
    }
    if matches!(from, ExchangeInFlight) && matches!(to, ExchangeOutcomeUnknown) {
        return true;
    }
    if matches!(from, CandidateValidation) && matches!(to, OAuthCandidateCleanup) {
        return true;
    }
    if matches!(from, OAuthCandidateCleanup) && matches!(to, RevokeInFlight) {
        return true;
    }
    match (mode, from, to) {
        (Connect, Prepared, AwaitingOAuthLogin)
        | (Connect, AwaitingOAuthLogin, ExchangeInFlight)
        | (Connect, ExchangeInFlight, OAuthCandidateStored)
        | (Connect, OAuthCandidateStored, CandidateValidation)
        | (Connect, CandidateValidation, Completed)
        | (Disconnect | Reconnect | Switch, Prepared, RevokeInFlight)
        | (Disconnect | Reconnect | Switch, RevokeInFlight, RevokedPendingCleanup)
        | (Disconnect | Reconnect | Switch, RevokedPendingCleanup, Disconnected)
        | (Disconnect, Disconnected, Completed)
        | (Reconnect | Switch, Disconnected, AwaitingOAuthLogin)
        | (Reconnect | Switch, AwaitingOAuthLogin, ExchangeInFlight)
        | (Reconnect | Switch, ExchangeInFlight, OAuthCandidateStored)
        | (Reconnect | Switch, OAuthCandidateStored, CandidateValidation)
        | (Reconnect | Switch, CandidateValidation, Completed) => true,
        (_, RevokedPendingCleanup, FailedTerminal) | (_, OAuthCandidateCleanup, FailedTerminal) => {
            false
        }
        _ => false,
    }
}

pub(crate) fn is_uuid_v4(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes[8] == b'-'
        && bytes[13] == b'-'
        && bytes[18] == b'-'
        && bytes[23] == b'-'
        && bytes[14] == b'4'
        && matches!(bytes[19], b'8' | b'9' | b'a' | b'b')
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 8 | 13 | 18 | 23)
                || byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()
        })
}

pub(crate) fn validate_request_id(value: &str) -> Result<(), LifecycleRecordError> {
    if is_uuid_v4(value) {
        Ok(())
    } else {
        Err(LifecycleRecordError::Invalid)
    }
}

pub(crate) fn validate_operation_id(value: &str) -> Result<(), LifecycleRecordError> {
    validate_b64url(value, OPERATION_ID_LEN)
}

fn valid_closed_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_CLOSED_CODE_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn revoke_target_wire(value: RevokeRequestTarget) -> &'static str {
    match value {
        RevokeRequestTarget::RefreshToken => "refresh_token",
        RevokeRequestTarget::AccessToken => "access_token",
    }
}

fn revoke_scope_wire(value: RevokeScopeGuarantee) -> &'static str {
    match value {
        RevokeScopeGuarantee::GuaranteedTokenSession => "guaranteed_token_session",
        RevokeScopeGuarantee::ObservedTokenSession => "observed_token_session",
        RevokeScopeGuarantee::Unknown => "unknown",
        RevokeScopeGuarantee::FullGrant => "full_grant",
    }
}

fn validate_b64url(value: &str, exact_len: usize) -> Result<(), LifecycleRecordError> {
    if value.len() != exact_len
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(LifecycleRecordError::Invalid);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "isyncyou-645-{label}-{}-{}",
            std::process::id(),
            random_b64url(8).unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn repository(root: &Path) -> LifecycleRepository {
        let config = isyncyou_agent::CredentialStoreConfig::new(root);
        let store = isyncyou_agent::CredentialStoreResolver::new(config.clone())
            .with_provided_key([42u8; 32])
            .resolve()
            .unwrap();
        LifecycleRepository::new(
            store,
            config.store_dir(),
            root.join("account-lifecycle.lock"),
        )
    }

    fn prepared(mode: AccountLifecycleMode) -> PreparedOperationV1 {
        PreparedOperationV1 {
            version: 1,
            provider: ProductProviderId::Codex,
            operation_id: "O".repeat(OPERATION_ID_LEN),
            route: AccountLifecycleRoute::Logout,
            request_id_hash: "a".repeat(43),
            idempotency_key: "b".repeat(43),
            payload_digest: "c".repeat(43),
            mode,
            lifecycle_epoch: 1,
            fence_epoch: 1,
            lifecycle_key_version: 1,
            credential_etag: Some("d".repeat(43)),
            prior_generation: Some("123e4567-e89b-42d3-a456-426614174001".into()),
            prior_subject_digest: Some("e".repeat(43)),
            revoke_spec_version: 1,
            initial_revoke_request_target: Some(RevokeRequestTarget::RefreshToken),
            initial_revoke_scope_guarantee: Some(RevokeScopeGuarantee::ObservedTokenSession),
            prepared_at_ms: 1,
        }
    }

    fn journal(mode: AccountLifecycleMode) -> AccountLifecycleJournalV1 {
        AccountLifecycleJournalV1 {
            version: 1,
            prepared: prepared(mode),
            lease_owner_nonce: "n".repeat(22),
            operation_etag: "o".repeat(43),
            phase: AccountLifecyclePhase::Prepared,
            revoke_leg: 0,
            revoked_grant: None,
            revoke_request_target: None,
            revoke_scope_guarantee: None,
            attempt_count: 0,
            in_flight_until_ms: 0,
            created_at_ms: 1,
            updated_at_ms: 1,
            closed_code: None,
        }
    }

    fn candidate(state: OAuthCandidateState) -> OAuthCandidateV1 {
        OAuthCandidateV1 {
            version: 1,
            provider: ProductProviderId::Codex,
            operation_id: "O".repeat(OPERATION_ID_LEN),
            record_id: "r".repeat(43),
            access_token: "private-access-sentinel".into(),
            refresh_token: "private-refresh-sentinel".into(),
            expires_at_ms: 2_000,
            provider_account_id: Some("private-account-sentinel".into()),
            subject_digest: Some("s".repeat(43)),
            session_id_digest: None,
            state,
            created_at_ms: 1,
            terminal_at_ms: matches!(
                state,
                OAuthCandidateState::RevokedCleaned | OAuthCandidateState::Promoted
            )
            .then_some(1_000),
        }
    }

    fn begin_disconnect_fixture(
        repository: &LifecycleRepository,
        request_id: &str,
    ) -> BeginLifecycleOperation {
        repository
            .begin_disconnect(
                ProductProviderId::Codex,
                request_id,
                "123e4567-e89b-42d3-a456-426614174001",
                Some(&"s".repeat(43)),
                RevokeRequestTarget::RefreshToken,
                RevokeScopeGuarantee::ObservedTokenSession,
                1_000,
            )
            .unwrap()
    }

    fn assert_prepared_blocks_provider(repository: &LifecycleRepository) {
        assert!(repository
            .provider_blocked(ProductProviderId::Codex)
            .unwrap());
    }

    fn advance_to_revoke_in_flight(
        repository: &LifecycleRepository,
        operation: &BeginLifecycleOperation,
    ) -> AccountLifecycleJournalV1 {
        repository
            .start_active_revoke(&operation.journal_record_id, 2_000)
            .unwrap()
    }

    fn advance_to_cleanup(
        repository: &LifecycleRepository,
        operation: &BeginLifecycleOperation,
    ) -> AccountLifecycleJournalV1 {
        advance_to_revoke_in_flight(repository, operation);
        repository
            .publish_revoke_outcome(
                &operation.journal_record_id,
                true,
                "revoke_confirmed",
                3_000,
            )
            .unwrap()
    }

    #[test]
    fn lifecycle_epoch_overflow_fails_closed() {
        assert_eq!(
            next_epoch(u64::MAX),
            Err(LifecycleRecordError::EpochExhausted)
        );
    }

    #[test]
    fn lifecycle_ids_require_uuidv4_and_full_csprng_operation_nonce_entropy() {
        assert!(validate_request_id("123e4567-e89b-42d3-a456-426614174000").is_ok());
        assert_eq!(
            validate_request_id("123e4567-e89b-12d3-a456-426614174000"),
            Err(LifecycleRecordError::Invalid)
        );
        assert!(validate_operation_id(&"A".repeat(OPERATION_ID_LEN)).is_ok());
        assert_eq!(
            validate_operation_id(&"A".repeat(OPERATION_ID_LEN - 1)),
            Err(LifecycleRecordError::Invalid)
        );
        assert_eq!(mint_operation_id().unwrap().len(), OPERATION_ID_LEN);
        let mut invalid = prepared(AccountLifecycleMode::Disconnect);
        invalid.operation_id = "not-an-operation-id".into();
        assert_eq!(
            validate_prepared(&invalid),
            Err(LifecycleRecordError::Invalid)
        );
    }

    #[test]
    fn lifecycle_mode_phase_transition_table_rejects_every_unlisted_edge() {
        let modes = [
            AccountLifecycleMode::Connect,
            AccountLifecycleMode::Disconnect,
            AccountLifecycleMode::Reconnect,
            AccountLifecycleMode::Switch,
        ];
        let phases = [
            AccountLifecyclePhase::Prepared,
            AccountLifecyclePhase::RevokeInFlight,
            AccountLifecyclePhase::RevokeOutcomeUnknown,
            AccountLifecyclePhase::RevokedPendingCleanup,
            AccountLifecyclePhase::Disconnected,
            AccountLifecyclePhase::AwaitingOAuthLogin,
            AccountLifecyclePhase::ExchangeInFlight,
            AccountLifecyclePhase::ExchangeOutcomeUnknown,
            AccountLifecyclePhase::OAuthCandidateStored,
            AccountLifecyclePhase::CandidateValidation,
            AccountLifecyclePhase::OAuthCandidateCleanup,
            AccountLifecyclePhase::Completed,
            AccountLifecyclePhase::FailedTerminal,
        ];
        for mode in modes {
            for from in phases {
                for to in phases {
                    if transition_allowed(mode, from, to) {
                        assert_ne!(from, to, "self transitions are never listed");
                    }
                }
            }
        }
        assert!(!transition_allowed(
            AccountLifecycleMode::Connect,
            AccountLifecyclePhase::Prepared,
            AccountLifecyclePhase::Completed
        ));
    }

    #[test]
    fn reconnect_and_switch_track_active_and_candidate_revoke_as_distinct_legs() {
        let mut journal = AccountLifecycleJournalV1 {
            version: 1,
            prepared: prepared(AccountLifecycleMode::Reconnect),
            lease_owner_nonce: "n".repeat(22),
            operation_etag: "o".repeat(43),
            phase: AccountLifecyclePhase::RevokeInFlight,
            revoke_leg: 1,
            revoked_grant: Some(RevokedGrantRef::ActiveCredential {
                generation: "123e4567-e89b-42d3-a456-426614174001".into(),
            }),
            revoke_request_target: Some(RevokeRequestTarget::RefreshToken),
            revoke_scope_guarantee: Some(RevokeScopeGuarantee::ObservedTokenSession),
            attempt_count: 1,
            in_flight_until_ms: 2,
            created_at_ms: 1,
            updated_at_ms: 1,
            closed_code: None,
        };
        assert!(validate_journal(&journal).is_ok());
        journal.revoke_leg = 2;
        journal.revoked_grant = Some(RevokedGrantRef::OAuthCandidate {
            record_id: "r".repeat(43),
        });
        assert!(validate_journal(&journal).is_ok());
    }

    #[test]
    fn account_lifecycle_journal_is_encrypted_bounded_and_owner_only() {
        let root = temp_root("journal");
        let repository = repository(&root);
        repository
            .put_journal(&"j".repeat(43), &journal(AccountLifecycleMode::Disconnect))
            .unwrap();
        let path = std::fs::read_dir(root.join("agent-credentials"))
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("account-lifecycle-journal__")
            })
            .unwrap();
        let raw = std::fs::read(&path).unwrap();
        assert!(raw.len() <= JOURNAL_ENVELOPE_MAX);
        assert!(!String::from_utf8_lossy(&raw).contains("123e4567"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn account_lifecycle_candidate_uses_distinct_secret_class_aad() {
        let root = temp_root("candidate-aad");
        let repository = repository(&root);
        let id = "r".repeat(43);
        repository
            .put_candidate(&id, &candidate(OAuthCandidateState::GrantBearing))
            .unwrap();
        repository
            .put_journal(&id, &journal(AccountLifecycleMode::Connect))
            .unwrap();
        let dir = root.join("agent-credentials");
        let mut candidate_path = None;
        let mut journal_path = None;
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("oauth-candidate__") {
                candidate_path = Some(entry.path());
            }
            if name.starts_with("account-lifecycle-journal__") {
                journal_path = Some(entry.path());
            }
        }
        std::fs::copy(candidate_path.unwrap(), journal_path.unwrap()).unwrap();
        assert!(repository
            .store
            .get_bounded(
                isyncyou_agent::SecretClass::AccountLifecycleJournal,
                &id,
                JOURNAL_ENVELOPE_MAX,
                JOURNAL_PLAINTEXT_MAX
            )
            .is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn installation_principal_survives_webview_restart_and_is_never_public() {
        let root = temp_root("principal-restart");
        let first = repository(&root).initialize().unwrap();
        let second = repository(&root).initialize().unwrap();
        assert_eq!(first.principal(), second.principal());
        assert_eq!(first.principal().len(), 22);
        assert!(!format!("{first:?}").contains(first.principal()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parallel_first_lifecycle_migration_creates_exactly_one_installation_principal() {
        let root = temp_root("principal-parallel");
        let mut threads = Vec::new();
        for _ in 0..8 {
            let root = root.clone();
            threads.push(std::thread::spawn(move || loop {
                match repository(&root).initialize() {
                    Ok(context) => break context.principal().to_string(),
                    Err(LifecycleRecordError::Busy) => std::thread::yield_now(),
                    Err(error) => panic!("unexpected initialization error: {error:?}"),
                }
            }));
        }
        let principals: std::collections::BTreeSet<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(principals.len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn initialized_authority_with_missing_principal_fails_closed() {
        let root = temp_root("missing-principal");
        let repo = repository(&root);
        repo.initialize().unwrap();
        repo.store
            .delete(
                isyncyou_agent::SecretClass::AccountLifecycleInstallation,
                "installation",
            )
            .unwrap();
        assert!(matches!(
            repository(&root).initialize(),
            Err(LifecycleRecordError::MissingInstallationPrincipal)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lifecycle_key_rotation_retains_live_receipt_authority_until_safe_retirement() {
        let context = AccountLifecycleAuthorityV1 {
            version: 1,
            installation_principal_initialized: true,
            lifecycle_epoch: 2,
            fence_epoch: 2,
            lifecycle_key_version: 2,
            current_credential_etags: BTreeMap::from([(ProductProviderId::Codex, "c".repeat(43))]),
            retired_credential_etags: BTreeMap::from([(
                ProductProviderId::Codex,
                vec!["r".repeat(43)],
            )]),
            active_operations: BTreeMap::new(),
        };
        assert!(validate_authority(&context).is_ok());
        assert_eq!(context.lifecycle_key_version, 2);
        assert_eq!(
            context.retired_credential_etags[&ProductProviderId::Codex].len(),
            1
        );
    }

    #[test]
    fn lifecycle_authority_persists_idempotency_fence_key_and_etag_authority_before_operation() {
        let prepared = prepared(AccountLifecycleMode::Disconnect);
        let authority = AccountLifecycleAuthorityV1 {
            version: 1,
            installation_principal_initialized: true,
            lifecycle_epoch: prepared.lifecycle_epoch,
            fence_epoch: prepared.fence_epoch,
            lifecycle_key_version: prepared.lifecycle_key_version,
            current_credential_etags: BTreeMap::from([(
                ProductProviderId::Codex,
                prepared.credential_etag.clone().unwrap(),
            )]),
            retired_credential_etags: BTreeMap::new(),
            active_operations: BTreeMap::from([(
                ProductProviderId::Codex,
                ActiveOperationRefV1 {
                    prepared: prepared.clone(),
                    operation_etag: "o".repeat(43),
                    journal_record_id: "j".repeat(43),
                },
            )]),
        };
        assert!(validate_authority(&authority).is_ok());
        let embedded = &authority.active_operations[&ProductProviderId::Codex].prepared;
        assert_eq!(embedded.idempotency_key, prepared.idempotency_key);
        assert_eq!(embedded.payload_digest, prepared.payload_digest);
        assert_eq!(embedded.fence_epoch, authority.fence_epoch);
    }

    #[test]
    fn lifecycle_records_enforce_concrete_id_count_and_byte_limits() {
        let mut authority = AccountLifecycleAuthorityV1 {
            version: 1,
            installation_principal_initialized: true,
            lifecycle_epoch: 1,
            fence_epoch: 1,
            lifecycle_key_version: 1,
            current_credential_etags: BTreeMap::new(),
            retired_credential_etags: BTreeMap::from([(
                ProductProviderId::Codex,
                vec!["x".repeat(43); 65],
            )]),
            active_operations: BTreeMap::new(),
        };
        assert_eq!(
            validate_authority(&authority),
            Err(LifecycleRecordError::Invalid)
        );
        authority.retired_credential_etags.clear();
        assert!(validate_authority(&authority).is_ok());
        assert_eq!(
            serialize_bounded(
                &"x".repeat(JOURNAL_PLAINTEXT_MAX + 1),
                JOURNAL_PLAINTEXT_MAX
            ),
            Err(LifecycleRecordError::SizeLimit)
        );
        let exchange = OAuthExchangeIntentV1 {
            version: 1,
            provider: ProductProviderId::Codex,
            operation_id: "O".repeat(OPERATION_ID_LEN),
            attempt_id: "A".repeat(DIGEST_LEN),
            created_at_ms: 1,
            expires_at_ms: 2,
        };
        assert!(validate_exchange_intent(&exchange).is_ok());
        let mut invalid_candidate = candidate(OAuthCandidateState::GrantBearing);
        invalid_candidate.access_token = "x".repeat(MAX_TOKEN_BYTES + 1);
        assert_eq!(
            validate_candidate(&invalid_candidate, &invalid_candidate.record_id),
            Err(LifecycleRecordError::Invalid)
        );
    }

    #[test]
    fn account_lifecycle_reaper_removes_expired_terminal_records_and_candidates() {
        let mut value = candidate(OAuthCandidateState::RevokedCleaned);
        value.terminal_at_ms = Some(1);
        assert!(candidate_reapable(&value, 31 * 24 * 60 * 60 * 1_000));
        value.state = OAuthCandidateState::Promoted;
        assert!(candidate_reapable(&value, 31 * 24 * 60 * 60 * 1_000));
    }

    #[test]
    fn prepared_journal_blocks_provider_before_disconnecting_bundle_write() {
        let root = temp_root("prepared-blocks");
        let repo = repository(&root);
        let operation = begin_disconnect_fixture(&repo, "123e4567-e89b-42d3-a456-426614174010");
        assert_prepared_blocks_provider(&repository(&root));
        assert_eq!(
            repository(&root)
                .load_journal(&operation.journal_record_id)
                .unwrap()
                .unwrap()
                .phase,
            AccountLifecyclePhase::Prepared
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_process_fence_cannot_publish_readiness_or_cleanup() {
        let root = temp_root("stale-fence");
        let repo = repository(&root);
        let operation = begin_disconnect_fixture(&repo, "123e4567-e89b-42d3-a456-426614174011");
        let mut stale = advance_to_revoke_in_flight(&repo, &operation);
        let mut context = repo.load_existing().unwrap().unwrap();
        context.authority.fence_epoch += 1;
        repo.put_authority(context.principal(), &context.authority)
            .unwrap();
        stale.phase = AccountLifecyclePhase::RevokedPendingCleanup;
        assert_eq!(
            repo.write_journal_fenced(&operation.journal_record_id, &stale),
            Err(LifecycleRecordError::StaleFence)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_after_each_disconnect_publication_window_is_fail_closed_and_idempotent() {
        let root = temp_root("recovery-windows");
        let repo = repository(&root);
        let operation = begin_disconnect_fixture(&repo, "123e4567-e89b-42d3-a456-426614174012");
        for expected in [
            AccountLifecyclePhase::Prepared,
            AccountLifecyclePhase::RevokeInFlight,
            AccountLifecyclePhase::RevokedPendingCleanup,
        ] {
            let reopened = repository(&root);
            assert_prepared_blocks_provider(&reopened);
            assert_eq!(
                reopened
                    .load_journal(&operation.journal_record_id)
                    .unwrap()
                    .unwrap()
                    .phase,
                expected
            );
            match expected {
                AccountLifecyclePhase::Prepared => {
                    advance_to_revoke_in_flight(&reopened, &operation);
                }
                AccountLifecyclePhase::RevokeInFlight => {
                    reopened
                        .publish_revoke_outcome(
                            &operation.journal_record_id,
                            true,
                            "revoke_confirmed",
                            3_000,
                        )
                        .unwrap();
                }
                AccountLifecyclePhase::RevokedPendingCleanup => break,
                _ => unreachable!(),
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn crash_after_authority_reservation_before_journal_blocks_provider_and_recovers() {
        let root = temp_root("authority-before-journal");
        let repo = repository(&root);
        let operation = begin_disconnect_fixture(&repo, "123e4567-e89b-42d3-a456-426614174013");
        repo.delete_journal(&operation.journal_record_id).unwrap();
        let reopened = repository(&root);
        assert_prepared_blocks_provider(&reopened);
        let context = reopened.load_existing().unwrap().unwrap();
        let active = &context.authority.active_operations[&ProductProviderId::Codex];
        assert_eq!(active.prepared.operation_id, operation.operation_id);
        assert_eq!(active.journal_record_id, operation.journal_record_id);
        let recovered = reopened
            .recover_active_journal(ProductProviderId::Codex, 2_000)
            .unwrap()
            .unwrap();
        assert_eq!(recovered.phase, AccountLifecyclePhase::Prepared);
        assert_eq!(recovered.prepared, active.prepared);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn crash_after_prepared_before_bundle_write_blocks_provider_and_resumes() {
        let root = temp_root("prepared-resume");
        let repo = repository(&root);
        let operation = begin_disconnect_fixture(&repo, "123e4567-e89b-42d3-a456-426614174014");
        let reopened = repository(&root);
        assert_prepared_blocks_provider(&reopened);
        assert_eq!(
            reopened
                .start_active_revoke(&operation.journal_record_id, 2_000)
                .unwrap()
                .phase,
            AccountLifecyclePhase::RevokeInFlight
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn crash_after_disconnecting_before_revoke_claim_resumes() {
        let root = temp_root("disconnecting-resume");
        let repo = repository(&root);
        let operation = begin_disconnect_fixture(&repo, "123e4567-e89b-42d3-a456-426614174015");
        assert_eq!(
            repository(&root)
                .start_active_revoke(&operation.journal_record_id, 2_000)
                .unwrap()
                .attempt_count,
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn crash_after_revoke_inflight_before_response_stays_outcome_unknown() {
        let root = temp_root("revoke-inflight");
        let repo = repository(&root);
        let operation = begin_disconnect_fixture(&repo, "123e4567-e89b-42d3-a456-426614174016");
        advance_to_revoke_in_flight(&repo, &operation);
        let unknown = repo
            .publish_revoke_outcome(
                &operation.journal_record_id,
                false,
                "revoke_interrupted",
                3_000,
            )
            .unwrap();
        assert_eq!(unknown.phase, AccountLifecyclePhase::RevokeOutcomeUnknown);
        assert_prepared_blocks_provider(&repository(&root));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn crash_after_provider_2xx_before_outcome_commit_retries_without_cleanup() {
        let root = temp_root("provider-2xx-window");
        let repo = repository(&root);
        let operation = begin_disconnect_fixture(&repo, "123e4567-e89b-42d3-a456-426614174017");
        advance_to_revoke_in_flight(&repo, &operation);
        let reopened = repository(&root);
        assert_eq!(
            reopened
                .load_journal(&operation.journal_record_id)
                .unwrap()
                .unwrap()
                .phase,
            AccountLifecyclePhase::RevokeInFlight
        );
        assert!(reopened
            .start_active_revoke(&operation.journal_record_id, 4_000)
            .is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn crash_after_revoked_pending_cleanup_resumes_without_provider_call() {
        let root = temp_root("cleanup-resume");
        let repo = repository(&root);
        let operation = begin_disconnect_fixture(&repo, "123e4567-e89b-42d3-a456-426614174018");
        advance_to_cleanup(&repo, &operation);
        let receipt = repository(&root)
            .complete_disconnect(&operation.journal_record_id, 4_000)
            .unwrap();
        assert_eq!(receipt.terminal_code, "disconnected");
        assert!(!repository(&root)
            .provider_blocked(ProductProviderId::Codex)
            .unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn crash_after_all_deletes_before_receipt_writes_terminal_receipt() {
        let root = temp_root("all-deletes-before-receipt");
        let repo = repository(&root);
        let operation = begin_disconnect_fixture(&repo, "123e4567-e89b-42d3-a456-426614174021");
        advance_to_cleanup(&repo, &operation);
        let receipt = repository(&root)
            .complete_disconnect(&operation.journal_record_id, 4_000)
            .unwrap();
        assert_eq!(receipt.operation_id, operation.operation_id);
        assert_eq!(receipt.terminal_code, "disconnected");
        assert!(!repository(&root)
            .provider_blocked(ProductProviderId::Codex)
            .unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn crash_after_receipt_before_authority_release_retires_etag_once() {
        let root = temp_root("receipt-before-authority-release");
        let repo = repository(&root);
        let operation = begin_disconnect_fixture(&repo, "123e4567-e89b-42d3-a456-426614174022");
        let mut journal = advance_to_cleanup(&repo, &operation);
        journal.phase = AccountLifecyclePhase::Disconnected;
        journal.updated_at_ms = 4_000;
        journal.closed_code = Some("disconnected".into());
        repo.write_journal_fenced(&operation.journal_record_id, &journal)
            .unwrap();
        let context = repo.load_existing().unwrap().unwrap();
        let receipt = disconnect_receipt(&journal, 4_000);
        let receipt_id = repo
            .receipt_index_id(context.principal(), ProductProviderId::Codex)
            .unwrap();
        repo.put_receipt_index(
            &receipt_id,
            &AccountLifecycleReceiptIndexV1 {
                version: SCHEMA_VERSION,
                provider: ProductProviderId::Codex,
                lifecycle_epoch: journal.prepared.lifecycle_epoch,
                receipts: vec![receipt],
            },
        )
        .unwrap();

        repository(&root)
            .complete_disconnect(&operation.journal_record_id, 5_000)
            .unwrap();
        let authority = repository(&root)
            .load_existing()
            .unwrap()
            .unwrap()
            .authority;
        assert_eq!(
            authority.retired_credential_etags[&ProductProviderId::Codex].len(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn crash_after_receipt_before_compaction_compacts_idempotently() {
        let root = temp_root("receipt-before-compaction");
        let repo = repository(&root);
        let operation = begin_disconnect_fixture(&repo, "123e4567-e89b-42d3-a456-426614174023");
        let mut disconnected = advance_to_cleanup(&repo, &operation);
        disconnected.phase = AccountLifecyclePhase::Disconnected;
        disconnected.updated_at_ms = 4_000;
        disconnected.closed_code = Some("disconnected".into());
        repo.write_journal_fenced(&operation.journal_record_id, &disconnected)
            .unwrap();
        repo.complete_disconnect(&operation.journal_record_id, 5_000)
            .unwrap();

        // Simulate an already-published terminal journal surviving a crash after authority
        // retirement. The existing receipt is authoritative and no duplicate is appended.
        repository(&root)
            .put_journal(&operation.journal_record_id, &disconnected)
            .unwrap();
        repository(&root)
            .complete_disconnect(&operation.journal_record_id, 6_000)
            .unwrap();
        let context = repository(&root).load_existing().unwrap().unwrap();
        let receipt_id = repo
            .receipt_index_id(context.principal(), ProductProviderId::Codex)
            .unwrap();
        assert_eq!(
            repository(&root)
                .load_receipt_index(&receipt_id)
                .unwrap()
                .unwrap()
                .receipts
                .len(),
            1
        );
        assert!(repository(&root)
            .load_journal(&operation.journal_record_id)
            .unwrap()
            .is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disconnect_repeated_request_is_idempotent() {
        let root = temp_root("idempotent-active");
        let repo = repository(&root);
        let request_id = "123e4567-e89b-42d3-a456-426614174019";
        let first = begin_disconnect_fixture(&repo, request_id);
        let second = begin_disconnect_fixture(&repository(&root), request_id);
        assert!(second.idempotent_replay);
        assert_eq!(first.operation_id, second.operation_id);
        assert_eq!(first.operation_etag, second.operation_etag);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disconnect_same_request_id_with_different_payload_conflicts() {
        let root = temp_root("idempotency-conflict");
        let repo = repository(&root);
        let request_id = "123e4567-e89b-42d3-a456-426614174020";
        begin_disconnect_fixture(&repo, request_id);
        assert_eq!(
            repo.begin_disconnect(
                ProductProviderId::Codex,
                request_id,
                "123e4567-e89b-42d3-a456-426614174099",
                Some(&"s".repeat(43)),
                RevokeRequestTarget::RefreshToken,
                RevokeScopeGuarantee::ObservedTokenSession,
                2_000,
            ),
            Err(LifecycleRecordError::IdempotencyConflict)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disconnect_idempotency_survives_webview_session_restart() {
        let root = temp_root("idempotency-restart");
        let request_id = "123e4567-e89b-42d3-a456-426614174021";
        let first = begin_disconnect_fixture(&repository(&root), request_id);
        let replay = begin_disconnect_fixture(&repository(&root), request_id);
        assert!(replay.idempotent_replay);
        assert_eq!(first.operation_id, replay.operation_id);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_or_corrupt_installation_principal_fails_closed_without_new_operation_identity() {
        let root = temp_root("corrupt-principal-operation");
        let repo = repository(&root);
        repo.initialize().unwrap();
        repo.store
            .delete_durable(
                isyncyou_agent::SecretClass::AccountLifecycleInstallation,
                "installation",
            )
            .unwrap();
        assert!(matches!(
            repository(&root).begin_disconnect(
                ProductProviderId::Codex,
                "123e4567-e89b-42d3-a456-426614174022",
                "123e4567-e89b-42d3-a456-426614174001",
                None,
                RevokeRequestTarget::RefreshToken,
                RevokeScopeGuarantee::ObservedTokenSession,
                1_000,
            ),
            Err(LifecycleRecordError::MissingInstallationPrincipal)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn account_lifecycle_status_is_read_only_and_never_runs_maintenance() {
        let root = temp_root("status-read-only");
        let repo = repository(&root);
        let operation = begin_disconnect_fixture(&repo, "123e4567-e89b-42d3-a456-426614174023");
        let before = std::fs::read_dir(root.join("agent-credentials"))
            .unwrap()
            .count();
        assert!(repo.provider_blocked(ProductProviderId::Codex).unwrap());
        assert_eq!(
            repo.load_journal(&operation.journal_record_id)
                .unwrap()
                .unwrap()
                .phase,
            AccountLifecyclePhase::Prepared
        );
        assert_eq!(
            before,
            std::fs::read_dir(root.join("agent-credentials"))
                .unwrap()
                .count()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn account_lifecycle_maintenance_never_resolves_revoke_unknown_by_time() {
        let root = temp_root("unknown-retained");
        let repo = repository(&root);
        let operation = begin_disconnect_fixture(&repo, "123e4567-e89b-42d3-a456-426614174024");
        advance_to_revoke_in_flight(&repo, &operation);
        repo.publish_revoke_outcome(&operation.journal_record_id, false, "revoke_timeout", 3_000)
            .unwrap();
        assert_eq!(
            repository(&root)
                .load_journal(&operation.journal_record_id)
                .unwrap()
                .unwrap()
                .phase,
            AccountLifecyclePhase::RevokeOutcomeUnknown
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn account_lifecycle_maintenance_never_resolves_or_reaps_exchange_unknown() {
        let mut value = journal(AccountLifecycleMode::Connect);
        value.phase = AccountLifecyclePhase::ExchangeOutcomeUnknown;
        value.updated_at_ms = 1;
        value.in_flight_until_ms = 0;
        assert!(validate_journal(&value).is_ok());
        assert!(!transition_allowed(
            AccountLifecycleMode::Connect,
            AccountLifecyclePhase::ExchangeOutcomeUnknown,
            AccountLifecyclePhase::Completed
        ));
    }

    #[test]
    fn provider_lifecycle_file_lock_excludes_real_child_process() {
        const CHILD_ENV: &str = "ISY645_LOCK_CHILD";
        if let Ok(path) = std::env::var(CHILD_ENV) {
            assert!(
                isyncyou_agent::FileLock::try_acquire_exclusive(Path::new(&path))
                    .unwrap()
                    .is_none()
            );
            return;
        }
        let root = temp_root("child-lock");
        let path = provider_lock_path(&root, ProductProviderId::Codex);
        let _held = isyncyou_agent::FileLock::try_acquire_shared(&path)
            .unwrap()
            .unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("account_lifecycle::tests::provider_lifecycle_file_lock_excludes_real_child_process")
            .arg("--nocapture")
            .env(CHILD_ENV, &path)
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disconnect_cross_process_lease_prevents_parallel_revoke_attempts() {
        let root = temp_root("cross-process-contract");
        let registry = Arc::new(ProviderLeaseRegistry::default());
        let operation = mint_operation_id().unwrap();
        let _lease = registry
            .acquire_exclusive(
                &root,
                ProductProviderId::Codex,
                operation,
                ProviderOperationKind::Lifecycle,
            )
            .unwrap();
        assert!(matches!(
            registry.acquire_exclusive(
                &root,
                ProductProviderId::Codex,
                mint_operation_id().unwrap(),
                ProviderOperationKind::Lifecycle,
            ),
            Err(LifecycleRecordError::Busy)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn account_lifecycle_audit_failure_increments_only_bounded_diagnostics_counter() {
        let diagnostics = LifecycleDiagnostics::default();
        diagnostics.record_audit_failure();
        diagnostics.record_audit_failure();
        assert_eq!(diagnostics.audit_failures(), 2);
        diagnostics
            .audit_failures
            .store(u32::MAX, std::sync::atomic::Ordering::Release);
        diagnostics.record_audit_failure();
        assert_eq!(diagnostics.audit_failures(), u32::MAX);
        assert!(!format!("{diagnostics:?}").contains("token"));
    }

    #[test]
    fn reaper_never_removes_grant_bearing_or_revoke_unknown_candidate() {
        for state in [
            OAuthCandidateState::GrantBearing,
            OAuthCandidateState::RevokeUnknown,
        ] {
            let mut value = candidate(state);
            value.terminal_at_ms = Some(1);
            assert!(!candidate_reapable(&value, u64::MAX));
        }
    }

    #[test]
    fn account_lifecycle_debug_output_contains_no_identity_or_secret() {
        let value = candidate(OAuthCandidateState::GrantBearing);
        let debug = format!("{value:?}");
        for forbidden in [
            "private-access",
            "private-refresh",
            "private-account",
            "123e4567",
            &"s".repeat(43),
        ] {
            assert!(!debug.contains(forbidden));
        }
    }
}
