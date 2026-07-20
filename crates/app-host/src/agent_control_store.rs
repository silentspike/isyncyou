use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use isyncyou_agent::{
    AgentCredentialStore, ConfirmError, FileLock, PairingClaimV2, PairingCodeV2,
    PairingDescriptorV2, PairingPayload, PairingSourceSecretV2, PendingActionBinding,
    PendingOwnerBinding, PendingPersistence, PersistedPendingAction, ToolAction,
};
use ring::{aead, hkdf, rand::SecureRandom as _};
use rusqlite::{
    params, Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use zeroize::Zeroizing;

const SCHEMA_VERSION: i64 = 11;
const CONTROL_KEY_DOMAIN: &[u8] = b"isyncyou-agent-control-store-root/v1";
const CONTROL_SUBKEY_SALT: &[u8] = b"isyncyou-agent-control-store-subkeys/v1";
const SQLCIPHER_KEY_INFO: &[u8] = b"isyncyou-agent-control-sqlcipher/v1";
const ROW_WRAP_KEY_INFO: &[u8] = b"isyncyou-agent-control-row-wrap/v1";
const MAX_CONTROL_BYTES: i64 = 32 * 1024 * 1024;
const MAX_CONFIRMATIONS: i64 = 4_096;
const MAX_PENDING_PLAINTEXT: usize = 256 * 1024;
const USER_PRESENCE_TTL_MS: u64 = 5 * 60 * 1_000;
const PAIRING_CLAIM_RESUME_TTL_MS: u64 = 24 * 60 * 60 * 1_000;
const CONTROL_TOMBSTONE_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
const MAX_PAIRING_SOURCE_BYTES: usize = 96 * 1_024;
const MUTATION_INTENT_TTL_MS: u64 = 15 * 60 * 1_000;
const MUTATION_TERMINAL_TTL_MS: u64 = 24 * 60 * 60 * 1_000;
const MUTATION_REQUEST_BINDING_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1_000;
const MAX_MUTATION_INTENTS_PER_OWNER: i64 = 4;
const MAX_MUTATION_INTENTS_PROCESS: i64 = 8;
const MAX_MUTATION_STAGED_BYTES: i64 = 256 * 1024 * 1024;
const MAX_MUTATION_CHUNKS: u32 = 8_192;
const MUTATION_FREE_SPACE_RESERVE: u64 = 128 * 1024 * 1024;
const PRODUCT_REQUEST_RECEIPT_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1_000;
const AGENT_TURN_ROUTE_DOMAIN: &str = "post:/api/v1/agent/turn";
const MAX_AGENT_TURN_ADMISSIONS: i64 = 8;
const MAX_AGENT_TURN_ADMISSION_BYTES: usize = 40 * 1024;

type MutationCommitRow = (
    Vec<u8>,
    i64,
    String,
    i64,
    String,
    Option<String>,
    Option<Vec<u8>>,
);
type MutationIntentCreateRow = (String, String, Vec<u8>, i64, String, i64, String);
type UserPresenceRow = (String, String, i64, Option<Vec<u8>>, Option<Vec<u8>>);
type PendingRow = (String, u64, String, Option<Vec<u8>>);
type ProductRequestReceiptRow = (String, String, String, String, Option<Vec<u8>>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredProductResponseV1 {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProductRequestBegin {
    Execute,
    Replay(StoredProductResponseV1),
    Conflict,
    OutcomeUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProductRequestBinding {
    Inserted,
    Existing,
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentTurnAdmissionV1 {
    version: u32,
    turn_id: String,
    route_domain: String,
    request_scope: String,
    payload_digest: String,
    request: isyncyou_webui::AgentTurnRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyAgentTurnAdmissionV1 {
    version: u32,
    turn_id: String,
    payload_digest: String,
    request: isyncyou_webui::AgentTurnRequest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecoveredAgentTurnAdmission {
    pub request: isyncyou_webui::AgentTurnRequest,
    pub turn_id: String,
    pub identity: isyncyou_webui::ProductRequestIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentTurnAdmissionBegin {
    Inserted,
    Existing,
    Cancelled,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentTurnAdmissionFailure {
    Failed,
    Cancelled,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingCancelProjection {
    pub pending_id: String,
    pub owner: PendingOwnerBinding,
    pub created_at_ms: u64,
}

#[derive(Debug, PartialEq)]
pub(crate) enum MutationCommitStart {
    Execute {
        purpose: Box<isyncyou_webui::MutationPurpose>,
        source: MutationChunkSource,
    },
    Replay(serde_json::Value),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MutationChunkDescriptor {
    index: u32,
    offset: u64,
    len: usize,
    sha256: String,
    relative_path: String,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct MutationChunkSource {
    root: PathBuf,
    row_wrap_key: Zeroizing<[u8; 32]>,
    intent_id: String,
    total_bytes: u64,
    chunks: Vec<MutationChunkDescriptor>,
}

impl std::fmt::Debug for MutationChunkSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MutationChunkSource")
            .field("total_bytes", &self.total_bytes)
            .field("chunk_count", &self.chunks.len())
            .finish_non_exhaustive()
    }
}

impl MutationChunkSource {
    pub(crate) fn len(&self) -> u64 {
        self.total_bytes
    }

    pub(crate) fn read_range(&self, offset: u64, len: usize) -> Result<Vec<u8>, String> {
        let requested_end = offset
            .checked_add(u64::try_from(len).map_err(|_| "mutation_intent_invalid")?)
            .ok_or("mutation_intent_invalid")?;
        if requested_end > self.total_bytes {
            return Err("mutation_intent_invalid".into());
        }
        let mut output = Vec::with_capacity(len);
        for descriptor in &self.chunks {
            let chunk_end = descriptor
                .offset
                .checked_add(u64::try_from(descriptor.len).map_err(|_| "mutation_intent_invalid")?)
                .ok_or("mutation_intent_invalid")?;
            if chunk_end <= offset || descriptor.offset >= requested_end {
                continue;
            }
            let sealed = std::fs::read(
                self.root
                    .join("mutation-staging")
                    .join(&descriptor.relative_path),
            )
            .map_err(|_| "mutation_intent_outcome_unknown")?;
            let chunk = open_row(
                &self.row_wrap_key,
                "mutation-chunk",
                &format!("{}:{}", self.intent_id, descriptor.index),
                &sealed,
            )?;
            if chunk.len() != descriptor.len || sha256_hex(&chunk) != descriptor.sha256 {
                return Err("mutation_intent_outcome_unknown".into());
            }
            let start = usize::try_from(offset.saturating_sub(descriptor.offset))
                .map_err(|_| "mutation_intent_invalid")?;
            let end = usize::try_from(requested_end.min(chunk_end) - descriptor.offset)
                .map_err(|_| "mutation_intent_invalid")?;
            output.extend_from_slice(&chunk[start..end]);
        }
        if output.len() != len {
            return Err("mutation_intent_outcome_unknown".into());
        }
        Ok(output)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PairingSourceRecord {
    pub pair_id: String,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PairingDescriptorCleanupTarget {
    pub pair_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum UserPresenceBinding {
    #[serde(rename = "session_archive")]
    Archive { session_id: String },
    #[serde(rename = "session_pairing_reveal")]
    PairingReveal { session_id: String, pair_id: String },
    #[serde(rename = "session_pairing_import")]
    PairingImport { pairing_code: String },
}

impl UserPresenceBinding {
    fn kind(&self) -> &'static str {
        match self {
            Self::Archive { .. } => "session_archive",
            Self::PairingReveal { .. } => "session_pairing_reveal",
            Self::PairingImport { .. } => "session_pairing_import",
        }
    }

    fn public_binding_digest(&self) -> String {
        let mut context = ring::digest::Context::new(&ring::digest::SHA256);
        context.update(b"isyncyou-user-presence-binding-v1\0");
        match self {
            Self::Archive { session_id } => context.update(session_id.as_bytes()),
            Self::PairingReveal {
                session_id,
                pair_id,
            } => {
                context.update(session_id.as_bytes());
                context.update(pair_id.as_bytes());
            }
            Self::PairingImport { pairing_code } => {
                context.update(pairing_code.as_bytes());
            }
        }
        URL_SAFE_NO_PAD.encode(context.finish())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserPresenceChallenge {
    pub operation_id: String,
    pub intent_id: String,
    pub token: String,
    pub action_hash: String,
    pub expires_at_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UserPresenceSecretV1 {
    version: u32,
    binding: UserPresenceBinding,
    token_hash: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UserPresenceConsumptionV1 {
    version: u32,
    route_request_id: String,
    binding: UserPresenceBinding,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PairingRevealResponseV1 {
    version: u32,
    route_request_id: String,
    source_secret: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingSecretV1 {
    version: u32,
    action: ToolAction,
    preview: String,
    token_hash: String,
    risk: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedRowV1 {
    version: u32,
    wrap_nonce: String,
    wrapped_key: String,
    payload_nonce: String,
    payload: String,
}

pub(crate) struct AgentControlStore {
    connection: Mutex<Connection>,
    row_wrap_key: Zeroizing<[u8; 32]>,
    installation_principal: Zeroizing<String>,
    installation_binding: String,
    _lock: FileLock,
    root: PathBuf,
}

impl std::fmt::Debug for AgentControlStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentControlStore")
            .field("root", &self.root)
            .field("installation_binding", &"[redacted]")
            .finish_non_exhaustive()
    }
}

impl AgentControlStore {
    pub(crate) fn open(
        base_dir: &Path,
        credential_store: &AgentCredentialStore,
        installation_principal: &str,
        lifecycle_key_version: u32,
    ) -> Result<Self, String> {
        if installation_principal.len() != 22 || !installation_principal.is_ascii() {
            return Err("control_store_identity_unavailable".into());
        }
        let root = base_dir.join("agent-control");
        create_private_directory(&root)?;
        let lock_path = root.join(".lock");
        reject_symlink_or_insecure_file(&lock_path)?;
        let lock = FileLock::try_acquire_exclusive(&lock_path)
            .map_err(|_| "control_store_lock_unavailable")?
            .ok_or_else(|| "control_store_busy".to_string())?;
        let db_path = root.join(".isyncyou-agent-control.db");
        reject_symlink_or_insecure_file(&db_path)?;
        for suffix in ["-wal", "-shm"] {
            reject_symlink_or_insecure_file(&PathBuf::from(format!(
                "{}{}",
                db_path.display(),
                suffix
            )))?;
        }

        let mut key_message = Vec::with_capacity(32);
        append_len_prefixed(&mut key_message, installation_principal.as_bytes())?;
        key_message.extend_from_slice(&lifecycle_key_version.to_be_bytes());
        let control_root = Zeroizing::new(
            credential_store
                .domain_hmac(CONTROL_KEY_DOMAIN, &key_message)
                .map_err(|_| "control_store_key_unavailable")?,
        );
        let (sqlcipher_key, row_wrap_key) = derive_control_subkeys(&control_root)?;
        let installation_binding = URL_SAFE_NO_PAD.encode(
            credential_store
                .domain_hmac(
                    b"isyncyou-agent-control-installation-binding/v1",
                    installation_principal.as_bytes(),
                )
                .map_err(|_| "control_store_key_unavailable")?,
        );

        let open_flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        // Android's bundled SQLCipher rejects SQLITE_OPEN_NOFOLLOW at open time. The database is
        // still confined to the app-private 0700 directory and is checked as a regular owner-only
        // file before and immediately after open. Other supported targets retain the kernel-backed
        // final-component no-follow flag.
        #[cfg(not(target_os = "android"))]
        let open_flags = open_flags | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let connection = Connection::open_with_flags(&db_path, open_flags)
            .map_err(|_| "control_store_database_open_failed")?;
        #[cfg(feature = "encrypted-store")]
        apply_sqlcipher_key(&connection, &sqlcipher_key[..])?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| "control_store_database_config_failed")?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys=ON;
                 PRAGMA journal_mode=WAL;
                 PRAGMA secure_delete=ON;",
            )
            .map_err(|_| "control_store_database_config_failed")?;
        let migration = connection
            .unchecked_transaction()
            .map_err(|_| "control_store_migration_failed")?;
        migration
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS control_metadata (
                   key TEXT PRIMARY KEY NOT NULL,
                   value TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS confirmation_intents (
                   intent_id TEXT PRIMARY KEY NOT NULL,
                   account_id TEXT NOT NULL,
                   session_id TEXT NOT NULL,
                   request_id TEXT NOT NULL,
                   turn_id TEXT NOT NULL,
                   owner_binding TEXT NOT NULL,
                   action_hash TEXT NOT NULL,
                   expires_at_ms INTEGER NOT NULL,
                   state TEXT NOT NULL CHECK(state IN ('pending','consumed','cancelled','expired')),
                   sealed_payload BLOB,
                   terminal_expires_at_ms INTEGER NOT NULL DEFAULT 0,
                   logical_bytes INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS confirmation_expiry
                   ON confirmation_intents(state, expires_at_ms);
                 CREATE TABLE IF NOT EXISTS pending_cancel_projections (
                   pending_id TEXT PRIMARY KEY NOT NULL,
                   account_id TEXT NOT NULL,
                   session_id TEXT NOT NULL,
                   request_id TEXT NOT NULL,
                   turn_id TEXT NOT NULL,
                   owner_binding TEXT NOT NULL,
                   created_at_ms INTEGER NOT NULL,
                   logical_bytes INTEGER NOT NULL,
                   FOREIGN KEY(pending_id) REFERENCES confirmation_intents(intent_id) ON DELETE CASCADE
                 );
                 CREATE INDEX IF NOT EXISTS pending_cancel_projection_created
                   ON pending_cancel_projections(created_at_ms);
                 INSERT OR IGNORE INTO pending_cancel_projections(
                   pending_id,account_id,session_id,request_id,turn_id,owner_binding,
                   created_at_ms,logical_bytes
                 )
                 SELECT intent_id,account_id,session_id,request_id,turn_id,owner_binding,
                   CAST(strftime('%s','now') AS INTEGER) * 1000,
                   length(intent_id) + length(account_id) + length(session_id) +
                   length(request_id) + length(turn_id) + length(owner_binding) + 128
                 FROM confirmation_intents
                 WHERE state='cancelled' AND session_id!='legacy-local';
                 CREATE TABLE IF NOT EXISTS user_presence_intents (
                   operation_id TEXT PRIMARY KEY NOT NULL,
                   intent_id TEXT UNIQUE NOT NULL,
                   request_id TEXT NOT NULL,
                   owner_binding TEXT NOT NULL,
                   kind TEXT NOT NULL,
                   action_hash TEXT NOT NULL,
                   expires_at_ms INTEGER NOT NULL,
                   state TEXT NOT NULL CHECK(state IN ('pending','authorized','consumed','cancelled','expired')),
                   sealed_payload BLOB,
                   sealed_response BLOB,
                   terminal_expires_at_ms INTEGER NOT NULL DEFAULT 0,
                   logical_bytes INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS user_presence_expiry
                   ON user_presence_intents(state, expires_at_ms);
                 CREATE TABLE IF NOT EXISTS pairing_sources (
                   pair_id TEXT PRIMARY KEY NOT NULL,
                   request_id TEXT UNIQUE NOT NULL,
                   session_id TEXT NOT NULL,
                   owner_binding TEXT NOT NULL,
                   expires_at_ms INTEGER NOT NULL,
                   state TEXT NOT NULL CHECK(state IN ('local','revealed','revoked','expired')),
                   sealed_source BLOB,
                   terminal_expires_at_ms INTEGER NOT NULL DEFAULT 0,
                   logical_bytes INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS pairing_source_expiry
                   ON pairing_sources(state, expires_at_ms);
                 CREATE TABLE IF NOT EXISTS pairing_claims (
                   operation_id TEXT PRIMARY KEY NOT NULL,
                   request_id TEXT UNIQUE NOT NULL,
                   pair_id TEXT NOT NULL,
                   owner_binding TEXT NOT NULL,
                   state TEXT NOT NULL CHECK(state IN ('claimed','installed','consumed','aborted','claimed_expired')),
                   resume_expires_at_ms INTEGER NOT NULL,
                   installed_session_id TEXT,
                   finalize_request_id TEXT,
                   sealed_resume BLOB,
                   terminal_expires_at_ms INTEGER NOT NULL DEFAULT 0,
                   logical_bytes INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS pairing_revocations (
                   operation_id TEXT PRIMARY KEY NOT NULL,
                   request_id TEXT UNIQUE NOT NULL,
                   pair_id TEXT NOT NULL,
                   owner_binding TEXT NOT NULL,
                   state TEXT NOT NULL CHECK(state IN ('in_flight','completed')),
                   terminal_expires_at_ms INTEGER NOT NULL DEFAULT 0,
                   logical_bytes INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS mutation_intents (
                   intent_id TEXT PRIMARY KEY NOT NULL,
                   create_request_id TEXT UNIQUE NOT NULL,
                   owner_binding TEXT NOT NULL,
                   purpose_json BLOB NOT NULL,
                   total_bytes INTEGER NOT NULL,
                   content_sha256 TEXT NOT NULL,
                   expires_at_ms INTEGER NOT NULL,
                   state TEXT NOT NULL CHECK(state IN ('active','committing','committed','cancelled','expired')),
                   commit_request_id TEXT,
                   result_json BLOB,
                   terminal_expires_at_ms INTEGER NOT NULL DEFAULT 0,
                   logical_bytes INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS mutation_intent_owner_state
                   ON mutation_intents(owner_binding,state,expires_at_ms);
                 CREATE TABLE IF NOT EXISTS mutation_request_bindings (
                   request_id TEXT PRIMARY KEY NOT NULL,
                   route_domain TEXT NOT NULL,
                   payload_digest TEXT NOT NULL,
                   expires_at_ms INTEGER NOT NULL DEFAULT 0,
                   logical_bytes INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS product_request_receipts (
                   request_id TEXT PRIMARY KEY NOT NULL,
                   request_key TEXT NOT NULL DEFAULT '',
                   route_domain TEXT NOT NULL,
                   request_scope TEXT NOT NULL DEFAULT 'installation',
                   payload_digest TEXT NOT NULL,
                   state TEXT NOT NULL CHECK(state IN ('started','completed')),
                   sealed_response BLOB,
                   expires_at_ms INTEGER NOT NULL,
                   logical_bytes INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS product_request_receipt_expiry
                   ON product_request_receipts(expires_at_ms);
                 CREATE TABLE IF NOT EXISTS product_request_bindings (
                   request_id TEXT PRIMARY KEY NOT NULL,
                   request_key TEXT NOT NULL DEFAULT '',
                   route_domain TEXT NOT NULL,
                   request_scope TEXT NOT NULL DEFAULT 'installation',
                   payload_digest TEXT NOT NULL,
                   expires_at_ms INTEGER NOT NULL,
                   logical_bytes INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS product_request_binding_expiry
                   ON product_request_bindings(expires_at_ms);
                 CREATE TABLE IF NOT EXISTS agent_turn_admissions (
                   request_id TEXT PRIMARY KEY NOT NULL,
                   turn_id TEXT UNIQUE NOT NULL,
                   request_scope TEXT NOT NULL DEFAULT 'installation',
                   payload_digest TEXT NOT NULL,
                   state TEXT NOT NULL DEFAULT 'active' CHECK(state IN ('active','cancelled')),
                   terminal_code TEXT,
                   sealed_request BLOB NOT NULL,
                   created_at_ms INTEGER NOT NULL,
                   logical_bytes INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS agent_turn_admission_created
                   ON agent_turn_admissions(created_at_ms,request_id);
                 CREATE TABLE IF NOT EXISTS mutation_chunks (
                   intent_id TEXT NOT NULL,
                   chunk_index INTEGER NOT NULL,
                   chunk_offset INTEGER NOT NULL,
                   chunk_bytes INTEGER NOT NULL,
                   chunk_sha256 TEXT NOT NULL,
                   sealed_path TEXT NOT NULL,
                   PRIMARY KEY(intent_id,chunk_index),
                   FOREIGN KEY(intent_id) REFERENCES mutation_intents(intent_id) ON DELETE CASCADE
                 );",
            )
            .map_err(|_| "control_store_migration_failed")?;
        ensure_text_column(
            &migration,
            "product_request_receipts",
            "request_scope",
            "installation",
        )?;
        ensure_text_column(
            &migration,
            "product_request_bindings",
            "request_scope",
            "installation",
        )?;
        ensure_text_column(
            &migration,
            "agent_turn_admissions",
            "request_scope",
            "installation",
        )?;
        ensure_text_column(&migration, "agent_turn_admissions", "state", "active")?;
        ensure_nullable_text_column(&migration, "agent_turn_admissions", "terminal_code")?;
        let invalid_admission_states: i64 = migration
            .query_row(
                "SELECT COUNT(*) FROM agent_turn_admissions
                 WHERE state NOT IN ('active','cancelled')
                    OR (state='cancelled' AND terminal_code IS NOT NULL)",
                [],
                |row| row.get(0),
            )
            .map_err(|_| "control_store_migration_failed")?;
        if invalid_admission_states != 0 {
            return Err("control_store_migration_failed".into());
        }
        let terminal_codes = {
            let mut statement = migration
                .prepare(
                    "SELECT terminal_code FROM agent_turn_admissions
                     WHERE terminal_code IS NOT NULL",
                )
                .map_err(|_| "control_store_migration_failed")?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|_| "control_store_migration_failed")?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|_| "control_store_migration_failed")?
        };
        if terminal_codes
            .iter()
            .any(|code| !valid_turn_terminal_code(code))
        {
            return Err("control_store_migration_failed".into());
        }
        ensure_request_key_column(&migration, "product_request_receipts")?;
        ensure_request_key_column(&migration, "product_request_bindings")?;
        ensure_integer_column(&migration, "mutation_intents", "terminal_expires_at_ms")?;
        ensure_integer_column(&migration, "mutation_request_bindings", "expires_at_ms")?;
        for table in [
            "confirmation_intents",
            "user_presence_intents",
            "pairing_sources",
            "pairing_claims",
            "pairing_revocations",
        ] {
            ensure_integer_column(&migration, table, "terminal_expires_at_ms")?;
        }
        migration
            .execute_batch(
                "CREATE INDEX IF NOT EXISTS mutation_intent_terminal_expiry
                   ON mutation_intents(state,terminal_expires_at_ms);
                 CREATE INDEX IF NOT EXISTS mutation_request_binding_expiry
                   ON mutation_request_bindings(expires_at_ms);
                 CREATE INDEX IF NOT EXISTS confirmation_terminal_expiry
                   ON confirmation_intents(state,terminal_expires_at_ms);
                 CREATE INDEX IF NOT EXISTS user_presence_terminal_expiry
                   ON user_presence_intents(state,terminal_expires_at_ms);
                 CREATE INDEX IF NOT EXISTS pairing_source_terminal_expiry
                   ON pairing_sources(state,terminal_expires_at_ms);
                 CREATE INDEX IF NOT EXISTS pairing_claim_terminal_expiry
                   ON pairing_claims(state,terminal_expires_at_ms);
                 CREATE INDEX IF NOT EXISTS pairing_revoke_terminal_expiry
                   ON pairing_revocations(state,terminal_expires_at_ms);",
            )
            .map_err(|_| "control_store_migration_failed")?;
        let stored_schema_version = stored_control_schema_version(&migration)?;
        if matches!(stored_schema_version, Some(1..=8)) {
            migrate_mutation_retention_v9(&migration, &row_wrap_key, crate::unix_now_ms())?;
        }
        if matches!(stored_schema_version, Some(1..=9)) {
            migrate_control_tombstone_retention_v10(&migration, crate::unix_now_ms())?;
        }
        migrate_agent_turn_admissions_v2(&migration, &row_wrap_key, installation_principal)?;
        migrate_product_request_scopes_v7(&migration, installation_principal)?;
        migrate_product_request_keys(&migration, installation_principal)?;
        initialize_metadata(&migration, &installation_binding, lifecycle_key_version)?;
        migration
            .commit()
            .map_err(|_| "control_store_migration_failed")?;
        secure_file_mode(&db_path)?;
        for suffix in ["-wal", "-shm"] {
            let path = PathBuf::from(format!("{}{}", db_path.display(), suffix));
            if path.exists() {
                secure_file_mode(&path)?;
            }
        }
        create_private_directory(&root.join("mutation-staging"))?;
        Ok(Self {
            connection: Mutex::new(connection),
            row_wrap_key,
            installation_principal: Zeroizing::new(installation_principal.to_owned()),
            installation_binding,
            _lock: lock,
            root,
        })
    }

    pub(crate) fn reap_expired(&self, now_ms: u64, limit: usize) -> Result<usize, String> {
        let mut remaining =
            i64::try_from(limit.min(256)).map_err(|_| "control_store_unavailable")?;
        let now = u64_to_i64(now_ms)?;
        let terminal_expires_at_ms = now.saturating_add(
            i64::try_from(CONTROL_TOMBSTONE_TTL_MS)
                .map_err(|_| "control_store_unavailable".to_string())?,
        );
        let connection = self
            .connection
            .lock()
            .map_err(|_| "control_store_unavailable")?;
        let result = (|| -> Result<usize, String> {
            let confirmations = connection
                .execute(
                    "UPDATE confirmation_intents
                 SET state='expired', sealed_payload=NULL, terminal_expires_at_ms=?3,
                     logical_bytes=0
                 WHERE intent_id IN (
                   SELECT intent_id FROM confirmation_intents
                   WHERE state='pending' AND expires_at_ms < ?1 LIMIT ?2
                 )",
                    params![now, remaining, terminal_expires_at_ms],
                )
                .map_err(|_| "control_store_unavailable")?;
            consume_reaper_budget(&mut remaining, confirmations)?;
            let presence = connection
                .execute(
                    "UPDATE user_presence_intents
                 SET state='expired', sealed_payload=NULL, sealed_response=NULL,
                     terminal_expires_at_ms=?3, logical_bytes=0
                 WHERE operation_id IN (
                   SELECT operation_id FROM user_presence_intents
                   WHERE state IN ('pending','authorized') AND expires_at_ms < ?1 LIMIT ?2
                 )",
                    params![now, remaining, terminal_expires_at_ms],
                )
                .map_err(|_| "control_store_unavailable")?;
            consume_reaper_budget(&mut remaining, presence)?;
            let pairing = connection
                .execute(
                    "UPDATE pairing_sources
                 SET state='expired', sealed_source=NULL, terminal_expires_at_ms=?3,
                     logical_bytes=0
                 WHERE pair_id IN (
                   SELECT pair_id FROM pairing_sources
                   WHERE state IN ('local','revealed') AND expires_at_ms < ?1
                     AND NOT EXISTS (
                       SELECT 1 FROM pairing_revocations
                       WHERE pairing_revocations.pair_id=pairing_sources.pair_id
                         AND pairing_revocations.state='in_flight'
                     )
                   LIMIT ?2
                 )",
                    params![now, remaining, terminal_expires_at_ms],
                )
                .map_err(|_| "control_store_unavailable")?;
            consume_reaper_budget(&mut remaining, pairing)?;
            let reveal_responses = connection
                .execute(
                    "UPDATE user_presence_intents
                 SET sealed_response=NULL, logical_bytes=0
                 WHERE operation_id IN (
                   SELECT operation_id FROM user_presence_intents
                   WHERE state='consumed' AND kind='session_pairing_reveal'
                     AND expires_at_ms < ?1 AND sealed_response IS NOT NULL LIMIT ?2
                 )",
                    params![now, remaining],
                )
                .map_err(|_| "control_store_unavailable")?;
            consume_reaper_budget(&mut remaining, reveal_responses)?;
            let claims = connection
                .execute(
                    "UPDATE pairing_claims
                 SET state='claimed_expired',sealed_resume=NULL,terminal_expires_at_ms=?3,
                     logical_bytes=0
                 WHERE operation_id IN (
                   SELECT operation_id FROM pairing_claims
                   WHERE state IN ('claimed','installed') AND resume_expires_at_ms < ?1 LIMIT ?2
                 )",
                    params![now, remaining, terminal_expires_at_ms],
                )
                .map_err(|_| "control_store_unavailable")?;
            consume_reaper_budget(&mut remaining, claims)?;
            let confirmation_tombstones = delete_expired_control_tombstones(
                &connection,
                "confirmation_intents",
                "intent_id",
                "'consumed','cancelled','expired'",
                now,
                remaining,
            )?;
            consume_reaper_budget(&mut remaining, confirmation_tombstones)?;
            let presence_tombstones = delete_expired_control_tombstones(
                &connection,
                "user_presence_intents",
                "operation_id",
                "'consumed','cancelled','expired'",
                now,
                remaining,
            )?;
            consume_reaper_budget(&mut remaining, presence_tombstones)?;
            let source_tombstones = delete_expired_control_tombstones(
                &connection,
                "pairing_sources",
                "pair_id",
                "'revoked','expired'",
                now,
                remaining,
            )?;
            consume_reaper_budget(&mut remaining, source_tombstones)?;
            let claim_tombstones = delete_expired_control_tombstones(
                &connection,
                "pairing_claims",
                "operation_id",
                "'consumed','aborted','claimed_expired'",
                now,
                remaining,
            )?;
            consume_reaper_budget(&mut remaining, claim_tombstones)?;
            let revoke_tombstones = delete_expired_control_tombstones(
                &connection,
                "pairing_revocations",
                "operation_id",
                "'completed'",
                now,
                remaining,
            )?;
            consume_reaper_budget(&mut remaining, revoke_tombstones)?;
            let expired_intents =
                self.reap_mutation_intents_locked(&connection, now_ms, remaining)?;
            consume_reaper_budget(&mut remaining, expired_intents)?;
            let product_receipts = connection
                .execute(
                    "DELETE FROM product_request_receipts
                 WHERE request_id IN (
                   SELECT request_id FROM product_request_receipts
                   WHERE expires_at_ms < ?1 LIMIT ?2
                 )",
                    params![now, remaining],
                )
                .map_err(|_| "control_store_unavailable")?;
            consume_reaper_budget(&mut remaining, product_receipts)?;
            let product_bindings = connection
                .execute(
                    "DELETE FROM product_request_bindings
                 WHERE request_id IN (
                   SELECT request_id FROM product_request_bindings
                   WHERE expires_at_ms < ?1 LIMIT ?2
                 )",
                    params![now, remaining],
                )
                .map_err(|_| "control_store_unavailable")?;
            consume_reaper_budget(&mut remaining, product_bindings)?;
            let mutation_bindings = connection
                .execute(
                    "DELETE FROM mutation_request_bindings
                 WHERE request_id IN (
                   SELECT request_id FROM mutation_request_bindings
                   WHERE expires_at_ms < ?1 LIMIT ?2
                 )",
                    params![now, remaining],
                )
                .map_err(|_| "control_store_unavailable")?;
            consume_reaper_budget(&mut remaining, mutation_bindings)?;
            let reaped = confirmations
                .saturating_add(presence)
                .saturating_add(pairing)
                .saturating_add(reveal_responses)
                .saturating_add(claims)
                .saturating_add(confirmation_tombstones)
                .saturating_add(presence_tombstones)
                .saturating_add(source_tombstones)
                .saturating_add(claim_tombstones)
                .saturating_add(revoke_tombstones)
                .saturating_add(expired_intents)
                .saturating_add(product_receipts)
                .saturating_add(product_bindings)
                .saturating_add(mutation_bindings);
            Ok(reaped)
        })();
        // Always checkpoint, including after a partially successful bounded pass.
        // Otherwise an error in a later cleanup could leave an earlier erased row key
        // recoverable from an old WAL frame forever.
        let checkpoint = checkpoint_secure_erasure(&connection)
            .map_err(|_| "control_store_unavailable".to_string());
        match (result, checkpoint) {
            (Ok(reaped), Ok(())) => Ok(reaped),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    pub(crate) fn claim_session_maintenance_offset(
        &self,
        target_count: usize,
        batch_size: usize,
    ) -> Result<usize, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "control_store_unavailable")?;
        let stored: Option<String> = connection
            .query_row(
                "SELECT value FROM control_metadata WHERE key='session_maintenance_cursor'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| "control_store_unavailable")?;
        let cursor = stored
            .as_deref()
            .map(str::parse::<usize>)
            .transpose()
            .map_err(|_| "control_store_unavailable")?
            .unwrap_or(0);
        let offset = if target_count == 0 {
            0
        } else {
            cursor % target_count
        };
        let advance = batch_size.min(target_count);
        let next = if target_count == 0 {
            0
        } else {
            offset
                .checked_add(advance)
                .ok_or_else(|| "control_store_unavailable".to_string())?
                % target_count
        };
        connection
            .execute(
                "INSERT INTO control_metadata(key,value)
                 VALUES('session_maintenance_cursor',?1)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![next.to_string()],
            )
            .map_err(|_| "control_store_unavailable")?;
        Ok(offset)
    }

    pub(crate) fn pending_cancel_projections(
        &self,
        limit: usize,
    ) -> Result<Vec<PendingCancelProjection>, String> {
        let limit = i64::try_from(limit.min(32)).map_err(|_| "control_store_unavailable")?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| "control_store_unavailable")?;
        let mut statement = connection
            .prepare(
                "SELECT pending_id,account_id,session_id,request_id,turn_id,created_at_ms
                 FROM pending_cancel_projections
                 WHERE owner_binding=?1
                 ORDER BY created_at_ms,pending_id
                 LIMIT ?2",
            )
            .map_err(|_| "control_store_unavailable")?;
        let rows = statement
            .query_map(params![self.installation_binding, limit], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    PendingOwnerBinding {
                        account: row.get(1)?,
                        session_id: row.get(2)?,
                        request_id: row.get(3)?,
                        turn_id: row.get(4)?,
                    },
                    row.get::<_, i64>(5)?,
                ))
            })
            .map_err(|_| "control_store_unavailable")?;
        let mut projections = Vec::new();
        for row in rows {
            let (pending_id, owner, created_at_ms) =
                row.map_err(|_| "control_store_unavailable")?;
            projections.push(PendingCancelProjection {
                pending_id,
                owner,
                created_at_ms: u64::try_from(created_at_ms)
                    .map_err(|_| "control_store_unavailable")?,
            });
        }
        Ok(projections)
    }

    pub(crate) fn complete_pending_cancel_projection(
        &self,
        pending_id: &str,
    ) -> Result<(), String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "control_store_unavailable")?;
        connection
            .execute(
                "DELETE FROM pending_cancel_projections
                 WHERE pending_id=?1 AND owner_binding=?2",
                params![pending_id, self.installation_binding],
            )
            .map_err(|_| "control_store_unavailable")?;
        Ok(())
    }

    fn mutation_owner_binding(&self, owner: &str) -> Result<String, String> {
        if owner.is_empty() || owner.len() > 256 {
            return Err("mutation_intent_invalid".into());
        }
        let mut context = ring::digest::Context::new(&ring::digest::SHA256);
        context.update(b"isyncyou-mutation-owner-v1\0");
        context.update(&self.row_wrap_key[..]);
        context.update(owner.as_bytes());
        Ok(URL_SAFE_NO_PAD.encode(context.finish()))
    }

    fn require_mutation_free_space(&self, additional_bytes: u64) -> Result<(), String> {
        let required = additional_bytes
            .checked_add(MUTATION_FREE_SPACE_RESERVE)
            .ok_or("mutation_intent_invalid")?;
        require_mutation_free_space(
            required,
            fs2::available_space(&self.root).map_err(|_| "mutation_intent_storage_unavailable")?,
        )
    }

    pub(crate) fn create_mutation_intent(
        &self,
        request: &isyncyou_webui::MutationIntentCreate,
        now_ms: u64,
    ) -> Result<isyncyou_webui::MutationIntentInfo, String> {
        let max_bytes = match &request.purpose {
            isyncyou_webui::MutationPurpose::OnedriveUpload { .. }
            | isyncyou_webui::MutationPurpose::OnedriveReplace { .. } => 64 * 1024 * 1024,
            isyncyou_webui::MutationPurpose::MailBody { .. }
            | isyncyou_webui::MutationPurpose::OnenoteBody { .. } => 2 * 1024 * 1024,
            isyncyou_webui::MutationPurpose::TodoDeleteBatch { .. } => 512 * 1024,
        };
        if request.total_bytes > max_bytes || !valid_sha256(&request.sha256) {
            return Err("mutation_intent_invalid".into());
        }
        let chunk_count = request
            .total_bytes
            .saturating_add(isyncyou_webui::MUTATION_CHUNK_BYTES as u64 - 1)
            / isyncyou_webui::MUTATION_CHUNK_BYTES as u64;
        if chunk_count > u64::from(MAX_MUTATION_CHUNKS) {
            return Err("mutation_intent_invalid".into());
        }
        self.require_mutation_free_space(request.total_bytes)?;
        let owner_binding = self.mutation_owner_binding(&request.owner)?;
        let purpose_json = serde_json::to_vec(&request.purpose)
            .map_err(|_| "mutation_intent_invalid".to_string())?;
        if purpose_json.len() > 8 * 1024 {
            return Err("mutation_intent_invalid".into());
        }
        let expires_at_ms = now_ms
            .checked_add(MUTATION_INTENT_TTL_MS)
            .ok_or_else(|| "mutation_intent_invalid".to_string())?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "mutation_intent_failed")?;
        self.reap_mutation_intents_locked(&connection, now_ms, 256)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "mutation_intent_failed")?;
        let request_binding = bind_mutation_request(
            &transaction,
            &request.request_id,
            "post:/api/v1/mutation-intent/create",
            &[
                owner_binding.as_bytes(),
                &purpose_json,
                &request.total_bytes.to_be_bytes(),
                request.sha256.as_bytes(),
            ],
            now_ms,
        )?;
        let existing: Option<MutationIntentCreateRow> = transaction
            .query_row(
                "SELECT intent_id,owner_binding,purpose_json,total_bytes,content_sha256,
                        expires_at_ms,state
                 FROM mutation_intents
                 WHERE create_request_id=?1",
                params![request.request_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| "mutation_intent_failed")?;
        if let Some((
            intent_id,
            existing_owner,
            existing_purpose,
            existing_total,
            existing_sha,
            expires,
            state,
        )) = existing
        {
            if existing_owner != owner_binding
                || existing_total != request.total_bytes as i64
                || existing_sha != request.sha256
            {
                return Err("request_id_conflict".into());
            }
            if matches!(state.as_str(), "active" | "committing") {
                let existing_purpose = open_row(
                    &self.row_wrap_key,
                    "mutation-purpose",
                    &intent_id,
                    &existing_purpose,
                )?;
                if existing_purpose != purpose_json {
                    return Err("request_id_conflict".into());
                }
            }
            transaction.commit().map_err(|_| "mutation_intent_failed")?;
            return Ok(isyncyou_webui::MutationIntentInfo {
                intent_id,
                chunk_bytes: isyncyou_webui::MUTATION_CHUNK_BYTES,
                expires_at_ms: u64::try_from(expires).map_err(|_| "mutation_intent_failed")?,
            });
        }
        if request_binding == ProductRequestBinding::Existing {
            return Err("mutation_intent_outcome_unknown".into());
        }
        let owner_count: i64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM mutation_intents
                 WHERE owner_binding=?1 AND state IN ('active','committing')",
                params![owner_binding],
                |row| row.get(0),
            )
            .map_err(|_| "mutation_intent_failed")?;
        let process_count: i64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM mutation_intents WHERE state IN ('active','committing')",
                [],
                |row| row.get(0),
            )
            .map_err(|_| "mutation_intent_failed")?;
        let reserved: i64 = transaction
            .query_row(
                "SELECT COALESCE(SUM(total_bytes),0) FROM mutation_intents
                 WHERE state IN ('active','committing')",
                [],
                |row| row.get(0),
            )
            .map_err(|_| "mutation_intent_failed")?;
        let total_bytes =
            i64::try_from(request.total_bytes).map_err(|_| "mutation_intent_invalid")?;
        let intent_id = random_id(24)?;
        let sealed_purpose = seal_row(
            &self.row_wrap_key,
            "mutation-purpose",
            &intent_id,
            &purpose_json,
        )?;
        let logical_bytes = i64::try_from(sealed_purpose.len().saturating_add(512))
            .map_err(|_| "mutation_intent_invalid")?;
        if owner_count >= MAX_MUTATION_INTENTS_PER_OWNER
            || process_count >= MAX_MUTATION_INTENTS_PROCESS
            || reserved.saturating_add(total_bytes) > MAX_MUTATION_STAGED_BYTES
        {
            return Err("mutation_intent_quota_exceeded".into());
        }
        enforce_control_quota(&transaction, logical_bytes)?;
        transaction
            .execute(
                "INSERT INTO mutation_intents(
                   intent_id,create_request_id,owner_binding,purpose_json,total_bytes,
                   content_sha256,expires_at_ms,state,logical_bytes
                 ) VALUES(?1,?2,?3,?4,?5,?6,?7,'active',?8)",
                params![
                    intent_id,
                    request.request_id,
                    owner_binding,
                    sealed_purpose,
                    total_bytes,
                    request.sha256,
                    u64_to_i64(expires_at_ms)?,
                    logical_bytes,
                ],
            )
            .map_err(|_| "mutation_intent_failed")?;
        transaction.commit().map_err(|_| "mutation_intent_failed")?;
        Ok(isyncyou_webui::MutationIntentInfo {
            intent_id,
            chunk_bytes: isyncyou_webui::MUTATION_CHUNK_BYTES,
            expires_at_ms,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn put_mutation_chunk(
        &self,
        owner: &str,
        request_id: &str,
        intent_id: &str,
        index: u32,
        offset: u64,
        chunk_sha256: &str,
        bytes: &[u8],
        now_ms: u64,
    ) -> Result<(), String> {
        if request_id.is_empty()
            || index >= MAX_MUTATION_CHUNKS
            || bytes.len() > isyncyou_webui::MUTATION_CHUNK_BYTES
            || !valid_sha256(chunk_sha256)
            || sha256_hex(bytes) != chunk_sha256
            || offset != u64::from(index) * isyncyou_webui::MUTATION_CHUNK_BYTES as u64
        {
            return Err("mutation_intent_invalid".into());
        }
        self.require_mutation_free_space(bytes.len() as u64)?;
        let owner_binding = self.mutation_owner_binding(owner)?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "mutation_intent_failed")?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "mutation_intent_failed")?;
        enforce_mutation_reservation_quota(&transaction, &owner_binding)?;
        let _ = bind_mutation_request(
            &transaction,
            request_id,
            "post:/api/v1/mutation-intent/chunk",
            &[
                owner_binding.as_bytes(),
                intent_id.as_bytes(),
                &index.to_be_bytes(),
                &offset.to_be_bytes(),
                chunk_sha256.as_bytes(),
            ],
            now_ms,
        )?;
        let intent: Option<(String, i64, i64)> = transaction
            .query_row(
                "SELECT state,total_bytes,expires_at_ms FROM mutation_intents
                 WHERE intent_id=?1 AND owner_binding=?2",
                params![intent_id, owner_binding],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|_| "mutation_intent_failed")?;
        let Some((state, total_bytes, expires)) = intent else {
            return Err("mutation_intent_not_found".into());
        };
        if now_ms > u64::try_from(expires).map_err(|_| "mutation_intent_failed")? {
            return Err("mutation_intent_expired".into());
        }
        if state != "active" || offset.saturating_add(bytes.len() as u64) > total_bytes as u64 {
            return Err("mutation_intent_conflict".into());
        }
        let existing: Option<(i64, i64, String)> = transaction
            .query_row(
                "SELECT chunk_offset,chunk_bytes,chunk_sha256 FROM mutation_chunks
                 WHERE intent_id=?1 AND chunk_index=?2",
                params![intent_id, index],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|_| "mutation_intent_failed")?;
        if let Some((stored_offset, stored_bytes, stored_sha)) = existing {
            if stored_offset == offset as i64
                && stored_bytes == bytes.len() as i64
                && stored_sha == chunk_sha256
            {
                transaction.commit().map_err(|_| "mutation_intent_failed")?;
                return Ok(());
            }
            return Err("mutation_intent_conflict".into());
        }
        let relative_path = format!("{intent_id}/{index}.chunk");
        let final_path = self.root.join("mutation-staging").join(&relative_path);
        create_private_directory(final_path.parent().ok_or("mutation_intent_failed")?)?;
        match std::fs::symlink_metadata(&final_path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err("mutation_intent_storage_unavailable".into());
            }
            Ok(_) => {
                reject_symlink_or_insecure_file(&final_path)
                    .map_err(|_| "mutation_intent_storage_unavailable".to_string())?;
                std::fs::remove_file(&final_path)
                    .map_err(|_| "mutation_intent_storage_unavailable".to_string())?;
                sync_directory(
                    final_path
                        .parent()
                        .ok_or("mutation_intent_storage_unavailable")?,
                )
                .map_err(|_| "mutation_intent_storage_unavailable".to_string())?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("mutation_intent_storage_unavailable".into()),
        }
        let sealed = seal_row(
            &self.row_wrap_key,
            "mutation-chunk",
            &format!("{intent_id}:{index}"),
            bytes,
        )?;
        write_private_atomic(&final_path, &sealed)?;
        let insert = transaction.execute(
            "INSERT INTO mutation_chunks(
               intent_id,chunk_index,chunk_offset,chunk_bytes,chunk_sha256,sealed_path
             ) VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                intent_id,
                index,
                offset as i64,
                bytes.len() as i64,
                chunk_sha256,
                relative_path
            ],
        );
        if insert.is_err() {
            remove_private_regular_file_and_sync(&final_path)
                .map_err(|_| "mutation_intent_storage_unavailable".to_string())?;
            return Err("mutation_intent_failed".into());
        }
        transaction
            .commit()
            .map_err(|_| "mutation_intent_failed".to_string())
    }

    pub(crate) fn begin_mutation_commit(
        &self,
        owner: &str,
        request_id: &str,
        intent_id: &str,
        total_bytes: u64,
        sha256: &str,
        now_ms: u64,
    ) -> Result<MutationCommitStart, String> {
        self.require_mutation_free_space(0)?;
        let owner_binding = self.mutation_owner_binding(owner)?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "mutation_intent_failed")?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "mutation_intent_failed")?;
        enforce_mutation_reservation_quota(&transaction, &owner_binding)?;
        let _ = bind_mutation_request(
            &transaction,
            request_id,
            "post:/api/v1/mutation-intent/commit",
            &[
                owner_binding.as_bytes(),
                intent_id.as_bytes(),
                &total_bytes.to_be_bytes(),
                sha256.as_bytes(),
            ],
            now_ms,
        )?;
        let row: Option<MutationCommitRow> = transaction
            .query_row(
                "SELECT purpose_json,total_bytes,content_sha256,expires_at_ms,state,commit_request_id,result_json
                 FROM mutation_intents WHERE intent_id=?1 AND owner_binding=?2",
                params![intent_id, owner_binding],
                |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?)),
            )
            .optional()
            .map_err(|_| "mutation_intent_failed")?;
        let Some((
            purpose_json,
            stored_total,
            stored_sha,
            expires,
            state,
            stored_request,
            result_json,
        )) = row
        else {
            return Err("mutation_intent_not_found".into());
        };
        if stored_total != total_bytes as i64 || stored_sha != sha256 {
            return Err("mutation_intent_conflict".into());
        }
        if state == "committed" {
            if stored_request.as_deref() != Some(request_id) {
                return Err("request_id_conflict".into());
            }
            let result = open_row(
                &self.row_wrap_key,
                "mutation-result",
                intent_id,
                result_json
                    .as_deref()
                    .ok_or("mutation_intent_outcome_unknown")?,
            )?;
            let result =
                serde_json::from_slice(&result).map_err(|_| "mutation_intent_outcome_unknown")?;
            self.remove_mutation_chunk_files(&transaction, intent_id)
                .map_err(|_| "mutation_intent_outcome_unknown")?;
            transaction.commit().map_err(|_| "mutation_intent_failed")?;
            checkpoint_secure_erasure(&connection)
                .map_err(|_| "mutation_intent_outcome_unknown")?;
            return Ok(MutationCommitStart::Replay(result));
        }
        if state == "committing" {
            if stored_request.as_deref() != Some(request_id) {
                return Err("request_id_conflict".into());
            }
            transaction.commit().map_err(|_| "mutation_intent_failed")?;
            return Err("mutation_intent_outcome_unknown".into());
        }
        if now_ms > u64::try_from(expires).map_err(|_| "mutation_intent_failed")? {
            return Err("mutation_intent_expired".into());
        }
        if state != "active" {
            return Err("mutation_intent_conflict".into());
        }
        let mut statement = transaction
            .prepare(
                "SELECT chunk_index,chunk_offset,chunk_bytes,chunk_sha256,sealed_path
                 FROM mutation_chunks WHERE intent_id=?1 ORDER BY chunk_index",
            )
            .map_err(|_| "mutation_intent_failed")?;
        let rows = statement
            .query_map(params![intent_id], |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .map_err(|_| "mutation_intent_failed")?;
        let mut descriptors = Vec::new();
        let mut expected_offset = 0u64;
        let mut content_digest = ring::digest::Context::new(&ring::digest::SHA256);
        for row in rows {
            let (index, stored_offset, stored_len, chunk_sha, relative_path) =
                row.map_err(|_| "mutation_intent_failed")?;
            let offset = u64::try_from(stored_offset).map_err(|_| "mutation_intent_failed")?;
            let expected_len = usize::try_from(stored_len).map_err(|_| "mutation_intent_failed")?;
            if index as usize != descriptors.len()
                || offset != expected_offset
                || relative_path != format!("{intent_id}/{index}.chunk")
            {
                return Err("mutation_intent_conflict".into());
            }
            let sealed = std::fs::read(self.root.join("mutation-staging").join(&relative_path))
                .map_err(|_| "mutation_intent_outcome_unknown")?;
            let chunk = open_row(
                &self.row_wrap_key,
                "mutation-chunk",
                &format!("{intent_id}:{index}"),
                &sealed,
            )?;
            if chunk.len() != expected_len || sha256_hex(&chunk) != chunk_sha {
                return Err("mutation_intent_outcome_unknown".into());
            }
            content_digest.update(&chunk);
            expected_offset = expected_offset
                .checked_add(u64::try_from(chunk.len()).map_err(|_| "mutation_intent_invalid")?)
                .ok_or("mutation_intent_invalid")?;
            descriptors.push(MutationChunkDescriptor {
                index,
                offset,
                len: expected_len,
                sha256: chunk_sha,
                relative_path,
            });
        }
        drop(statement);
        if expected_offset != total_bytes || digest_hex(content_digest.finish().as_ref()) != sha256
        {
            return Err("mutation_intent_conflict".into());
        }
        let changed = transaction
            .execute(
                "UPDATE mutation_intents SET state='committing',commit_request_id=?2
                 WHERE intent_id=?1 AND state='active'",
                params![intent_id, request_id],
            )
            .map_err(|_| "mutation_intent_failed")?;
        if changed != 1 {
            return Err("mutation_intent_conflict".into());
        }
        transaction.commit().map_err(|_| "mutation_intent_failed")?;
        let purpose_json = open_row(
            &self.row_wrap_key,
            "mutation-purpose",
            intent_id,
            &purpose_json,
        )?;
        let purpose =
            serde_json::from_slice(&purpose_json).map_err(|_| "mutation_intent_outcome_unknown")?;
        Ok(MutationCommitStart::Execute {
            purpose: Box::new(purpose),
            source: MutationChunkSource {
                root: self.root.clone(),
                row_wrap_key: self.row_wrap_key.clone(),
                intent_id: intent_id.to_string(),
                total_bytes,
                chunks: descriptors,
            },
        })
    }

    pub(crate) fn complete_mutation_commit(
        &self,
        owner: &str,
        request_id: &str,
        intent_id: &str,
        result: &serde_json::Value,
        now_ms: u64,
    ) -> Result<(), String> {
        let owner_binding = self.mutation_owner_binding(owner)?;
        let result_json = serde_json::to_vec(result).map_err(|_| "mutation_intent_failed")?;
        let sealed_result = seal_row(
            &self.row_wrap_key,
            "mutation-result",
            intent_id,
            &result_json,
        )?;
        let logical_bytes = i64::try_from(sealed_result.len().saturating_add(512))
            .map_err(|_| "mutation_intent_failed")?;
        let terminal_expires_at_ms = now_ms
            .checked_add(MUTATION_TERMINAL_TTL_MS)
            .ok_or("mutation_intent_failed")?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| "mutation_intent_failed")?;
        let changed = connection
            .execute(
                "UPDATE mutation_intents
                 SET state='committed',purpose_json=X'',result_json=?4,
                     terminal_expires_at_ms=?5,logical_bytes=?6
                 WHERE intent_id=?1 AND owner_binding=?2 AND commit_request_id=?3 AND state='committing'",
                params![
                    intent_id,
                    owner_binding,
                    request_id,
                    sealed_result,
                    u64_to_i64(terminal_expires_at_ms)?,
                    logical_bytes
                ],
            )
            .map_err(|_| "mutation_intent_failed")?;
        if changed != 1 {
            return Err("mutation_intent_outcome_unknown".into());
        }
        self.remove_mutation_chunk_files(&connection, intent_id)
            .map_err(|_| "mutation_intent_outcome_unknown")?;
        checkpoint_secure_erasure(&connection).map_err(|_| "mutation_intent_outcome_unknown".into())
    }

    pub(crate) fn cancel_mutation_intent(
        &self,
        owner: &str,
        request_id: &str,
        intent_id: &str,
    ) -> Result<(), String> {
        let now_ms = crate::unix_now_ms();
        let owner_binding = self.mutation_owner_binding(owner)?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "mutation_intent_failed")?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "mutation_intent_failed")?;
        let _ = bind_mutation_request(
            &transaction,
            request_id,
            "post:/api/v1/mutation-intent/cancel",
            &[owner_binding.as_bytes(), intent_id.as_bytes()],
            now_ms,
        )?;
        let state: Option<String> = transaction
            .query_row(
                "SELECT state FROM mutation_intents WHERE intent_id=?1 AND owner_binding=?2",
                params![intent_id, owner_binding],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| "mutation_intent_failed")?;
        if state.as_deref() == Some("cancelled") {
            transaction.commit().map_err(|_| "mutation_intent_failed")?;
            self.remove_mutation_chunk_files(&connection, intent_id)?;
            checkpoint_secure_erasure(&connection).map_err(|_| "mutation_intent_failed")?;
            return Ok(());
        }
        let terminal_expires_at_ms = now_ms
            .checked_add(MUTATION_TERMINAL_TTL_MS)
            .ok_or("mutation_intent_failed")?;
        let changed = transaction
            .execute(
                "UPDATE mutation_intents
                 SET state='cancelled',purpose_json=X'',result_json=NULL,
                     terminal_expires_at_ms=?3,logical_bytes=512
                 WHERE intent_id=?1 AND owner_binding=?2 AND state='active'",
                params![
                    intent_id,
                    owner_binding,
                    u64_to_i64(terminal_expires_at_ms)?
                ],
            )
            .map_err(|_| "mutation_intent_failed")?;
        if changed == 0 {
            return Err("mutation_intent_conflict".into());
        }
        transaction.commit().map_err(|_| "mutation_intent_failed")?;
        self.remove_mutation_chunk_files(&connection, intent_id)?;
        checkpoint_secure_erasure(&connection).map_err(|_| "mutation_intent_failed".into())
    }

    fn reap_mutation_intents_locked(
        &self,
        connection: &Connection,
        now_ms: u64,
        limit: i64,
    ) -> Result<usize, String> {
        let terminal_expires_at_ms = now_ms
            .checked_add(MUTATION_TERMINAL_TTL_MS)
            .ok_or("mutation_intent_failed")?;
        let mut statement = connection
            .prepare(
                "SELECT intent_id FROM mutation_intents
                 WHERE state='active' AND expires_at_ms < ?1 LIMIT ?2",
            )
            .map_err(|_| "mutation_intent_failed")?;
        let ids = statement
            .query_map(params![u64_to_i64(now_ms)?, limit], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|_| "mutation_intent_failed")?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "mutation_intent_failed")?;
        drop(statement);
        for intent_id in &ids {
            // Keep the reservation active until every sealed chunk and its durable
            // directory entry are gone. A failed cleanup is retried as the same
            // active intent instead of publishing a false terminal state.
            self.remove_mutation_chunk_files(connection, intent_id)?;
            let changed = connection
                .execute(
                    "UPDATE mutation_intents
                     SET state='expired',purpose_json=X'',result_json=NULL,
                         terminal_expires_at_ms=?2,logical_bytes=512
                     WHERE intent_id=?1 AND state='active'",
                    params![intent_id, u64_to_i64(terminal_expires_at_ms)?],
                )
                .map_err(|_| "mutation_intent_failed")?;
            if changed != 1 {
                return Err("mutation_intent_failed".into());
            }
        }
        let remaining = usize::try_from(limit)
            .unwrap_or(0)
            .saturating_sub(ids.len());
        let mut terminal_statement = connection
            .prepare(
                "SELECT intent_id FROM mutation_intents
                 WHERE state IN ('committed','cancelled','expired')
                   AND terminal_expires_at_ms > 0 AND terminal_expires_at_ms < ?1
                 LIMIT ?2",
            )
            .map_err(|_| "mutation_intent_failed")?;
        let terminal_ids = terminal_statement
            .query_map(
                params![
                    u64_to_i64(now_ms)?,
                    i64::try_from(remaining).map_err(|_| "mutation_intent_failed")?
                ],
                |row| row.get::<_, String>(0),
            )
            .map_err(|_| "mutation_intent_failed")?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "mutation_intent_failed")?;
        drop(terminal_statement);
        for intent_id in &terminal_ids {
            self.remove_mutation_chunk_files(connection, intent_id)?;
            connection
                .execute(
                    "DELETE FROM mutation_intents WHERE intent_id=?1
                     AND state IN ('committed','cancelled','expired')",
                    params![intent_id],
                )
                .map_err(|_| "mutation_intent_failed")?;
        }
        let remaining = remaining.saturating_sub(terminal_ids.len());
        let orphans = self.reconcile_mutation_staging_locked(connection, remaining)?;
        Ok(ids
            .len()
            .saturating_add(terminal_ids.len())
            .saturating_add(orphans))
    }

    fn reconcile_mutation_staging_locked(
        &self,
        connection: &Connection,
        limit: usize,
    ) -> Result<usize, String> {
        if limit == 0 {
            return Ok(0);
        }
        let staging = self.root.join("mutation-staging");
        let entries = std::fs::read_dir(&staging).map_err(|_| "mutation_intent_failed")?;
        let mut removed = 0usize;
        for entry in entries {
            if removed >= limit {
                break;
            }
            let entry = entry.map_err(|_| "mutation_intent_failed")?;
            let name = entry.file_name();
            let Some(intent_id) = name.to_str() else {
                remove_private_tree_no_follow(&entry.path())?;
                removed += 1;
                continue;
            };
            let retained: bool = connection
                .query_row(
                    "SELECT EXISTS(
                       SELECT 1 FROM mutation_intents
                       WHERE intent_id=?1 AND state IN ('active','committing')
                     )",
                    params![intent_id],
                    |row| row.get(0),
                )
                .map_err(|_| "mutation_intent_failed")?;
            if !retained {
                remove_private_tree_no_follow(&entry.path())?;
                connection
                    .execute(
                        "DELETE FROM mutation_chunks WHERE intent_id=?1",
                        params![intent_id],
                    )
                    .map_err(|_| "mutation_intent_failed")?;
                removed += 1;
            }
        }
        if removed > 0 {
            std::fs::File::open(&staging)
                .and_then(|directory| directory.sync_all())
                .map_err(|_| "mutation_intent_failed")?;
        }
        Ok(removed)
    }

    fn remove_mutation_chunk_files(
        &self,
        connection: &Connection,
        intent_id: &str,
    ) -> Result<(), String> {
        let mut statement = connection
            .prepare(
                "SELECT chunk_index,sealed_path FROM mutation_chunks
                 WHERE intent_id=?1 ORDER BY chunk_index",
            )
            .map_err(|_| "mutation_intent_failed")?;
        let paths = statement
            .query_map(params![intent_id], |row| {
                Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|_| "mutation_intent_failed")?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "mutation_intent_failed")?;
        drop(statement);
        let staging = self.root.join("mutation-staging");
        let intent_dir = staging.join(intent_id);
        for (index, relative_path) in paths {
            if relative_path != format!("{intent_id}/{index}.chunk") {
                return Err("mutation_intent_failed".into());
            }
            let path = staging.join(relative_path);
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                    return Err("mutation_intent_failed".into());
                }
                Ok(_) => {
                    std::fs::remove_file(&path).map_err(|_| "mutation_intent_failed".to_string())?
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err("mutation_intent_failed".into()),
            }
        }
        match std::fs::remove_dir(&intent_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("mutation_intent_failed".into()),
        }
        connection
            .execute(
                "DELETE FROM mutation_chunks WHERE intent_id=?1",
                params![intent_id],
            )
            .map_err(|_| "mutation_intent_failed")?;
        std::fs::File::open(&staging)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| "mutation_intent_failed")?;
        Ok(())
    }

    pub(crate) fn create_pairing_source(
        &self,
        request_id: &str,
        session_id: &str,
        payload: &PairingPayload,
        now_ms: u64,
    ) -> Result<PairingSourceRecord, String> {
        self.reap_expired(now_ms, 256)?;
        if request_id.is_empty()
            || request_id.len() > 64
            || session_id.is_empty()
            || session_id.len() > 128
        {
            return Err("pairing_invalid_session".into());
        }
        let source =
            PairingSourceSecretV2::create(payload, now_ms).map_err(|error| error.to_string())?;
        let pair_id = source.pair_id().to_owned();
        let expires_at_ms = source.descriptor().expires_at_ms;
        let plaintext = source
            .to_secret_bytes()
            .map_err(|error| error.to_string())?;
        if plaintext.len() > MAX_PAIRING_SOURCE_BYTES {
            return Err("pairing_source_too_large".into());
        }
        let sealed = seal_row(&self.row_wrap_key, "pairing-source", &pair_id, &plaintext)?;
        let logical_bytes = i64::try_from(sealed.len()).map_err(|_| "pairing_unavailable")?;
        let mut connection = self.connection.lock().map_err(|_| "pairing_unavailable")?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "pairing_unavailable")?;
        let existing: Option<(String, String, i64)> = transaction
            .query_row(
                "SELECT pair_id,session_id,expires_at_ms FROM pairing_sources
                 WHERE request_id=?1 AND owner_binding=?2",
                params![request_id, self.installation_binding],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|_| "pairing_unavailable")?;
        if let Some((existing_pair_id, existing_session_id, existing_expires)) = existing {
            if existing_session_id != session_id {
                return Err("request_id_conflict".into());
            }
            return Ok(PairingSourceRecord {
                pair_id: existing_pair_id,
                expires_at_ms: u64::try_from(existing_expires)
                    .map_err(|_| "pairing_unavailable")?,
            });
        }
        enforce_control_quota(&transaction, logical_bytes)?;
        transaction
            .execute(
                "INSERT INTO pairing_sources(
                   pair_id,request_id,session_id,owner_binding,expires_at_ms,state,sealed_source,logical_bytes
                 ) VALUES(?1,?2,?3,?4,?5,'local',?6,?7)",
                params![
                    pair_id,
                    request_id,
                    session_id,
                    self.installation_binding,
                    u64_to_i64(expires_at_ms)?,
                    sealed,
                    logical_bytes,
                ],
            )
            .map_err(|_| "pairing_unavailable")?;
        transaction.commit().map_err(|_| "pairing_unavailable")?;
        Ok(PairingSourceRecord {
            pair_id,
            expires_at_ms,
        })
    }

    pub(crate) fn pairing_descriptor_cleanup_targets(
        &self,
        limit: usize,
    ) -> Result<Vec<PairingDescriptorCleanupTarget>, String> {
        let limit = i64::try_from(limit.min(256)).map_err(|_| "pairing_unavailable")?;
        let connection = self.connection.lock().map_err(|_| "pairing_unavailable")?;
        let mut statement = connection
            .prepare(
                "SELECT pair_id,session_id FROM pairing_sources
                 WHERE owner_binding=?1 AND state IN ('revoked','expired')
                 ORDER BY expires_at_ms,pair_id LIMIT ?2",
            )
            .map_err(|_| "pairing_unavailable")?;
        let rows = statement
            .query_map(params![self.installation_binding, limit], |row| {
                Ok(PairingDescriptorCleanupTarget {
                    pair_id: row.get(0)?,
                    session_id: row.get(1)?,
                })
            })
            .map_err(|_| "pairing_unavailable")?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|_| "pairing_unavailable".into())
    }

    pub(crate) fn consume_pairing_reveal(
        &self,
        operation_id: &str,
        route_request_id: &str,
        now_ms: u64,
    ) -> Result<PairingSourceSecretV2, String> {
        let mut connection = self.connection.lock().map_err(|_| "pairing_unavailable")?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "pairing_unavailable")?;
        let presence: Option<UserPresenceRow> = transaction
            .query_row(
                "SELECT state,kind,expires_at_ms,sealed_payload,sealed_response
                 FROM user_presence_intents
                 WHERE operation_id=?1 AND owner_binding=?2",
                params![operation_id, self.installation_binding],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| "presence_unavailable")?;
        let Some((state, kind, expires, sealed_presence, sealed_response)) = presence else {
            return Err("presence_not_found".into());
        };
        if kind != "session_pairing_reveal" {
            return Err("presence_not_authorized".into());
        }
        if now_ms > u64::try_from(expires).map_err(|_| "pairing_unavailable")? {
            return Err("presence_expired".into());
        }
        if state == "consumed" {
            let response = open_row(
                &self.row_wrap_key,
                "pairing-reveal-response",
                operation_id,
                sealed_response.as_deref().ok_or("pairing_unavailable")?,
            )?;
            let response: PairingRevealResponseV1 =
                serde_json::from_slice(&response).map_err(|_| "pairing_unavailable")?;
            if response.version != 1 || response.route_request_id != route_request_id {
                return Err("request_id_conflict".into());
            }
            let source_secret = URL_SAFE_NO_PAD
                .decode(response.source_secret)
                .map_err(|_| "pairing_unavailable")?;
            let source = PairingSourceSecretV2::from_secret_bytes(&source_secret)
                .map_err(|error| error.to_string())?;
            transaction.commit().map_err(|_| "pairing_unavailable")?;
            checkpoint_secure_erasure(&connection).map_err(|_| "pairing_unavailable")?;
            return Ok(source);
        }
        if state != "authorized" {
            return Err("presence_not_authorized".into());
        }
        let presence_plaintext = open_row(
            &self.row_wrap_key,
            "user-presence",
            operation_id,
            sealed_presence.as_deref().ok_or("presence_unavailable")?,
        )?;
        let presence_secret: UserPresenceSecretV1 =
            serde_json::from_slice(&presence_plaintext).map_err(|_| "presence_unavailable")?;
        let UserPresenceBinding::PairingReveal {
            session_id,
            pair_id,
        } = presence_secret.binding
        else {
            return Err("presence_not_authorized".into());
        };
        let row: Option<(String, String, i64, Option<Vec<u8>>)> = transaction
            .query_row(
                "SELECT session_id,state,expires_at_ms,sealed_source FROM pairing_sources
                 WHERE pair_id=?1 AND owner_binding=?2",
                params![pair_id, self.installation_binding],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(|_| "pairing_unavailable")?;
        let Some((stored_session, source_state, source_expires, sealed_source)) = row else {
            return Err("pairing_not_found".into());
        };
        if stored_session != session_id || !matches!(source_state.as_str(), "local" | "revealed") {
            return Err("pairing_not_found".into());
        }
        if now_ms > u64::try_from(source_expires).map_err(|_| "pairing_unavailable")? {
            return Err("pairing_expired".into());
        }
        let plaintext = open_row(
            &self.row_wrap_key,
            "pairing-source",
            &pair_id,
            sealed_source.as_deref().ok_or("pairing_unavailable")?,
        )?;
        let source = PairingSourceSecretV2::from_secret_bytes(&plaintext)
            .map_err(|error| error.to_string())?;
        let response = serde_json::to_vec(&PairingRevealResponseV1 {
            version: 1,
            route_request_id: route_request_id.to_owned(),
            source_secret: URL_SAFE_NO_PAD.encode(&plaintext),
        })
        .map_err(|_| "pairing_unavailable")?;
        let response = seal_row(
            &self.row_wrap_key,
            "pairing-reveal-response",
            operation_id,
            &response,
        )?;
        let response_bytes = i64::try_from(response.len()).map_err(|_| "pairing_unavailable")?;
        let terminal_expires_at_ms =
            control_tombstone_expiry(now_ms).map_err(|_| "pairing_unavailable".to_string())?;
        enforce_control_quota(&transaction, response_bytes)?;
        transaction
            .execute(
                "UPDATE pairing_sources SET state='revealed'
                 WHERE pair_id=?1 AND state IN ('local','revealed')",
                params![pair_id],
            )
            .map_err(|_| "pairing_unavailable")?;
        let changed = transaction
            .execute(
                "UPDATE user_presence_intents
                 SET state='consumed',sealed_payload=NULL,sealed_response=?2,
                     terminal_expires_at_ms=?3,logical_bytes=?4
                 WHERE operation_id=?1 AND state='authorized'",
                params![
                    operation_id,
                    response,
                    terminal_expires_at_ms,
                    response_bytes
                ],
            )
            .map_err(|_| "pairing_unavailable")?;
        if changed != 1 {
            return Err("presence_not_authorized".into());
        }
        transaction.commit().map_err(|_| "pairing_unavailable")?;
        checkpoint_secure_erasure(&connection).map_err(|_| "pairing_unavailable")?;
        Ok(source)
    }

    pub(crate) fn pairing_import_code(&self, operation_id: &str) -> Result<PairingCodeV2, String> {
        let connection = self.connection.lock().map_err(|_| "pairing_unavailable")?;
        let row: Option<(String, Option<Vec<u8>>)> = connection
            .query_row(
                "SELECT state,sealed_payload FROM user_presence_intents
                 WHERE operation_id=?1 AND owner_binding=?2 AND kind='session_pairing_import'",
                params![operation_id, self.installation_binding],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|_| "pairing_unavailable")?;
        let Some((state, sealed)) = row else {
            return Err("presence_not_found".into());
        };
        if state != "authorized" {
            return Err("presence_not_authorized".into());
        }
        let plaintext = open_row(
            &self.row_wrap_key,
            "user-presence",
            operation_id,
            sealed.as_deref().ok_or("presence_unavailable")?,
        )?;
        let secret: UserPresenceSecretV1 =
            serde_json::from_slice(&plaintext).map_err(|_| "presence_unavailable")?;
        let UserPresenceBinding::PairingImport { pairing_code } = secret.binding else {
            return Err("presence_not_authorized".into());
        };
        PairingCodeV2::parse(&pairing_code).map_err(|error| error.to_string())
    }

    pub(crate) fn begin_pairing_revoke(
        &self,
        operation_id: &str,
        request_id: &str,
    ) -> Result<PairingSourceSecretV2, String> {
        let mut connection = self.connection.lock().map_err(|_| "pairing_unavailable")?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "pairing_unavailable")?;
        let existing: Option<(String, String, String)> = transaction
            .query_row(
                "SELECT request_id,pair_id,state FROM pairing_revocations
                 WHERE operation_id=?1 AND owner_binding=?2",
                params![operation_id, self.installation_binding],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|_| "pairing_unavailable")?;
        let pair_id = if let Some((stored_request, pair_id, state)) = existing {
            if stored_request != request_id {
                return Err("request_id_conflict".into());
            }
            if state == "completed" {
                return Err("pairing_revoked".into());
            }
            pair_id
        } else {
            let sealed_response: Option<Vec<u8>> = transaction
                .query_row(
                    "SELECT sealed_response FROM user_presence_intents
                     WHERE operation_id=?1 AND owner_binding=?2
                       AND kind='session_pairing_reveal' AND state='consumed'",
                    params![operation_id, self.installation_binding],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|_| "pairing_unavailable")?
                .flatten();
            let response = open_row(
                &self.row_wrap_key,
                "pairing-reveal-response",
                operation_id,
                sealed_response.as_deref().ok_or("pairing_not_found")?,
            )?;
            let response: PairingRevealResponseV1 =
                serde_json::from_slice(&response).map_err(|_| "pairing_unavailable")?;
            if response.version != 1 {
                return Err("pairing_unavailable".into());
            }
            let source_secret = URL_SAFE_NO_PAD
                .decode(response.source_secret)
                .map_err(|_| "pairing_unavailable")?;
            let source = PairingSourceSecretV2::from_secret_bytes(&source_secret)
                .map_err(|error| error.to_string())?;
            let pair_id = source.pair_id().to_owned();
            transaction
                .execute(
                    "INSERT INTO pairing_revocations(
                       operation_id,request_id,pair_id,owner_binding,state,logical_bytes
                     ) VALUES(?1,?2,?3,?4,'in_flight',0)",
                    params![operation_id, request_id, pair_id, self.installation_binding],
                )
                .map_err(|_| "pairing_unavailable")?;
            pair_id
        };
        let sealed_source: Option<Vec<u8>> = transaction
            .query_row(
                "SELECT sealed_source FROM pairing_sources
                 WHERE pair_id=?1 AND owner_binding=?2 AND state IN ('local','revealed')",
                params![pair_id, self.installation_binding],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| "pairing_unavailable")?
            .flatten();
        let source = open_row(
            &self.row_wrap_key,
            "pairing-source",
            &pair_id,
            sealed_source.as_deref().ok_or("pairing_not_found")?,
        )?;
        let source =
            PairingSourceSecretV2::from_secret_bytes(&source).map_err(|error| error.to_string())?;
        transaction.commit().map_err(|_| "pairing_unavailable")?;
        Ok(source)
    }

    pub(crate) fn complete_pairing_revoke(
        &self,
        operation_id: &str,
        request_id: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|_| "pairing_unavailable")?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "pairing_unavailable")?;
        let row: Option<(String, String, String)> = transaction
            .query_row(
                "SELECT request_id,pair_id,state FROM pairing_revocations
                 WHERE operation_id=?1 AND owner_binding=?2",
                params![operation_id, self.installation_binding],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|_| "pairing_unavailable")?;
        let Some((stored_request, pair_id, state)) = row else {
            return Err("pairing_not_found".into());
        };
        if stored_request != request_id {
            return Err("request_id_conflict".into());
        }
        if state == "completed" {
            transaction.commit().map_err(|_| "pairing_unavailable")?;
            checkpoint_secure_erasure(&connection).map_err(|_| "pairing_unavailable")?;
            return Ok(());
        }
        let terminal_expires_at_ms =
            control_tombstone_expiry(now_ms).map_err(|_| "pairing_unavailable".to_string())?;
        transaction
            .execute(
                "UPDATE pairing_sources
                 SET state='revoked',sealed_source=NULL,terminal_expires_at_ms=?3,logical_bytes=0
                 WHERE pair_id=?1 AND owner_binding=?2",
                params![pair_id, self.installation_binding, terminal_expires_at_ms],
            )
            .map_err(|_| "pairing_unavailable")?;
        transaction
            .execute(
                "UPDATE user_presence_intents SET sealed_response=NULL,logical_bytes=0
                 WHERE operation_id=?1 AND owner_binding=?2",
                params![operation_id, self.installation_binding],
            )
            .map_err(|_| "pairing_unavailable")?;
        transaction
            .execute(
                "UPDATE pairing_revocations SET state='completed',terminal_expires_at_ms=?2
                 WHERE operation_id=?1 AND state='in_flight'",
                params![operation_id, terminal_expires_at_ms],
            )
            .map_err(|_| "pairing_unavailable")?;
        transaction
            .commit()
            .map_err(|_| "pairing_unavailable".to_string())?;
        checkpoint_secure_erasure(&connection).map_err(|_| "pairing_unavailable".to_string())
    }

    pub(crate) fn begin_pairing_claim(
        &self,
        request_id: &str,
        operation_id: &str,
        descriptor: &PairingDescriptorV2,
        installation_principal: &str,
        now_ms: u64,
    ) -> Result<(PairingCodeV2, PairingClaimV2), String> {
        match self.load_pairing_claim_for_request(
            operation_id,
            request_id,
            installation_principal,
            now_ms,
        ) {
            Ok(existing) => {
                let connection = self.connection.lock().map_err(|_| "pairing_unavailable")?;
                checkpoint_secure_erasure(&connection).map_err(|_| "pairing_unavailable")?;
                return Ok(existing);
            }
            Err(error) if error == "pairing_not_found" => {}
            Err(error) => return Err(error),
        }
        let code = self.pairing_import_code(operation_id)?;
        let claim = PairingClaimV2::claim(&code, descriptor, installation_principal, now_ms)
            .map_err(|error| error.to_string())?;
        let resume = claim
            .to_resume_bytes(&code)
            .map_err(|error| error.to_string())?;
        let sealed = seal_row(&self.row_wrap_key, "pairing-claim", operation_id, &resume)?;
        let logical_bytes = i64::try_from(sealed.len()).map_err(|_| "pairing_unavailable")?;
        let resume_expires_at_ms = now_ms
            .checked_add(PAIRING_CLAIM_RESUME_TTL_MS)
            .ok_or_else(|| "pairing_unavailable".to_string())?;
        let mut connection = self.connection.lock().map_err(|_| "pairing_unavailable")?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "pairing_unavailable")?;
        enforce_control_quota(&transaction, logical_bytes)?;
        transaction
            .execute(
                "INSERT INTO pairing_claims(
                   operation_id,request_id,pair_id,owner_binding,state,resume_expires_at_ms,
                   installed_session_id,finalize_request_id,sealed_resume,logical_bytes
                 ) VALUES(?1,?2,?3,?4,'claimed',?5,NULL,NULL,?6,?7)",
                params![
                    operation_id,
                    request_id,
                    code.pair_id(),
                    self.installation_binding,
                    u64_to_i64(resume_expires_at_ms)?,
                    sealed,
                    logical_bytes,
                ],
            )
            .map_err(|_| "pairing_unavailable")?;
        let changed = transaction
            .execute(
                "UPDATE user_presence_intents
                 SET state='consumed',sealed_payload=NULL,terminal_expires_at_ms=?2,logical_bytes=0
                 WHERE operation_id=?1 AND state='authorized' AND kind='session_pairing_import'",
                params![
                    operation_id,
                    control_tombstone_expiry(now_ms)
                        .map_err(|_| "pairing_unavailable".to_string())?
                ],
            )
            .map_err(|_| "pairing_unavailable")?;
        if changed != 1 {
            return Err("presence_not_authorized".into());
        }
        transaction.commit().map_err(|_| "pairing_unavailable")?;
        checkpoint_secure_erasure(&connection).map_err(|_| "pairing_unavailable")?;
        Ok((code, claim))
    }

    pub(crate) fn load_pairing_claim(
        &self,
        operation_id: &str,
        installation_principal: &str,
        now_ms: u64,
    ) -> Result<(PairingCodeV2, PairingClaimV2), String> {
        let connection = self.connection.lock().map_err(|_| "pairing_unavailable")?;
        let sealed: Option<Vec<u8>> = connection
            .query_row(
                "SELECT sealed_resume FROM pairing_claims
                 WHERE operation_id=?1 AND owner_binding=?2
                   AND state IN ('claimed','installed') AND resume_expires_at_ms>=?3",
                params![operation_id, self.installation_binding, u64_to_i64(now_ms)?],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| "pairing_unavailable")?
            .flatten();
        let resume = open_row(
            &self.row_wrap_key,
            "pairing-claim",
            operation_id,
            sealed.as_deref().ok_or("pairing_not_found")?,
        )?;
        PairingClaimV2::from_resume_bytes(&resume, installation_principal, now_ms)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn load_pairing_claim_for_request(
        &self,
        operation_id: &str,
        request_id: &str,
        installation_principal: &str,
        now_ms: u64,
    ) -> Result<(PairingCodeV2, PairingClaimV2), String> {
        let connection = self.connection.lock().map_err(|_| "pairing_unavailable")?;
        let stored_request: Option<String> = connection
            .query_row(
                "SELECT request_id FROM pairing_claims
                 WHERE operation_id=?1 AND owner_binding=?2",
                params![operation_id, self.installation_binding],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| "pairing_unavailable")?;
        drop(connection);
        let Some(stored_request) = stored_request else {
            return Err("pairing_not_found".into());
        };
        if stored_request != request_id {
            return Err("request_id_conflict".into());
        }
        self.load_pairing_claim(operation_id, installation_principal, now_ms)
    }

    pub(crate) fn mark_pairing_claim_installed(
        &self,
        operation_id: &str,
        session_id: &str,
    ) -> Result<(), String> {
        if session_id.is_empty() || session_id.len() > 128 {
            return Err("pairing_invalid_session".into());
        }
        let connection = self.connection.lock().map_err(|_| "pairing_unavailable")?;
        let changed = connection
            .execute(
                "UPDATE pairing_claims SET state='installed',installed_session_id=?3
                 WHERE operation_id=?1 AND owner_binding=?2
                   AND (state='claimed' OR (state='installed' AND installed_session_id=?3))",
                params![operation_id, self.installation_binding, session_id],
            )
            .map_err(|_| "pairing_unavailable")?;
        if changed != 1 {
            return Err("pairing_not_found".into());
        }
        Ok(())
    }

    pub(crate) fn complete_pairing_claim(
        &self,
        operation_id: &str,
        request_id: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        let connection = self.connection.lock().map_err(|_| "pairing_unavailable")?;
        let row: Option<(String, Option<String>)> = connection
            .query_row(
                "SELECT state,finalize_request_id FROM pairing_claims
                 WHERE operation_id=?1 AND owner_binding=?2",
                params![operation_id, self.installation_binding],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|_| "pairing_unavailable")?;
        let Some((state, stored_request)) = row else {
            return Err("pairing_not_found".into());
        };
        if state == "consumed" {
            if stored_request.as_deref() != Some(request_id) {
                return Err("request_id_conflict".into());
            }
            checkpoint_secure_erasure(&connection).map_err(|_| "pairing_unavailable")?;
            return Ok(());
        }
        if stored_request
            .as_deref()
            .is_some_and(|stored| stored != request_id)
        {
            return Err("request_id_conflict".into());
        }
        if stored_request.as_deref() != Some(request_id) {
            return Err("pairing_outcome_unknown".into());
        }
        let terminal_expires_at_ms =
            control_tombstone_expiry(now_ms).map_err(|_| "pairing_unavailable".to_string())?;
        let changed = connection
            .execute(
                "UPDATE pairing_claims
                 SET state='consumed',finalize_request_id=?3,sealed_resume=NULL,
                     terminal_expires_at_ms=?4,logical_bytes=0
                 WHERE operation_id=?1 AND owner_binding=?2 AND state='installed'
                   AND finalize_request_id=?3",
                params![
                    operation_id,
                    self.installation_binding,
                    request_id,
                    terminal_expires_at_ms
                ],
            )
            .map_err(|_| "pairing_unavailable")?;
        if changed != 1 {
            return Err("pairing_not_found".into());
        }
        checkpoint_secure_erasure(&connection).map_err(|_| "pairing_unavailable".into())
    }

    pub(crate) fn pairing_claim_remote_consumed(
        &self,
        operation_id: &str,
        request_id: &str,
    ) -> Result<bool, String> {
        let connection = self.connection.lock().map_err(|_| "pairing_unavailable")?;
        let row: Option<(String, Option<String>)> = connection
            .query_row(
                "SELECT state,finalize_request_id FROM pairing_claims
                 WHERE operation_id=?1 AND owner_binding=?2",
                params![operation_id, self.installation_binding],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|_| "pairing_unavailable")?;
        let Some((state, stored_request)) = row else {
            return Err("pairing_not_found".into());
        };
        if stored_request
            .as_deref()
            .is_some_and(|stored| stored != request_id)
        {
            return Err("request_id_conflict".into());
        }
        match state.as_str() {
            "installed" => Ok(stored_request.is_some()),
            "consumed" => Ok(true),
            _ => Err("pairing_not_found".into()),
        }
    }

    pub(crate) fn mark_pairing_claim_remote_consumed(
        &self,
        operation_id: &str,
        request_id: &str,
    ) -> Result<(), String> {
        let connection = self.connection.lock().map_err(|_| "pairing_unavailable")?;
        let changed = connection
            .execute(
                "UPDATE pairing_claims SET finalize_request_id=?3
                 WHERE operation_id=?1 AND owner_binding=?2 AND state='installed'
                   AND (finalize_request_id IS NULL OR finalize_request_id=?3)",
                params![operation_id, self.installation_binding, request_id],
            )
            .map_err(|_| "pairing_unavailable")?;
        if changed == 1 {
            return Ok(());
        }
        drop(connection);
        self.pairing_claim_remote_consumed(operation_id, request_id)
            .and_then(|confirmed| {
                confirmed
                    .then_some(())
                    .ok_or_else(|| "pairing_not_found".into())
            })
    }

    pub(crate) fn pairing_claim_state(
        &self,
        operation_id: &str,
        request_id: Option<&str>,
    ) -> Result<Option<String>, String> {
        let connection = self.connection.lock().map_err(|_| "pairing_unavailable")?;
        let row: Option<(String, String)> = connection
            .query_row(
                "SELECT request_id,state FROM pairing_claims
                 WHERE operation_id=?1 AND owner_binding=?2",
                params![operation_id, self.installation_binding],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|_| "pairing_unavailable")?;
        let Some((stored_request, state)) = row else {
            return Ok(None);
        };
        if request_id.is_some_and(|request_id| request_id != stored_request) {
            return Err("request_id_conflict".into());
        }
        Ok(Some(state))
    }

    pub(crate) fn pairing_claim_result_session(
        &self,
        operation_id: &str,
        request_id: &str,
    ) -> Result<Option<String>, String> {
        let connection = self.connection.lock().map_err(|_| "pairing_unavailable")?;
        let row: Option<(String, Option<String>)> = connection
            .query_row(
                "SELECT request_id,installed_session_id FROM pairing_claims
                 WHERE operation_id=?1 AND owner_binding=?2",
                params![operation_id, self.installation_binding],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|_| "pairing_unavailable")?;
        let Some((stored_request, session_id)) = row else {
            return Ok(None);
        };
        if stored_request != request_id {
            return Err("request_id_conflict".into());
        }
        Ok(session_id)
    }

    pub(crate) fn start_user_presence(
        &self,
        request_id: &str,
        binding: UserPresenceBinding,
        now_ms: u64,
    ) -> Result<UserPresenceChallenge, String> {
        self.reap_expired(now_ms, 256)?;
        if request_id.is_empty() || request_id.len() > 64 {
            return Err("presence_invalid_request".into());
        }
        let operation_id = random_b64(16)?;
        let intent_id = random_b64(16)?;
        let token = random_b64(32)?;
        let expires_at_ms = now_ms
            .checked_add(USER_PRESENCE_TTL_MS)
            .ok_or_else(|| "presence_invalid_request".to_string())?;
        let public_digest = binding.public_binding_digest();
        let action_hash = presence_action_hash(binding.kind(), &public_digest, expires_at_ms);
        let token_hash = presence_token_hash(&token);
        let payload = serde_json::to_vec(&UserPresenceSecretV1 {
            version: 1,
            binding: binding.clone(),
            token_hash,
        })
        .map_err(|_| "presence_unavailable")?;
        let sealed = seal_row(&self.row_wrap_key, "user-presence", &operation_id, &payload)?;
        let logical_bytes = i64::try_from(sealed.len()).map_err(|_| "presence_unavailable")?;
        let mut connection = self.connection.lock().map_err(|_| "presence_unavailable")?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "presence_unavailable")?;
        enforce_control_quota(&transaction, logical_bytes)?;
        transaction
            .execute(
                "INSERT INTO user_presence_intents(
                   operation_id,intent_id,request_id,owner_binding,kind,action_hash,
                   expires_at_ms,state,sealed_payload,sealed_response,logical_bytes
                 ) VALUES(?1,?2,?3,?4,?5,?6,?7,'pending',?8,NULL,?9)",
                params![
                    operation_id,
                    intent_id,
                    request_id,
                    self.installation_binding,
                    binding.kind(),
                    action_hash,
                    u64_to_i64(expires_at_ms)?,
                    sealed,
                    logical_bytes,
                ],
            )
            .map_err(|_| "presence_unavailable")?;
        transaction.commit().map_err(|_| "presence_unavailable")?;
        Ok(UserPresenceChallenge {
            operation_id,
            intent_id,
            token,
            action_hash,
            expires_at_ms,
        })
    }

    pub(crate) fn confirm_user_presence(
        &self,
        operation_id: &str,
        intent_id: &str,
        token: &str,
        action_hash: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        let connection = self.connection.lock().map_err(|_| "presence_unavailable")?;
        let row: Option<(String, i64, String, Option<Vec<u8>>)> = connection
            .query_row(
                "SELECT state,expires_at_ms,action_hash,sealed_payload
                 FROM user_presence_intents
                 WHERE operation_id=?1 AND intent_id=?2 AND owner_binding=?3",
                params![operation_id, intent_id, self.installation_binding],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(|_| "presence_unavailable")?;
        let Some((state, expires, expected_action_hash, sealed)) = row else {
            return Err("presence_not_found".into());
        };
        if !matches!(state.as_str(), "pending" | "authorized") {
            return Err("presence_not_found".into());
        }
        if now_ms > u64::try_from(expires).map_err(|_| "presence_unavailable")? {
            return Err("presence_expired".into());
        }
        if !constant_time_eq(action_hash.as_bytes(), expected_action_hash.as_bytes()) {
            return Err("presence_action_mismatch".into());
        }
        let payload = open_row(
            &self.row_wrap_key,
            "user-presence",
            operation_id,
            sealed.as_deref().ok_or("presence_unavailable")?,
        )?;
        let secret: UserPresenceSecretV1 =
            serde_json::from_slice(&payload).map_err(|_| "presence_unavailable")?;
        if secret.version != 1
            || !constant_time_eq(
                presence_token_hash(token).as_bytes(),
                secret.token_hash.as_bytes(),
            )
        {
            return Err("presence_token_invalid".into());
        }
        if state == "authorized" {
            return Ok(());
        }
        let changed = connection
            .execute(
                "UPDATE user_presence_intents SET state='authorized'
                 WHERE operation_id=?1 AND state='pending'",
                params![operation_id],
            )
            .map_err(|_| "presence_unavailable")?;
        if changed == 1 {
            Ok(())
        } else {
            Err("presence_not_found".into())
        }
    }

    pub(crate) fn consume_user_presence(
        &self,
        operation_id: &str,
        expected_kind: &str,
        route_request_id: &str,
        now_ms: u64,
    ) -> Result<UserPresenceBinding, String> {
        if route_request_id.is_empty() || route_request_id.len() > 64 {
            return Err("request_id_conflict".into());
        }
        let mut connection = self.connection.lock().map_err(|_| "presence_unavailable")?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "presence_unavailable")?;
        let row: Option<UserPresenceRow> = transaction
            .query_row(
                "SELECT state,kind,expires_at_ms,sealed_payload,sealed_response
                 FROM user_presence_intents
                 WHERE operation_id=?1 AND owner_binding=?2",
                params![operation_id, self.installation_binding],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| "presence_unavailable")?;
        let Some((state, kind, expires, sealed, sealed_response)) = row else {
            return Err("presence_not_found".into());
        };
        if kind != expected_kind {
            return Err("presence_not_authorized".into());
        }
        if state == "consumed" {
            let response = open_row(
                &self.row_wrap_key,
                "user-presence-response",
                operation_id,
                sealed_response.as_deref().ok_or("presence_unavailable")?,
            )?;
            let response: UserPresenceConsumptionV1 =
                serde_json::from_slice(&response).map_err(|_| "presence_unavailable")?;
            if response.version != 1
                || response.route_request_id != route_request_id
                || response.binding.kind() != expected_kind
            {
                return Err("request_id_conflict".into());
            }
            transaction.commit().map_err(|_| "presence_unavailable")?;
            checkpoint_secure_erasure(&connection).map_err(|_| "presence_unavailable")?;
            return Ok(response.binding);
        }
        if state != "authorized" {
            return Err("presence_not_authorized".into());
        }
        if now_ms > u64::try_from(expires).map_err(|_| "presence_unavailable")? {
            return Err("presence_expired".into());
        }
        let payload = open_row(
            &self.row_wrap_key,
            "user-presence",
            operation_id,
            sealed.as_deref().ok_or("presence_unavailable")?,
        )?;
        let secret: UserPresenceSecretV1 =
            serde_json::from_slice(&payload).map_err(|_| "presence_unavailable")?;
        if secret.version != 1 || secret.binding.kind() != expected_kind {
            return Err("presence_unavailable".into());
        }
        let response = serde_json::to_vec(&UserPresenceConsumptionV1 {
            version: 1,
            route_request_id: route_request_id.to_owned(),
            binding: secret.binding.clone(),
        })
        .map_err(|_| "presence_unavailable")?;
        let sealed_response = seal_row(
            &self.row_wrap_key,
            "user-presence-response",
            operation_id,
            &response,
        )?;
        let terminal_expires_at_ms =
            control_tombstone_expiry(now_ms).map_err(|_| "presence_unavailable".to_string())?;
        enforce_control_quota(
            &transaction,
            i64::try_from(sealed_response.len()).map_err(|_| "presence_unavailable")?,
        )?;
        let changed = transaction
            .execute(
                "UPDATE user_presence_intents
                 SET state='consumed',sealed_payload=NULL,sealed_response=?2,
                     terminal_expires_at_ms=?3,logical_bytes=?4
                 WHERE operation_id=?1 AND state='authorized'",
                params![
                    operation_id,
                    sealed_response,
                    terminal_expires_at_ms,
                    i64::try_from(sealed_response.len()).map_err(|_| "presence_unavailable")?
                ],
            )
            .map_err(|_| "presence_unavailable")?;
        if changed == 1 {
            transaction.commit().map_err(|_| "presence_unavailable")?;
            checkpoint_secure_erasure(&connection).map_err(|_| "presence_unavailable")?;
            Ok(secret.binding)
        } else {
            Err("presence_not_authorized".into())
        }
    }

    fn load_pending(&self, intent_id: &str) -> Result<Option<PendingRow>, ConfirmError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ConfirmError::Unavailable)?;
        connection
            .query_row(
                "SELECT state, expires_at_ms, action_hash, sealed_payload
                 FROM confirmation_intents
                 WHERE intent_id=?1 AND owner_binding=?2",
                params![intent_id, self.installation_binding],
                |row| {
                    let expires: i64 = row.get(1)?;
                    Ok((
                        row.get(0)?,
                        u64::try_from(expires)
                            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(1, expires))?,
                        row.get(2)?,
                        row.get(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| ConfirmError::Unavailable)
    }

    fn decrypt_pending(
        &self,
        intent_id: &str,
        sealed: &[u8],
    ) -> Result<PendingSecretV1, ConfirmError> {
        let plaintext = open_row(&self.row_wrap_key, "agent-tool", intent_id, sealed)
            .map_err(|_| ConfirmError::Unavailable)?;
        if plaintext.len() > MAX_PENDING_PLAINTEXT {
            return Err(ConfirmError::Unavailable);
        }
        let pending: PendingSecretV1 =
            serde_json::from_slice(&plaintext).map_err(|_| ConfirmError::Unavailable)?;
        (pending.version == 1)
            .then_some(pending)
            .ok_or(ConfirmError::Unavailable)
    }

    fn erase_with_state(
        &self,
        intent_id: &str,
        state: &str,
        now_ms: u64,
    ) -> Result<(), ConfirmError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ConfirmError::Unavailable)?;
        let changed = connection
            .execute(
                "UPDATE confirmation_intents
                 SET state=?1, sealed_payload=NULL, terminal_expires_at_ms=?4, logical_bytes=0
                 WHERE intent_id=?2 AND owner_binding=?3 AND state='pending'",
                params![
                    state,
                    intent_id,
                    self.installation_binding,
                    control_tombstone_expiry(now_ms).map_err(|_| ConfirmError::Unavailable)?
                ],
            )
            .map_err(|_| ConfirmError::Unavailable)?;
        if changed != 1 {
            return Err(ConfirmError::NotFound);
        }
        checkpoint_secure_erasure(&connection).map_err(|_| ConfirmError::Unavailable)
    }

    pub(crate) fn bind_product_request(
        &self,
        identity: &isyncyou_webui::ProductRequestIdentity,
    ) -> Result<ProductRequestBinding, String> {
        if identity.request_id.len() > 64
            || identity.route_domain.len() > 160
            || identity.request_scope.is_empty()
            || identity.request_scope.len() > 256
            || !valid_sha256(&identity.payload_digest)
        {
            return Err("request_store_unavailable".into());
        }
        let now_ms = crate::unix_now_ms();
        self.reap_expired(now_ms, 256)?;
        // A turn UUID is scoped to an encrypted session and must remain bound for
        // that session's lifetime. Ordinary app-wide mutation receipts retain the
        // bounded 30-day policy; turn bindings are quota-bounded and survive until
        // the app profile is explicitly reset.
        let expires_at_ms = if identity.route_domain == AGENT_TURN_ROUTE_DOMAIN {
            i64::MAX
        } else {
            u64_to_i64(
                now_ms
                    .checked_add(PRODUCT_REQUEST_RECEIPT_TTL_MS)
                    .ok_or_else(|| "request_store_unavailable".to_string())?,
            )?
        };
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "request_store_unavailable")?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "request_store_unavailable")?;
        let result = bind_product_request_transaction(
            &transaction,
            identity,
            expires_at_ms,
            &self.installation_principal,
        )?;
        transaction
            .commit()
            .map_err(|_| "request_store_unavailable")?;
        Ok(result)
    }

    pub(crate) fn begin_product_request_identity(
        &self,
        identity: &isyncyou_webui::ProductRequestIdentity,
    ) -> Result<ProductRequestBegin, String> {
        if !identity.permits_durable_response() {
            return Err("request_response_policy_violation".into());
        }
        match self.bind_product_request(identity)? {
            ProductRequestBinding::Conflict => return Ok(ProductRequestBegin::Conflict),
            ProductRequestBinding::Inserted | ProductRequestBinding::Existing => {}
        }
        let now_ms = crate::unix_now_ms();
        let expires_at_ms = if identity.route_domain == AGENT_TURN_ROUTE_DOMAIN {
            i64::MAX
        } else {
            u64_to_i64(
                now_ms
                    .checked_add(PRODUCT_REQUEST_RECEIPT_TTL_MS)
                    .ok_or_else(|| "request_store_unavailable".to_string())?,
            )?
        };
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "request_store_unavailable")?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "request_store_unavailable")?;
        let request_key = product_request_key(identity, &self.installation_principal)?;
        let existing: Option<ProductRequestReceiptRow> = transaction
            .query_row(
                "SELECT route_domain,request_scope,payload_digest,state,sealed_response
                 FROM product_request_receipts
                 WHERE request_key=?1 AND request_id=?2",
                params![request_key, identity.request_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| "request_store_unavailable")?;
        if let Some((stored_route, stored_scope, stored_digest, state, sealed_response)) = existing
        {
            if stored_route != identity.route_domain
                || stored_scope != identity.request_scope
                || stored_digest != identity.payload_digest
            {
                return Ok(ProductRequestBegin::Conflict);
            }
            if state == "started" {
                return Ok(ProductRequestBegin::OutcomeUnknown);
            }
            let sealed = sealed_response.ok_or("request_store_unavailable")?;
            let plaintext = open_row(
                &self.row_wrap_key,
                "product-request-response",
                &identity.request_id,
                &sealed,
            )?;
            let response: StoredProductResponseV1 =
                serde_json::from_slice(&plaintext).map_err(|_| "request_store_unavailable")?;
            return Ok(ProductRequestBegin::Replay(response));
        }
        let logical_bytes = i64::try_from(
            identity
                .request_id
                .len()
                .saturating_add(request_key.len())
                .saturating_add(identity.route_domain.len())
                .saturating_add(identity.request_scope.len())
                .saturating_add(identity.payload_digest.len())
                .saturating_add(128),
        )
        .map_err(|_| "request_store_unavailable")?;
        enforce_control_quota(&transaction, logical_bytes)?;
        transaction
            .execute(
                "INSERT INTO product_request_receipts(
                   request_id,request_key,route_domain,request_scope,payload_digest,
                   state,sealed_response,expires_at_ms,logical_bytes
                 ) VALUES(?1,?2,?3,?4,?5,'started',NULL,?6,?7)",
                params![
                    identity.request_id,
                    request_key,
                    identity.route_domain,
                    identity.request_scope,
                    identity.payload_digest,
                    expires_at_ms,
                    logical_bytes
                ],
            )
            .map_err(|_| "request_store_unavailable")?;
        transaction
            .commit()
            .map_err(|_| "request_store_unavailable")?;
        Ok(ProductRequestBegin::Execute)
    }

    pub(crate) fn complete_product_request_identity(
        &self,
        identity: &isyncyou_webui::ProductRequestIdentity,
        response: &StoredProductResponseV1,
    ) -> Result<(), String> {
        if !identity.permits_durable_response() {
            return Err("request_response_policy_violation".into());
        }
        let plaintext = serde_json::to_vec(response).map_err(|_| "request_store_unavailable")?;
        if plaintext.len() > 1024 * 1024 || response.headers.len() > 32 {
            return Err("request_store_unavailable".into());
        }
        let sealed = seal_row(
            &self.row_wrap_key,
            "product-request-response",
            &identity.request_id,
            &plaintext,
        )?;
        let request_key = product_request_key(identity, &self.installation_principal)?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "request_store_unavailable")?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "request_store_unavailable")?;
        let previous_bytes: i64 = transaction
            .query_row(
                "SELECT logical_bytes FROM product_request_receipts
                 WHERE request_key=?1 AND request_id=?2 AND route_domain=?3
                   AND request_scope=?4 AND payload_digest=?5
                   AND state='started'",
                params![
                    request_key,
                    identity.request_id,
                    identity.route_domain,
                    identity.request_scope,
                    identity.payload_digest
                ],
                |row| row.get(0),
            )
            .map_err(|_| "request_store_unavailable")?;
        let next_bytes = i64::try_from(sealed.len().saturating_add(256))
            .map_err(|_| "request_store_unavailable")?;
        enforce_control_quota(&transaction, next_bytes.saturating_sub(previous_bytes))?;
        let changed = transaction
            .execute(
                "UPDATE product_request_receipts
                 SET state='completed',sealed_response=?1,logical_bytes=?2
                 WHERE request_key=?3 AND request_id=?4 AND route_domain=?5
                   AND request_scope=?6 AND payload_digest=?7
                   AND state='started'",
                params![
                    sealed,
                    next_bytes,
                    request_key,
                    identity.request_id,
                    identity.route_domain,
                    identity.request_scope,
                    identity.payload_digest
                ],
            )
            .map_err(|_| "request_store_unavailable")?;
        if changed != 1 {
            return Err("request_store_unavailable".into());
        }
        transaction
            .commit()
            .map_err(|_| "request_store_unavailable".to_string())
    }

    pub(crate) fn abort_product_request_identity(
        &self,
        identity: &isyncyou_webui::ProductRequestIdentity,
    ) -> Result<(), String> {
        if !identity.permits_durable_response() {
            return Err("request_response_policy_violation".into());
        }
        let request_key = product_request_key(identity, &self.installation_principal)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| "request_store_unavailable")?;
        connection
            .execute(
                "DELETE FROM product_request_receipts
                 WHERE request_key=?1 AND request_id=?2 AND route_domain=?3
                   AND request_scope=?4 AND payload_digest=?5
                   AND state='started'",
                params![
                    request_key,
                    identity.request_id,
                    identity.route_domain,
                    identity.request_scope,
                    identity.payload_digest
                ],
            )
            .map(|_| ())
            .map_err(|_| "request_store_unavailable".into())
    }

    pub(crate) fn begin_agent_turn_admission_identity(
        &self,
        request: &isyncyou_webui::AgentTurnRequest,
        turn_id: &str,
        identity: &isyncyou_webui::ProductRequestIdentity,
    ) -> Result<AgentTurnAdmissionBegin, String> {
        if request.request_id != identity.request_id
            || identity.route_domain != AGENT_TURN_ROUTE_DOMAIN
            || identity.request_scope != format!("session_id:{}", request.session_id)
            || request.request_id.len() > 64
            || turn_id.len() > 128
            || !valid_sha256(&identity.payload_digest)
        {
            return Err("turn_admission_unavailable".into());
        }
        let record = AgentTurnAdmissionV1 {
            version: 2,
            turn_id: turn_id.to_owned(),
            route_domain: identity.route_domain.to_owned(),
            request_scope: identity.request_scope.clone(),
            payload_digest: identity.payload_digest.clone(),
            request: request.clone(),
        };
        let plaintext = serde_json::to_vec(&record).map_err(|_| "turn_admission_unavailable")?;
        if plaintext.len() > MAX_AGENT_TURN_ADMISSION_BYTES {
            return Err("turn_admission_unavailable".into());
        }
        let sealed = seal_row(
            &self.row_wrap_key,
            "agent-turn-admission",
            &request.request_id,
            &plaintext,
        )?;
        let logical_bytes = i64::try_from(sealed.len().saturating_add(256))
            .map_err(|_| "turn_admission_unavailable")?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "turn_admission_unavailable")?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "turn_admission_unavailable")?;
        match bind_product_request_transaction(
            &transaction,
            identity,
            i64::MAX,
            &self.installation_principal,
        )
        .map_err(|_| "turn_admission_unavailable")?
        {
            ProductRequestBinding::Conflict => return Err("request_id_conflict".into()),
            ProductRequestBinding::Inserted | ProductRequestBinding::Existing => {}
        }
        let existing: Option<(String, String, String, String, Option<String>)> = transaction
            .query_row(
                "SELECT turn_id,request_scope,payload_digest,state,terminal_code
                 FROM agent_turn_admissions
                 WHERE request_id=?1",
                params![request.request_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| "turn_admission_unavailable")?;
        if let Some((stored_turn_id, stored_scope, stored_digest, state, terminal_code)) = existing
        {
            if stored_turn_id != turn_id
                || stored_scope != identity.request_scope
                || stored_digest != identity.payload_digest
            {
                return Err("request_id_conflict".into());
            }
            return match (state.as_str(), terminal_code) {
                ("active", Some(code)) if valid_turn_terminal_code(&code) => {
                    Ok(AgentTurnAdmissionBegin::Failed(code))
                }
                ("active", None) => Ok(AgentTurnAdmissionBegin::Existing),
                ("cancelled", None) => Ok(AgentTurnAdmissionBegin::Cancelled),
                _ => Err("turn_admission_unavailable".into()),
            };
        }
        let count: i64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM agent_turn_admissions
                 WHERE state='active' AND terminal_code IS NULL",
                [],
                |row| row.get(0),
            )
            .map_err(|_| "turn_admission_unavailable")?;
        if count >= MAX_AGENT_TURN_ADMISSIONS {
            return Err("turn_registry_unavailable".into());
        }
        enforce_control_quota(&transaction, logical_bytes)?;
        transaction
            .execute(
                "INSERT INTO agent_turn_admissions(
                   request_id,turn_id,request_scope,payload_digest,state,sealed_request,
                   created_at_ms,logical_bytes
                 ) VALUES(?1,?2,?3,?4,'active',?5,?6,?7)",
                params![
                    request.request_id,
                    turn_id,
                    identity.request_scope,
                    identity.payload_digest,
                    sealed,
                    u64_to_i64(crate::unix_now_ms())?,
                    logical_bytes
                ],
            )
            .map_err(|_| "turn_admission_unavailable")?;
        transaction
            .commit()
            .map_err(|_| "turn_admission_unavailable")?;
        Ok(AgentTurnAdmissionBegin::Inserted)
    }

    #[cfg(test)]
    pub(crate) fn begin_product_request(
        &self,
        request_id: &str,
        route_domain: &'static str,
        payload_digest: &str,
    ) -> Result<ProductRequestBegin, String> {
        self.begin_product_request_identity(&isyncyou_webui::ProductRequestIdentity {
            request_id: request_id.into(),
            route_domain,
            request_scope: "installation".into(),
            payload_digest: payload_digest.into(),
        })
    }

    #[cfg(test)]
    pub(crate) fn complete_product_request(
        &self,
        request_id: &str,
        route_domain: &'static str,
        payload_digest: &str,
        response: &StoredProductResponseV1,
    ) -> Result<(), String> {
        self.complete_product_request_identity(
            &isyncyou_webui::ProductRequestIdentity {
                request_id: request_id.into(),
                route_domain,
                request_scope: "installation".into(),
                payload_digest: payload_digest.into(),
            },
            response,
        )
    }

    #[cfg(test)]
    pub(crate) fn abort_product_request(
        &self,
        request_id: &str,
        route_domain: &'static str,
        payload_digest: &str,
    ) -> Result<(), String> {
        self.abort_product_request_identity(&isyncyou_webui::ProductRequestIdentity {
            request_id: request_id.into(),
            route_domain,
            request_scope: "installation".into(),
            payload_digest: payload_digest.into(),
        })
    }

    #[cfg(test)]
    pub(crate) fn begin_agent_turn_admission(
        &self,
        request: &isyncyou_webui::AgentTurnRequest,
        turn_id: &str,
        payload_digest: &str,
    ) -> Result<AgentTurnAdmissionBegin, String> {
        self.begin_agent_turn_admission_identity(
            request,
            turn_id,
            &isyncyou_webui::ProductRequestIdentity {
                request_id: request.request_id.clone(),
                route_domain: AGENT_TURN_ROUTE_DOMAIN,
                request_scope: format!("session_id:{}", request.session_id),
                payload_digest: payload_digest.into(),
            },
        )
    }

    pub(crate) fn recover_agent_turn_admissions(
        &self,
        limit: usize,
    ) -> Result<Vec<RecoveredAgentTurnAdmission>, String> {
        let limit = i64::try_from(limit.min(MAX_AGENT_TURN_ADMISSIONS as usize))
            .map_err(|_| "turn_admission_unavailable")?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| "turn_admission_unavailable")?;
        let mut statement = connection
            .prepare(
                "SELECT request_id,turn_id,request_scope,payload_digest,sealed_request
                 FROM agent_turn_admissions
                 WHERE state='active' AND terminal_code IS NULL
                 ORDER BY created_at_ms,request_id LIMIT ?1",
            )
            .map_err(|_| "turn_admission_unavailable")?;
        let rows = statement
            .query_map(params![limit], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            })
            .map_err(|_| "turn_admission_unavailable")?;
        let mut recovered = Vec::new();
        for row in rows {
            let (request_id, turn_id, request_scope, payload_digest, sealed) =
                row.map_err(|_| "turn_admission_unavailable")?;
            let plaintext = open_row(
                &self.row_wrap_key,
                "agent-turn-admission",
                &request_id,
                &sealed,
            )?;
            let record: AgentTurnAdmissionV1 =
                serde_json::from_slice(&plaintext).map_err(|_| "turn_admission_unavailable")?;
            if record.version != 2
                || record.request.request_id != request_id
                || record.turn_id != turn_id
                || record.route_domain != AGENT_TURN_ROUTE_DOMAIN
                || record.request_scope != request_scope
                || record.payload_digest != payload_digest
            {
                return Err("turn_admission_unavailable".into());
            }
            recovered.push(RecoveredAgentTurnAdmission {
                request: record.request,
                turn_id: record.turn_id,
                identity: isyncyou_webui::ProductRequestIdentity {
                    request_id,
                    route_domain: AGENT_TURN_ROUTE_DOMAIN,
                    request_scope,
                    payload_digest,
                },
            });
        }
        Ok(recovered)
    }

    pub(crate) fn complete_agent_turn_admission(&self, request_id: &str) -> Result<(), String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "turn_admission_unavailable")?;
        let changed = connection
            .execute(
                "DELETE FROM agent_turn_admissions WHERE request_id=?1",
                params![request_id],
            )
            .map_err(|_| "turn_admission_unavailable")?;
        if changed > 0 {
            checkpoint_secure_erasure(&connection)
                .map_err(|_| "turn_admission_unavailable".to_string())?;
        }
        Ok(())
    }

    pub(crate) fn cancel_agent_turn_admission(&self, turn_id: &str) -> Result<(), String> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "turn_admission_unavailable")?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "turn_admission_unavailable")?;
        let existing: Option<(String, String, Option<String>)> = transaction
            .query_row(
                "SELECT request_id,state,terminal_code FROM agent_turn_admissions WHERE turn_id=?1",
                params![turn_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|_| "turn_admission_unavailable")?;
        let Some((request_id, state, terminal_code)) = existing else {
            return Err("turn_admission_unavailable".into());
        };
        match state.as_str() {
            "cancelled" => {
                transaction
                    .commit()
                    .map_err(|_| "turn_admission_unavailable")?;
                checkpoint_secure_erasure(&connection).map_err(|_| "turn_admission_unavailable")?;
                return Ok(());
            }
            "active" if terminal_code.is_none() => {}
            _ => return Err("turn_admission_unavailable".into()),
        }
        let logical_bytes = i64::try_from(
            request_id
                .len()
                .saturating_add(turn_id.len())
                .saturating_add(192),
        )
        .map_err(|_| "turn_admission_unavailable")?;
        let changed = transaction
            .execute(
                "UPDATE agent_turn_admissions
                 SET state='cancelled',sealed_request=X'',logical_bytes=?1
                 WHERE turn_id=?2 AND request_id=?3 AND state='active'",
                params![logical_bytes, turn_id, request_id],
            )
            .map_err(|_| "turn_admission_unavailable")?;
        if changed != 1 {
            return Err("turn_admission_unavailable".into());
        }
        transaction
            .commit()
            .map_err(|_| "turn_admission_unavailable".to_string())?;
        checkpoint_secure_erasure(&connection).map_err(|_| "turn_admission_unavailable".to_string())
    }

    pub(crate) fn fail_agent_turn_admission(
        &self,
        turn_id: &str,
        terminal_code: &str,
    ) -> Result<AgentTurnAdmissionFailure, String> {
        if !valid_turn_terminal_code(terminal_code) {
            return Err("turn_admission_unavailable".into());
        }
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "turn_admission_unavailable")?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "turn_admission_unavailable")?;
        let existing: Option<(String, Option<String>)> = transaction
            .query_row(
                "SELECT state,terminal_code FROM agent_turn_admissions WHERE turn_id=?1",
                params![turn_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|_| "turn_admission_unavailable")?;
        let Some((state, stored_code)) = existing else {
            return Ok(AgentTurnAdmissionFailure::Completed);
        };
        if state == "cancelled" {
            return Ok(AgentTurnAdmissionFailure::Cancelled);
        }
        if state != "active" {
            return Err("turn_admission_unavailable".into());
        }
        if let Some(stored_code) = stored_code {
            return if stored_code == terminal_code && valid_turn_terminal_code(&stored_code) {
                Ok(AgentTurnAdmissionFailure::Failed)
            } else {
                Err("turn_admission_unavailable".into())
            };
        }
        let logical_bytes = i64::try_from(
            turn_id
                .len()
                .saturating_add(terminal_code.len())
                .saturating_add(192),
        )
        .map_err(|_| "turn_admission_unavailable")?;
        let changed = transaction
            .execute(
                "UPDATE agent_turn_admissions
                 SET terminal_code=?1,sealed_request=X'',logical_bytes=?2
                 WHERE turn_id=?3 AND state='active' AND terminal_code IS NULL",
                params![terminal_code, logical_bytes, turn_id],
            )
            .map_err(|_| "turn_admission_unavailable")?;
        if changed != 1 {
            return Err("turn_admission_unavailable".into());
        }
        transaction
            .commit()
            .map_err(|_| "turn_admission_unavailable".to_string())?;
        checkpoint_secure_erasure(&connection)
            .map_err(|_| "turn_admission_unavailable".to_string())?;
        Ok(AgentTurnAdmissionFailure::Failed)
    }

    pub(crate) fn agent_turn_admission_terminal(
        &self,
        request_id: &str,
        request_scope: &str,
    ) -> Result<Option<(isyncyou_agent::RequestPhase, Option<String>)>, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "turn_admission_unavailable")?;
        let row: Option<(String, Option<String>)> = connection
            .query_row(
                "SELECT state,terminal_code FROM agent_turn_admissions
                 WHERE request_id=?1 AND request_scope=?2",
                params![request_id, request_scope],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|_| "turn_admission_unavailable")?;
        match row {
            Some((state, None)) if state == "cancelled" => {
                Ok(Some((isyncyou_agent::RequestPhase::Cancelled, None)))
            }
            Some((state, Some(code))) if state == "active" && valid_turn_terminal_code(&code) => {
                Ok(Some((isyncyou_agent::RequestPhase::Failed, Some(code))))
            }
            Some((state, None)) if state == "active" => Ok(None),
            None => Ok(None),
            _ => Err("turn_admission_unavailable".into()),
        }
    }
}

fn bind_product_request_transaction(
    transaction: &Transaction<'_>,
    identity: &isyncyou_webui::ProductRequestIdentity,
    expires_at_ms: i64,
    installation_principal: &str,
) -> Result<ProductRequestBinding, String> {
    let request_key = product_request_key(identity, installation_principal)?;
    let binding: Option<(String, String, String, String)> = transaction
        .query_row(
            "SELECT request_key,route_domain,request_scope,payload_digest
             FROM product_request_bindings WHERE request_id=?1",
            params![identity.request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(|_| "request_store_unavailable")?;
    if let Some((stored_key, stored_route, stored_scope, stored_digest)) = binding {
        return if stored_key == request_key
            && stored_route == identity.route_domain
            && stored_scope == identity.request_scope
            && stored_digest == identity.payload_digest
        {
            if expires_at_ms == i64::MAX {
                transaction
                    .execute(
                        "UPDATE product_request_bindings SET expires_at_ms=?1
                         WHERE request_id=?2 AND expires_at_ms<?1",
                        params![expires_at_ms, identity.request_id],
                    )
                    .map_err(|_| "request_store_unavailable")?;
            }
            Ok(ProductRequestBinding::Existing)
        } else {
            Ok(ProductRequestBinding::Conflict)
        };
    }
    let logical_bytes = i64::try_from(
        identity
            .request_id
            .len()
            .saturating_add(request_key.len())
            .saturating_add(identity.route_domain.len())
            .saturating_add(identity.request_scope.len())
            .saturating_add(identity.payload_digest.len())
            .saturating_add(128),
    )
    .map_err(|_| "request_store_unavailable")?;
    enforce_control_quota(transaction, logical_bytes)?;
    transaction
        .execute(
            "INSERT INTO product_request_bindings(
               request_id,request_key,route_domain,request_scope,payload_digest,
               expires_at_ms,logical_bytes
             ) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                identity.request_id,
                request_key,
                identity.route_domain,
                identity.request_scope,
                identity.payload_digest,
                expires_at_ms,
                logical_bytes
            ],
        )
        .map_err(|_| "request_store_unavailable")?;
    Ok(ProductRequestBinding::Inserted)
}

fn product_request_key(
    identity: &isyncyou_webui::ProductRequestIdentity,
    installation_principal: &str,
) -> Result<String, String> {
    product_request_key_components(
        identity.route_domain,
        &identity.request_scope,
        &identity.request_id,
        installation_principal,
    )
}

fn product_request_key_components(
    route_domain: &str,
    request_scope: &str,
    request_id: &str,
    installation_principal: &str,
) -> Result<String, String> {
    if route_domain.is_empty()
        || route_domain.len() > 160
        || request_scope.is_empty()
        || request_scope.len() > 256
        || request_id.is_empty()
        || request_id.len() > 64
        || installation_principal.len() != 22
    {
        return Err("request_store_unavailable".into());
    }
    let effective_scope = if request_scope == "installation" {
        installation_principal
    } else {
        request_scope
    };
    let mut input = b"isyncyou-idempotency-v1".to_vec();
    for component in [
        route_domain.as_bytes(),
        effective_scope.as_bytes(),
        request_id.as_bytes(),
    ] {
        input.extend_from_slice(&(component.len() as u64).to_be_bytes());
        input.extend_from_slice(component);
    }
    Ok(sha256_hex(&input))
}

type ControlSubkeys = (Zeroizing<[u8; 32]>, Zeroizing<[u8; 32]>);

fn derive_control_subkeys(control_root: &[u8; 32]) -> Result<ControlSubkeys, String> {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, CONTROL_SUBKEY_SALT);
    let prk = salt.extract(control_root);
    let mut sqlcipher_key = Zeroizing::new([0u8; 32]);
    prk.expand(&[SQLCIPHER_KEY_INFO], hkdf::HKDF_SHA256)
        .map_err(|_| "control_store_key_unavailable")?
        .fill(&mut sqlcipher_key[..])
        .map_err(|_| "control_store_key_unavailable")?;
    let mut row_wrap_key = Zeroizing::new([0u8; 32]);
    prk.expand(&[ROW_WRAP_KEY_INFO], hkdf::HKDF_SHA256)
        .map_err(|_| "control_store_key_unavailable")?
        .fill(&mut row_wrap_key[..])
        .map_err(|_| "control_store_key_unavailable")?;
    Ok((sqlcipher_key, row_wrap_key))
}

impl PendingPersistence for AgentControlStore {
    fn insert(&self, pending: PersistedPendingAction) -> Result<(), ConfirmError> {
        self.reap_expired(crate::unix_now_ms(), 256)
            .map_err(|_| ConfirmError::Unavailable)?;
        if pending.id.is_empty()
            || pending.action_hash.len() != 64
            || pending.owner.account.is_empty()
            || pending.owner.session_id.is_empty()
            || pending.owner.request_id.is_empty()
            || pending.owner.turn_id.is_empty()
        {
            return Err(ConfirmError::Unavailable);
        }
        let secret = PendingSecretV1 {
            version: 1,
            action: pending.action,
            preview: pending.preview,
            token_hash: URL_SAFE_NO_PAD.encode(pending.token_hash),
            risk: pending.risk,
        };
        let plaintext = serde_json::to_vec(&secret).map_err(|_| ConfirmError::Unavailable)?;
        if plaintext.len() > MAX_PENDING_PLAINTEXT {
            return Err(ConfirmError::Unavailable);
        }
        let sealed = seal_row(&self.row_wrap_key, "agent-tool", &pending.id, &plaintext)
            .map_err(|_| ConfirmError::Unavailable)?;
        let logical_bytes = i64::try_from(sealed.len()).map_err(|_| ConfirmError::Unavailable)?;
        let expires = u64_to_i64(pending.expires_at_ms).map_err(|_| ConfirmError::Unavailable)?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ConfirmError::Unavailable)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| ConfirmError::Unavailable)?;
        let count: i64 = transaction
            .query_row("SELECT COUNT(*) FROM confirmation_intents", [], |row| {
                row.get(0)
            })
            .map_err(|_| ConfirmError::Unavailable)?;
        if count >= MAX_CONFIRMATIONS || enforce_control_quota(&transaction, logical_bytes).is_err()
        {
            return Err(ConfirmError::Unavailable);
        }
        transaction
            .execute(
                "INSERT INTO confirmation_intents(
                   intent_id,account_id,session_id,request_id,turn_id,owner_binding,
                   action_hash,expires_at_ms,state,sealed_payload,logical_bytes
                 ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'pending',?9,?10)",
                params![
                    pending.id,
                    pending.owner.account,
                    pending.owner.session_id,
                    pending.owner.request_id,
                    pending.owner.turn_id,
                    self.installation_binding,
                    pending.action_hash,
                    expires,
                    sealed,
                    logical_bytes,
                ],
            )
            .map_err(|_| ConfirmError::Unavailable)?;
        transaction.commit().map_err(|_| ConfirmError::Unavailable)
    }

    fn confirm(
        &self,
        pending_id: &str,
        token_hash: &[u8; 32],
        action_hash: &str,
        now_ms: u64,
    ) -> Result<ToolAction, ConfirmError> {
        let Some((state, expires, expected_action_hash, sealed)) = self.load_pending(pending_id)?
        else {
            return Err(ConfirmError::NotFound);
        };
        if state != "pending" {
            return Err(ConfirmError::NotFound);
        }
        if now_ms > expires {
            self.erase_with_state(pending_id, "expired", now_ms)?;
            return Err(ConfirmError::Expired);
        }
        if !constant_time_eq(action_hash.as_bytes(), expected_action_hash.as_bytes()) {
            return Err(ConfirmError::ActionMismatch);
        }
        let pending = self.decrypt_pending(
            pending_id,
            sealed.as_deref().ok_or(ConfirmError::Unavailable)?,
        )?;
        let expected_token = URL_SAFE_NO_PAD
            .decode(pending.token_hash)
            .map_err(|_| ConfirmError::Unavailable)?;
        if !constant_time_eq(token_hash, &expected_token) {
            return Err(ConfirmError::BadToken);
        }
        self.erase_with_state(pending_id, "consumed", now_ms)?;
        Ok(pending.action)
    }

    fn binding(
        &self,
        pending_id: &str,
        action_hash: &str,
        now_ms: u64,
    ) -> Result<PendingActionBinding, ConfirmError> {
        let Some((state, expires, expected_action_hash, sealed)) = self.load_pending(pending_id)?
        else {
            return Err(ConfirmError::NotFound);
        };
        if state != "pending" {
            return Err(ConfirmError::NotFound);
        }
        if now_ms > expires {
            self.erase_with_state(pending_id, "expired", now_ms)?;
            return Err(ConfirmError::Expired);
        }
        if !constant_time_eq(action_hash.as_bytes(), expected_action_hash.as_bytes()) {
            return Err(ConfirmError::ActionMismatch);
        }
        let pending = self.decrypt_pending(
            pending_id,
            sealed.as_deref().ok_or(ConfirmError::Unavailable)?,
        )?;
        Ok(PendingActionBinding {
            op: pending.action.op().to_owned(),
            account: pending.action.account().to_owned(),
            service: pending.action.service().unwrap_or("agent").to_owned(),
            item: format!(
                "pending:{}:{}:action_hash:{}:{}",
                pending_id.len(),
                pending_id,
                action_hash.len(),
                action_hash
            ),
            expires_at_ms: expires,
        })
    }

    fn cancel(
        &self,
        pending_id: &str,
        action_hash: &str,
        now_ms: u64,
    ) -> Result<PendingOwnerBinding, ConfirmError> {
        let now = u64_to_i64(now_ms).map_err(|_| ConfirmError::Unavailable)?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| ConfirmError::Unavailable)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| ConfirmError::Unavailable)?;
        let row: Option<(String, i64, String, String, String, String, String)> = transaction
            .query_row(
                "SELECT state,expires_at_ms,action_hash,account_id,session_id,request_id,turn_id
                 FROM confirmation_intents
                 WHERE intent_id=?1 AND owner_binding=?2",
                params![pending_id, self.installation_binding],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| ConfirmError::Unavailable)?;
        let Some((state, expires, expected_action_hash, account, session_id, request_id, turn_id)) =
            row
        else {
            return Err(ConfirmError::NotFound);
        };
        if state != "pending" && state != "cancelled" {
            return Err(ConfirmError::NotFound);
        }
        if state == "pending" && now > expires {
            let terminal_expires_at_ms =
                control_tombstone_expiry(now_ms).map_err(|_| ConfirmError::Unavailable)?;
            transaction
                .execute(
                    "UPDATE confirmation_intents
                     SET state='expired',sealed_payload=NULL,terminal_expires_at_ms=?3,
                         logical_bytes=0
                     WHERE intent_id=?1 AND owner_binding=?2 AND state='pending'",
                    params![
                        pending_id,
                        self.installation_binding,
                        terminal_expires_at_ms
                    ],
                )
                .map_err(|_| ConfirmError::Unavailable)?;
            transaction
                .commit()
                .map_err(|_| ConfirmError::Unavailable)?;
            checkpoint_secure_erasure(&connection).map_err(|_| ConfirmError::Unavailable)?;
            return Err(ConfirmError::Expired);
        }
        if !constant_time_eq(action_hash.as_bytes(), expected_action_hash.as_bytes()) {
            return Err(ConfirmError::ActionMismatch);
        }
        if state == "pending" {
            let terminal_expires_at_ms =
                control_tombstone_expiry(now_ms).map_err(|_| ConfirmError::Unavailable)?;
            let changed = transaction
                .execute(
                    "UPDATE confirmation_intents
                     SET state='cancelled',sealed_payload=NULL,terminal_expires_at_ms=?3,
                         logical_bytes=0
                     WHERE intent_id=?1 AND owner_binding=?2 AND state='pending'",
                    params![
                        pending_id,
                        self.installation_binding,
                        terminal_expires_at_ms
                    ],
                )
                .map_err(|_| ConfirmError::Unavailable)?;
            if changed != 1 {
                return Err(ConfirmError::Unavailable);
            }
        }
        let logical_bytes = i64::try_from(
            pending_id
                .len()
                .saturating_add(account.len())
                .saturating_add(session_id.len())
                .saturating_add(request_id.len())
                .saturating_add(turn_id.len())
                .saturating_add(self.installation_binding.len())
                .saturating_add(128),
        )
        .map_err(|_| ConfirmError::Unavailable)?;
        transaction
            .execute(
                "INSERT INTO pending_cancel_projections(
                   pending_id,account_id,session_id,request_id,turn_id,owner_binding,
                   created_at_ms,logical_bytes
                 ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(pending_id) DO NOTHING",
                params![
                    pending_id,
                    account,
                    session_id,
                    request_id,
                    turn_id,
                    self.installation_binding,
                    now,
                    logical_bytes,
                ],
            )
            .map_err(|_| ConfirmError::Unavailable)?;
        transaction
            .commit()
            .map_err(|_| ConfirmError::Unavailable)?;
        checkpoint_secure_erasure(&connection).map_err(|_| ConfirmError::Unavailable)?;
        let owner = PendingOwnerBinding {
            account,
            session_id,
            request_id,
            turn_id,
        };
        Ok(owner)
    }

    fn has_pending_for_turn(&self, turn_id: &str, now_ms: u64) -> Result<bool, ConfirmError> {
        let now = u64_to_i64(now_ms).map_err(|_| ConfirmError::Unavailable)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| ConfirmError::Unavailable)?;
        let count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM confirmation_intents
                 WHERE turn_id=?1 AND owner_binding=?2 AND state='pending'
                   AND expires_at_ms>=?3",
                params![turn_id, self.installation_binding, now],
                |row| row.get(0),
            )
            .map_err(|_| ConfirmError::Unavailable)?;
        Ok(count > 0)
    }
}

fn ensure_text_column(
    connection: &Connection,
    table: &str,
    column: &str,
    default_value: &str,
) -> Result<(), String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|_| "control_store_migration_failed")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|_| "control_store_migration_failed")?;
    for existing in columns {
        if existing.map_err(|_| "control_store_migration_failed")? == column {
            return Ok(());
        }
    }
    drop(statement);
    let sql = match (table, column, default_value) {
        ("product_request_receipts", "request_scope", "installation")
        | ("product_request_bindings", "request_scope", "installation")
        | ("agent_turn_admissions", "request_scope", "installation") => {
            format!("ALTER TABLE {table} ADD COLUMN {column} TEXT NOT NULL DEFAULT 'installation'")
        }
        ("agent_turn_admissions", "state", "active") => {
            "ALTER TABLE agent_turn_admissions ADD COLUMN state TEXT NOT NULL DEFAULT 'active'"
                .into()
        }
        _ => return Err("control_store_migration_failed".into()),
    };
    connection
        .execute(&sql, [])
        .map(|_| ())
        .map_err(|_| "control_store_migration_failed".into())
}

fn ensure_nullable_text_column(
    connection: &Connection,
    table: &str,
    column: &str,
) -> Result<(), String> {
    if table != "agent_turn_admissions" || column != "terminal_code" {
        return Err("control_store_migration_failed".into());
    }
    let mut statement = connection
        .prepare("PRAGMA table_info(agent_turn_admissions)")
        .map_err(|_| "control_store_migration_failed")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|_| "control_store_migration_failed")?;
    for existing in columns {
        if existing.map_err(|_| "control_store_migration_failed")? == column {
            return Ok(());
        }
    }
    drop(statement);
    connection
        .execute(
            "ALTER TABLE agent_turn_admissions ADD COLUMN terminal_code TEXT",
            [],
        )
        .map(|_| ())
        .map_err(|_| "control_store_migration_failed".into())
}

fn ensure_request_key_column(connection: &Connection, table: &str) -> Result<(), String> {
    if !matches!(
        table,
        "product_request_receipts" | "product_request_bindings"
    ) {
        return Err("control_store_migration_failed".into());
    }
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|_| "control_store_migration_failed")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|_| "control_store_migration_failed")?;
    for existing in columns {
        if existing.map_err(|_| "control_store_migration_failed")? == "request_key" {
            return Ok(());
        }
    }
    drop(statement);
    connection
        .execute(
            &format!("ALTER TABLE {table} ADD COLUMN request_key TEXT NOT NULL DEFAULT ''"),
            [],
        )
        .map(|_| ())
        .map_err(|_| "control_store_migration_failed".into())
}

fn ensure_integer_column(connection: &Connection, table: &str, column: &str) -> Result<(), String> {
    if !matches!(
        (table, column),
        ("mutation_intents", "terminal_expires_at_ms")
            | ("mutation_request_bindings", "expires_at_ms")
            | ("confirmation_intents", "terminal_expires_at_ms")
            | ("user_presence_intents", "terminal_expires_at_ms")
            | ("pairing_sources", "terminal_expires_at_ms")
            | ("pairing_claims", "terminal_expires_at_ms")
            | ("pairing_revocations", "terminal_expires_at_ms")
    ) {
        return Err("control_store_migration_failed".into());
    }
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|_| "control_store_migration_failed")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|_| "control_store_migration_failed")?;
    for existing in columns {
        if existing.map_err(|_| "control_store_migration_failed")? == column {
            return Ok(());
        }
    }
    drop(statement);
    connection
        .execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0"),
            [],
        )
        .map(|_| ())
        .map_err(|_| "control_store_migration_failed".into())
}

fn delete_expired_control_tombstones(
    connection: &Connection,
    table: &str,
    id_column: &str,
    terminal_states: &str,
    now_ms: i64,
    limit: i64,
) -> Result<usize, String> {
    if !matches!(
        (table, id_column, terminal_states),
        (
            "confirmation_intents",
            "intent_id",
            "'consumed','cancelled','expired'"
        ) | (
            "user_presence_intents",
            "operation_id",
            "'consumed','cancelled','expired'"
        ) | ("pairing_sources", "pair_id", "'revoked','expired'")
            | (
                "pairing_claims",
                "operation_id",
                "'consumed','aborted','claimed_expired'"
            )
            | ("pairing_revocations", "operation_id", "'completed'")
    ) {
        return Err("control_store_unavailable".into());
    }
    connection
        .execute(
            &format!(
                "DELETE FROM {table} WHERE {id_column} IN (
                   SELECT {id_column} FROM {table}
                   WHERE state IN ({terminal_states})
                     AND terminal_expires_at_ms > 0 AND terminal_expires_at_ms < ?1
                   LIMIT ?2
                 )"
            ),
            params![now_ms, limit],
        )
        .map_err(|_| "control_store_unavailable".into())
}

fn consume_reaper_budget(remaining: &mut i64, changed: usize) -> Result<(), String> {
    let changed = i64::try_from(changed).map_err(|_| "control_store_unavailable")?;
    if changed > *remaining {
        return Err("control_store_unavailable".into());
    }
    *remaining -= changed;
    Ok(())
}

fn stored_control_schema_version(connection: &Connection) -> Result<Option<i64>, String> {
    connection
        .query_row(
            "SELECT value FROM control_metadata WHERE key='schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|_| "control_store_migration_failed")?
        .map(|value| {
            value
                .parse::<i64>()
                .map_err(|_| "control_store_migration_failed".to_string())
        })
        .transpose()
}

fn migrate_mutation_retention_v9(
    transaction: &Transaction<'_>,
    row_wrap_key: &[u8; 32],
    now_ms: u64,
) -> Result<(), String> {
    let binding_expiry = now_ms
        .checked_add(MUTATION_REQUEST_BINDING_TTL_MS)
        .ok_or("control_store_migration_failed")?;
    transaction
        .execute(
            "UPDATE mutation_request_bindings SET expires_at_ms=?1 WHERE expires_at_ms=0",
            params![u64_to_i64(binding_expiry)?],
        )
        .map_err(|_| "control_store_migration_failed")?;

    let mut statement = transaction
        .prepare(
            "SELECT intent_id,state,purpose_json,result_json,expires_at_ms,terminal_expires_at_ms
             FROM mutation_intents",
        )
        .map_err(|_| "control_store_migration_failed")?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Option<Vec<u8>>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .map_err(|_| "control_store_migration_failed")?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "control_store_migration_failed")?;
    drop(statement);

    for (intent_id, state, purpose, result, expires, terminal_expires) in rows {
        let (purpose, result, terminal_expires, logical_bytes) = match state.as_str() {
            "active" | "committing" => {
                let purpose = ensure_mutation_json_sealed(
                    row_wrap_key,
                    "mutation-purpose",
                    &intent_id,
                    &purpose,
                )?;
                let logical_bytes = i64::try_from(purpose.len().saturating_add(512))
                    .map_err(|_| "control_store_migration_failed")?;
                (purpose, None, 0, logical_bytes)
            }
            "committed" => {
                let result = result.ok_or("control_store_migration_failed")?;
                let result = ensure_mutation_json_sealed(
                    row_wrap_key,
                    "mutation-result",
                    &intent_id,
                    &result,
                )?;
                let terminal_expires = if terminal_expires > 0 {
                    terminal_expires
                } else {
                    expires
                        .checked_add(
                            i64::try_from(MUTATION_TERMINAL_TTL_MS)
                                .map_err(|_| "control_store_migration_failed")?,
                        )
                        .ok_or("control_store_migration_failed")?
                };
                let logical_bytes = i64::try_from(result.len().saturating_add(512))
                    .map_err(|_| "control_store_migration_failed")?;
                (Vec::new(), Some(result), terminal_expires, logical_bytes)
            }
            "cancelled" | "expired" => {
                let terminal_expires = if terminal_expires > 0 {
                    terminal_expires
                } else {
                    expires
                        .checked_add(
                            i64::try_from(MUTATION_TERMINAL_TTL_MS)
                                .map_err(|_| "control_store_migration_failed")?,
                        )
                        .ok_or("control_store_migration_failed")?
                };
                (Vec::new(), None, terminal_expires, 512)
            }
            _ => return Err("control_store_migration_failed".into()),
        };
        transaction
            .execute(
                "UPDATE mutation_intents
                 SET purpose_json=?1,result_json=?2,terminal_expires_at_ms=?3,logical_bytes=?4
                 WHERE intent_id=?5",
                params![purpose, result, terminal_expires, logical_bytes, intent_id],
            )
            .map_err(|_| "control_store_migration_failed")?;
    }
    Ok(())
}

fn migrate_control_tombstone_retention_v10(
    transaction: &Transaction<'_>,
    now_ms: u64,
) -> Result<(), String> {
    let terminal_expires_at_ms = now_ms
        .checked_add(CONTROL_TOMBSTONE_TTL_MS)
        .ok_or("control_store_migration_failed")?;
    let terminal_expires_at_ms = u64_to_i64(terminal_expires_at_ms)?;
    for (table, terminal_states) in [
        ("confirmation_intents", "'consumed','cancelled','expired'"),
        ("user_presence_intents", "'consumed','cancelled','expired'"),
        ("pairing_sources", "'revoked','expired'"),
        ("pairing_claims", "'consumed','aborted','claimed_expired'"),
        ("pairing_revocations", "'completed'"),
    ] {
        transaction
            .execute(
                &format!(
                    "UPDATE {table} SET terminal_expires_at_ms=?1
                     WHERE terminal_expires_at_ms=0 AND state IN ({terminal_states})"
                ),
                params![terminal_expires_at_ms],
            )
            .map_err(|_| "control_store_migration_failed")?;
    }
    Ok(())
}

fn ensure_mutation_json_sealed(
    row_wrap_key: &[u8; 32],
    class: &str,
    intent_id: &str,
    value: &[u8],
) -> Result<Vec<u8>, String> {
    if let Ok(plaintext) = open_row(row_wrap_key, class, intent_id, value) {
        serde_json::from_slice::<serde_json::Value>(&plaintext)
            .map_err(|_| "control_store_migration_failed")?;
        return Ok(value.to_vec());
    }
    serde_json::from_slice::<serde_json::Value>(value)
        .map_err(|_| "control_store_migration_failed")?;
    seal_row(row_wrap_key, class, intent_id, value)
        .map_err(|_| "control_store_migration_failed".into())
}

fn migrate_product_request_keys(
    transaction: &Transaction<'_>,
    installation_principal: &str,
) -> Result<(), String> {
    let receipts = {
        let mut statement = transaction
            .prepare(
                "SELECT request_id,route_domain,request_scope,payload_digest,expires_at_ms
                 FROM product_request_receipts ORDER BY request_id",
            )
            .map_err(|_| "control_store_migration_failed")?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .map_err(|_| "control_store_migration_failed")?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|_| "control_store_migration_failed")?
    };
    for (request_id, route, scope, digest, expires_at_ms) in receipts {
        let binding: Option<(String, String, String)> = transaction
            .query_row(
                "SELECT route_domain,request_scope,payload_digest
                 FROM product_request_bindings WHERE request_id=?1",
                params![request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|_| "control_store_migration_failed")?;
        match binding {
            Some((stored_route, stored_scope, stored_digest)) => {
                if stored_route != route
                    || stored_digest != digest
                    || (stored_scope != scope && stored_scope != "installation")
                {
                    return Err("control_store_migration_failed".into());
                }
                if stored_scope != scope {
                    transaction
                        .execute(
                            "UPDATE product_request_bindings SET request_scope=?1
                             WHERE request_id=?2",
                            params![scope, request_id],
                        )
                        .map_err(|_| "control_store_migration_failed")?;
                }
            }
            None => {
                let logical_bytes = i64::try_from(
                    request_id
                        .len()
                        .saturating_add(route.len())
                        .saturating_add(scope.len())
                        .saturating_add(digest.len())
                        .saturating_add(128),
                )
                .map_err(|_| "control_store_migration_failed")?;
                enforce_control_quota(transaction, logical_bytes)
                    .map_err(|_| "control_store_migration_failed")?;
                transaction
                    .execute(
                        "INSERT INTO product_request_bindings(
                           request_id,request_key,route_domain,request_scope,payload_digest,
                           expires_at_ms,logical_bytes
                         ) VALUES(?1,'',?2,?3,?4,?5,?6)",
                        params![
                            request_id,
                            route,
                            scope,
                            digest,
                            expires_at_ms,
                            logical_bytes
                        ],
                    )
                    .map_err(|_| "control_store_migration_failed")?;
            }
        }
    }

    for table in ["product_request_bindings", "product_request_receipts"] {
        let rows = {
            let mut statement = transaction
                .prepare(&format!(
                    "SELECT request_id,request_key,route_domain,request_scope FROM {table}
                     ORDER BY request_id"
                ))
                .map_err(|_| "control_store_migration_failed")?;
            let mapped = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .map_err(|_| "control_store_migration_failed")?;
            mapped
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| "control_store_migration_failed")?
        };
        for (request_id, stored_key, route, scope) in rows {
            let expected =
                product_request_key_components(&route, &scope, &request_id, installation_principal)
                    .map_err(|_| "control_store_migration_failed")?;
            if stored_key == expected {
                continue;
            }
            if !stored_key.is_empty() {
                return Err("control_store_migration_failed".into());
            }
            let added_bytes =
                i64::try_from(expected.len()).map_err(|_| "control_store_migration_failed")?;
            enforce_control_quota(transaction, added_bytes)
                .map_err(|_| "control_store_migration_failed")?;
            let changed = transaction
                .execute(
                    &format!(
                        "UPDATE {table} SET request_key=?1,logical_bytes=logical_bytes+?2
                         WHERE request_id=?3 AND request_key=''"
                    ),
                    params![expected, added_bytes, request_id],
                )
                .map_err(|_| "control_store_migration_failed")?;
            if changed != 1 {
                return Err("control_store_migration_failed".into());
            }
        }
    }
    transaction
        .execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS product_request_binding_key
               ON product_request_bindings(request_key);
             CREATE UNIQUE INDEX IF NOT EXISTS product_request_receipt_key
               ON product_request_receipts(request_key);",
        )
        .map_err(|_| "control_store_migration_failed")?;
    Ok(())
}

fn migrate_product_request_scopes_v7(
    transaction: &Transaction<'_>,
    installation_principal: &str,
) -> Result<(), String> {
    for table in ["product_request_bindings", "product_request_receipts"] {
        let rows = {
            let mut statement = transaction
                .prepare(&format!(
                    "SELECT request_id,request_key,route_domain,request_scope,logical_bytes
                     FROM {table}
                     WHERE request_scope!='installation'
                       AND request_scope NOT LIKE 'session_id:%'
                       AND request_scope NOT LIKE 'operation_id:%'
                     ORDER BY request_id"
                ))
                .map_err(|_| "control_store_migration_failed")?;
            let mapped = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                })
                .map_err(|_| "control_store_migration_failed")?;
            mapped
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| "control_store_migration_failed")?
        };
        for (request_id, stored_key, route, old_scope, logical_bytes) in rows {
            let old_key = product_request_key_components(
                &route,
                &old_scope,
                &request_id,
                installation_principal,
            )
            .map_err(|_| "control_store_migration_failed")?;
            if !stored_key.is_empty() && stored_key != old_key {
                return Err("control_store_migration_failed".into());
            }
            let new_scope = "installation";
            let new_key = product_request_key_components(
                &route,
                new_scope,
                &request_id,
                installation_principal,
            )
            .map_err(|_| "control_store_migration_failed")?;
            let next_bytes = logical_bytes
                .checked_sub(
                    i64::try_from(old_scope.len()).map_err(|_| "control_store_migration_failed")?,
                )
                .and_then(|bytes| bytes.checked_add(i64::try_from(new_scope.len()).ok()?))
                .and_then(|bytes| {
                    bytes.checked_add(
                        i64::try_from(new_key.len().saturating_sub(stored_key.len())).ok()?,
                    )
                })
                .ok_or_else(|| "control_store_migration_failed".to_string())?;
            if next_bytes < 0 {
                return Err("control_store_migration_failed".into());
            }
            let changed = transaction
                .execute(
                    &format!(
                        "UPDATE {table}
                         SET request_scope=?1,request_key=?2,logical_bytes=?3
                         WHERE request_id=?4 AND request_scope=?5 AND request_key=?6"
                    ),
                    params![new_scope, new_key, next_bytes, request_id, old_scope, stored_key],
                )
                .map_err(|_| "control_store_migration_failed")?;
            if changed != 1 {
                return Err("control_store_migration_failed".into());
            }
        }
    }
    Ok(())
}

fn migrate_agent_turn_admissions_v2(
    transaction: &Transaction<'_>,
    row_wrap_key: &[u8; 32],
    installation_principal: &str,
) -> Result<(), String> {
    let rows = {
        let mut statement = transaction
            .prepare(
                "SELECT request_id,turn_id,payload_digest,sealed_request,logical_bytes
                 FROM agent_turn_admissions
                 WHERE state='active' AND terminal_code IS NULL
                 ORDER BY created_at_ms,request_id",
            )
            .map_err(|_| "control_store_migration_failed")?;
        let mapped = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .map_err(|_| "control_store_migration_failed")?;
        mapped
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "control_store_migration_failed")?
    };
    for (request_id, turn_id, payload_digest, sealed, previous_logical_bytes) in rows {
        let plaintext = open_row(row_wrap_key, "agent-turn-admission", &request_id, &sealed)
            .map_err(|_| "control_store_migration_failed")?;
        if let Ok(current) = serde_json::from_slice::<AgentTurnAdmissionV1>(&plaintext) {
            if current.version != 2
                || current.request.request_id != request_id
                || current.turn_id != turn_id
                || current.route_domain != AGENT_TURN_ROUTE_DOMAIN
                || current.request_scope != format!("session_id:{}", current.request.session_id)
                || current.payload_digest != payload_digest
            {
                return Err("control_store_migration_failed".into());
            }
            continue;
        }
        let legacy: LegacyAgentTurnAdmissionV1 =
            serde_json::from_slice(&plaintext).map_err(|_| "control_store_migration_failed")?;
        if legacy.version != 1
            || legacy.request.request_id != request_id
            || legacy.turn_id != turn_id
            || legacy.payload_digest != payload_digest
        {
            return Err("control_store_migration_failed".into());
        }
        let request_scope = format!("session_id:{}", legacy.request.session_id);
        let identity = isyncyou_webui::ProductRequestIdentity {
            request_id: request_id.clone(),
            route_domain: AGENT_TURN_ROUTE_DOMAIN,
            request_scope: request_scope.clone(),
            payload_digest: payload_digest.clone(),
        };
        transaction
            .execute(
                "DELETE FROM product_request_bindings
                 WHERE request_id=?1 AND route_domain=?2 AND request_scope='installation'
                   AND payload_digest=?3",
                params![request_id, AGENT_TURN_ROUTE_DOMAIN, payload_digest],
            )
            .map_err(|_| "control_store_migration_failed")?;
        match bind_product_request_transaction(
            transaction,
            &identity,
            i64::MAX,
            installation_principal,
        )
        .map_err(|_| "control_store_migration_failed")?
        {
            ProductRequestBinding::Inserted | ProductRequestBinding::Existing => {}
            ProductRequestBinding::Conflict => return Err("control_store_migration_failed".into()),
        }
        let current = AgentTurnAdmissionV1 {
            version: 2,
            turn_id: legacy.turn_id,
            route_domain: AGENT_TURN_ROUTE_DOMAIN.into(),
            request_scope: request_scope.clone(),
            payload_digest: legacy.payload_digest,
            request: legacy.request,
        };
        let plaintext =
            serde_json::to_vec(&current).map_err(|_| "control_store_migration_failed")?;
        let sealed = seal_row(
            row_wrap_key,
            "agent-turn-admission",
            &request_id,
            &plaintext,
        )
        .map_err(|_| "control_store_migration_failed")?;
        let logical_bytes = i64::try_from(sealed.len().saturating_add(256))
            .map_err(|_| "control_store_migration_failed")?;
        enforce_control_quota(
            transaction,
            logical_bytes.saturating_sub(previous_logical_bytes).max(0),
        )
        .map_err(|_| "control_store_migration_failed")?;
        let changed = transaction
            .execute(
                "UPDATE agent_turn_admissions
                 SET request_scope=?1,sealed_request=?2,logical_bytes=?3
                 WHERE request_id=?4 AND turn_id=?5 AND payload_digest=?6",
                params![
                    request_scope,
                    sealed,
                    logical_bytes,
                    request_id,
                    turn_id,
                    payload_digest
                ],
            )
            .map_err(|_| "control_store_migration_failed")?;
        if changed != 1 {
            return Err("control_store_migration_failed".into());
        }
    }
    Ok(())
}

fn initialize_metadata(
    connection: &Connection,
    installation_binding: &str,
    key_version: u32,
) -> Result<(), String> {
    let existing_binding: Option<String> = connection
        .query_row(
            "SELECT value FROM control_metadata WHERE key='installation_binding'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| "control_store_unavailable")?;
    if let Some(binding) = existing_binding {
        let version: String = connection
            .query_row(
                "SELECT value FROM control_metadata WHERE key='key_version'",
                [],
                |row| row.get(0),
            )
            .map_err(|_| "control_store_unavailable")?;
        let schema: String = connection
            .query_row(
                "SELECT value FROM control_metadata WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .map_err(|_| "control_store_unavailable")?;
        if !constant_time_eq(binding.as_bytes(), installation_binding.as_bytes())
            || version != key_version.to_string()
        {
            return Err("control_store_identity_mismatch".into());
        }
        match schema.parse::<i64>() {
            Ok(SCHEMA_VERSION) => {}
            Ok(1..=10) if SCHEMA_VERSION == 11 => {
                connection
                    .execute(
                        "UPDATE control_metadata SET value=?1 WHERE key='schema_version'",
                        params![SCHEMA_VERSION.to_string()],
                    )
                    .map_err(|_| "control_store_unavailable")?;
            }
            _ => return Err("control_store_identity_mismatch".into()),
        }
        return Ok(());
    }
    for (key, value) in [
        ("schema_version", SCHEMA_VERSION.to_string()),
        ("installation_binding", installation_binding.to_owned()),
        ("key_version", key_version.to_string()),
    ] {
        connection
            .execute(
                "INSERT INTO control_metadata(key,value) VALUES(?1,?2)",
                params![key, value],
            )
            .map_err(|_| "control_store_unavailable")?;
    }
    Ok(())
}

fn bind_mutation_request(
    transaction: &Transaction<'_>,
    request_id: &str,
    route_domain: &str,
    components: &[&[u8]],
    now_ms: u64,
) -> Result<ProductRequestBinding, String> {
    if request_id.is_empty() || request_id.len() > 64 {
        return Err("mutation_intent_invalid".into());
    }
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    digest.update(b"isyncyou-mutation-request-v1\0");
    for component in components {
        digest.update(
            &u64::try_from(component.len())
                .map_err(|_| "mutation_intent_invalid")?
                .to_be_bytes(),
        );
        digest.update(component);
    }
    let payload_digest = digest_hex(digest.finish().as_ref());
    let existing: Option<(String, String)> = transaction
        .query_row(
            "SELECT route_domain,payload_digest FROM mutation_request_bindings
             WHERE request_id=?1",
            params![request_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|_| "mutation_intent_failed")?;
    if let Some((stored_route, stored_digest)) = existing {
        return if stored_route == route_domain && stored_digest == payload_digest {
            Ok(ProductRequestBinding::Existing)
        } else {
            Err("request_id_conflict".into())
        };
    }
    let expires_at_ms = now_ms
        .checked_add(MUTATION_REQUEST_BINDING_TTL_MS)
        .ok_or("mutation_intent_invalid")?;
    let logical_bytes = i64::try_from(
        request_id
            .len()
            .saturating_add(route_domain.len())
            .saturating_add(payload_digest.len())
            .saturating_add(64),
    )
    .map_err(|_| "mutation_intent_failed")?;
    enforce_control_quota(transaction, logical_bytes)?;
    transaction
        .execute(
            "INSERT INTO mutation_request_bindings(
               request_id,route_domain,payload_digest,expires_at_ms,logical_bytes
             ) VALUES(?1,?2,?3,?4,?5)",
            params![
                request_id,
                route_domain,
                payload_digest,
                u64_to_i64(expires_at_ms)?,
                logical_bytes
            ],
        )
        .map_err(|_| "mutation_intent_failed")?;
    Ok(ProductRequestBinding::Inserted)
}

fn enforce_control_quota(connection: &Connection, additional_bytes: i64) -> Result<(), String> {
    if additional_bytes < 0 {
        return Err("control_store_quota_exceeded".into());
    }
    let bytes: i64 = connection
        .query_row(
            "SELECT
               COALESCE((SELECT SUM(logical_bytes) FROM confirmation_intents),0) +
               COALESCE((SELECT SUM(logical_bytes) FROM pending_cancel_projections),0) +
               COALESCE((SELECT SUM(logical_bytes) FROM user_presence_intents),0) +
               COALESCE((SELECT SUM(logical_bytes) FROM pairing_sources),0) +
               COALESCE((SELECT SUM(logical_bytes) FROM pairing_claims),0) +
               COALESCE((SELECT SUM(logical_bytes) FROM pairing_revocations),0) +
               COALESCE((SELECT SUM(logical_bytes) FROM mutation_request_bindings),0) +
               COALESCE((SELECT SUM(logical_bytes) FROM product_request_bindings),0) +
               COALESCE((SELECT SUM(logical_bytes) FROM product_request_receipts),0) +
               COALESCE((SELECT SUM(logical_bytes) FROM agent_turn_admissions),0) +
               COALESCE((SELECT SUM(logical_bytes) FROM mutation_intents),0)",
            [],
            |row| row.get(0),
        )
        .map_err(|_| "control_store_unavailable")?;
    if bytes.saturating_add(additional_bytes) > MAX_CONTROL_BYTES {
        Err("control_store_quota_exceeded".into())
    } else {
        Ok(())
    }
}

fn enforce_mutation_reservation_quota(
    connection: &Connection,
    owner_binding: &str,
) -> Result<(), String> {
    let owner_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM mutation_intents
             WHERE owner_binding=?1 AND state IN ('active','committing')",
            params![owner_binding],
            |row| row.get(0),
        )
        .map_err(|_| "mutation_intent_failed")?;
    let process_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM mutation_intents WHERE state IN ('active','committing')",
            [],
            |row| row.get(0),
        )
        .map_err(|_| "mutation_intent_failed")?;
    let reserved: i64 = connection
        .query_row(
            "SELECT COALESCE(SUM(total_bytes),0) FROM mutation_intents
             WHERE state IN ('active','committing')",
            [],
            |row| row.get(0),
        )
        .map_err(|_| "mutation_intent_failed")?;
    if owner_count > MAX_MUTATION_INTENTS_PER_OWNER
        || process_count > MAX_MUTATION_INTENTS_PROCESS
        || !(0..=MAX_MUTATION_STAGED_BYTES).contains(&reserved)
    {
        Err("mutation_intent_quota_exceeded".into())
    } else {
        Ok(())
    }
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_turn_terminal_code(value: &str) -> bool {
    matches!(
        value,
        "product_not_ready"
            | "provider_busy"
            | "provider_generation_changed"
            | "request_id_conflict"
            | "session_account_mismatch"
            | "session_busy"
            | "session_not_found"
            | "session_store_unavailable"
            | "session_transport_unavailable"
            | "session_transport_timed_out"
            | "session_name_resolution_failed"
            | "session_tls_failed"
            | "session_connect_failed"
            | "session_storage_response_invalid"
            | "session_writer_reconnect_required"
            | "session_storage_permission_denied"
            | "session_storage_request_rejected"
            | "turn_start_failed"
    )
}

fn require_mutation_free_space(required: u64, available: u64) -> Result<(), String> {
    (available >= required)
        .then_some(())
        .ok_or_else(|| "mutation_intent_insufficient_storage".into())
}

fn sha256_hex(value: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, value);
    digest_hex(digest.as_ref())
}

fn digest_hex(digest: &[u8]) -> String {
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn random_id(bytes: usize) -> Result<String, String> {
    let mut value = vec![0u8; bytes];
    ring::rand::SystemRandom::new()
        .fill(&mut value)
        .map_err(|_| "control_store_unavailable")?;
    Ok(URL_SAFE_NO_PAD.encode(value))
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;

    let parent = path.parent().ok_or("mutation_intent_failed")?;
    let temporary = parent.join(format!(".{}.tmp", random_id(12)?));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|_| "mutation_intent_storage_unavailable")?;
    let write_result = (|| {
        file.write_all(bytes)
            .map_err(|_| "mutation_intent_storage_unavailable".to_string())?;
        secure_file_mode(&temporary)?;
        file.sync_all()
            .map_err(|_| "mutation_intent_storage_unavailable".to_string())
    })();
    drop(file);
    if let Err(error) = write_result {
        remove_private_regular_file_and_sync(&temporary)
            .map_err(|_| "mutation_intent_storage_unavailable".to_string())?;
        return Err(error);
    }
    match std::fs::rename(&temporary, path) {
        Ok(()) => {
            sync_directory(parent).map_err(|_| "mutation_intent_storage_unavailable".to_string())
        }
        Err(_) => {
            remove_private_regular_file_and_sync(&temporary)
                .map_err(|_| "mutation_intent_storage_unavailable".to_string())?;
            Err("mutation_intent_storage_unavailable".into())
        }
    }
}

fn remove_private_regular_file_and_sync(path: &Path) -> Result<(), ()> {
    let parent = path.parent().ok_or(())?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => return Err(()),
        Ok(_) => std::fs::remove_file(path).map_err(|_| ())?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(()),
    }
    sync_directory(parent)
}

fn sync_directory(path: &Path) -> Result<(), ()> {
    #[cfg(unix)]
    {
        std::fs::File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| ())?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn remove_private_tree_no_follow(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| "mutation_intent_failed")?;
    if metadata.file_type().is_symlink() || metadata.is_file() {
        return std::fs::remove_file(path).map_err(|_| "mutation_intent_failed".into());
    }
    if !metadata.is_dir() {
        return Err("mutation_intent_failed".into());
    }
    for entry in std::fs::read_dir(path).map_err(|_| "mutation_intent_failed")? {
        remove_private_tree_no_follow(&entry.map_err(|_| "mutation_intent_failed")?.path())?;
    }
    std::fs::remove_dir(path).map_err(|_| "mutation_intent_failed".into())
}

fn checkpoint_secure_erasure(connection: &Connection) -> Result<(), ()> {
    // `secure_delete=ON` overwrites removed cell content in database pages. A
    // truncate checkpoint is still required before returning terminal success,
    // otherwise an older WAL frame can retain the removed wrapped row key.
    let (busy, _, _): (i64, i64, i64) = connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .map_err(|_| ())?;
    (busy == 0).then_some(()).ok_or(())
}

fn seal_row(root: &[u8; 32], class: &str, id: &str, plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let rng = ring::rand::SystemRandom::new();
    let mut data_key = [0u8; 32];
    let mut wrap_nonce = [0u8; 12];
    let mut payload_nonce = [0u8; 12];
    rng.fill(&mut data_key)
        .map_err(|_| "control_store_unavailable")?;
    rng.fill(&mut wrap_nonce)
        .map_err(|_| "control_store_unavailable")?;
    rng.fill(&mut payload_nonce)
        .map_err(|_| "control_store_unavailable")?;
    let aad = row_aad(class, id)?;
    let wrapped_key = seal_aead(root, &wrap_nonce, &aad, &data_key)?;
    let payload = seal_aead(&data_key, &payload_nonce, &aad, plaintext)?;
    let envelope = SealedRowV1 {
        version: 1,
        wrap_nonce: URL_SAFE_NO_PAD.encode(wrap_nonce),
        wrapped_key: URL_SAFE_NO_PAD.encode(wrapped_key),
        payload_nonce: URL_SAFE_NO_PAD.encode(payload_nonce),
        payload: URL_SAFE_NO_PAD.encode(payload),
    };
    serde_json::to_vec(&envelope).map_err(|_| "control_store_unavailable".into())
}

fn open_row(root: &[u8; 32], class: &str, id: &str, sealed: &[u8]) -> Result<Vec<u8>, String> {
    let envelope: SealedRowV1 =
        serde_json::from_slice(sealed).map_err(|_| "control_store_unavailable")?;
    if envelope.version != 1 {
        return Err("control_store_unavailable".into());
    }
    let wrap_nonce = decode_array::<12>(&envelope.wrap_nonce)?;
    let payload_nonce = decode_array::<12>(&envelope.payload_nonce)?;
    let aad = row_aad(class, id)?;
    let wrapped_key = URL_SAFE_NO_PAD
        .decode(envelope.wrapped_key)
        .map_err(|_| "control_store_unavailable")?;
    let key = open_aead(root, &wrap_nonce, &aad, &wrapped_key)?;
    let key: [u8; 32] = key.try_into().map_err(|_| "control_store_unavailable")?;
    let payload = URL_SAFE_NO_PAD
        .decode(envelope.payload)
        .map_err(|_| "control_store_unavailable")?;
    open_aead(&key, &payload_nonce, &aad, &payload)
}

fn seal_aead(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    value: &[u8],
) -> Result<Vec<u8>, String> {
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_256_GCM, key).map_err(|_| "control_store_unavailable")?,
    );
    let mut output = value.to_vec();
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(*nonce),
        aead::Aad::from(aad),
        &mut output,
    )
    .map_err(|_| "control_store_unavailable")?;
    Ok(output)
}

fn open_aead(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    value: &[u8],
) -> Result<Vec<u8>, String> {
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_256_GCM, key).map_err(|_| "control_store_unavailable")?,
    );
    let mut output = value.to_vec();
    let opened = key
        .open_in_place(
            aead::Nonce::assume_unique_for_key(*nonce),
            aead::Aad::from(aad),
            &mut output,
        )
        .map_err(|_| "control_store_unavailable")?;
    Ok(opened.to_vec())
}

fn row_aad(class: &str, id: &str) -> Result<Vec<u8>, String> {
    let mut aad = b"isyncyou-agent-control-row/v1".to_vec();
    append_len_prefixed(&mut aad, class.as_bytes())?;
    append_len_prefixed(&mut aad, id.as_bytes())?;
    Ok(aad)
}

fn append_len_prefixed(target: &mut Vec<u8>, value: &[u8]) -> Result<(), String> {
    let length = u32::try_from(value.len()).map_err(|_| "control_store_unavailable")?;
    target.extend_from_slice(&length.to_be_bytes());
    target.extend_from_slice(value);
    Ok(())
}

fn decode_array<const N: usize>(value: &str) -> Result<[u8; N], String> {
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| "control_store_unavailable")?
        .try_into()
        .map_err(|_| "control_store_unavailable".into())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

fn random_b64(bytes: usize) -> Result<String, String> {
    let mut value = vec![0u8; bytes];
    ring::rand::SystemRandom::new()
        .fill(&mut value)
        .map_err(|_| "control_store_unavailable")?;
    Ok(URL_SAFE_NO_PAD.encode(value))
}

fn presence_token_hash(token: &str) -> String {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    context.update(b"isyncyou-confirm-user-presence-token-v1\0");
    context.update(token.as_bytes());
    URL_SAFE_NO_PAD.encode(context.finish())
}

fn presence_action_hash(kind: &str, binding_digest: &str, expires_at_ms: u64) -> String {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    context.update(b"isyncyou-confirm-user-presence-v1\0");
    context.update(&(kind.len() as u32).to_be_bytes());
    context.update(kind.as_bytes());
    context.update(&(binding_digest.len() as u32).to_be_bytes());
    context.update(binding_digest.as_bytes());
    context.update(&expires_at_ms.to_be_bytes());
    context
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn u64_to_i64(value: u64) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| "control_store_unavailable".into())
}

fn control_tombstone_expiry(now_ms: u64) -> Result<i64, String> {
    now_ms
        .checked_add(CONTROL_TOMBSTONE_TTL_MS)
        .ok_or_else(|| "control_store_unavailable".to_string())
        .and_then(u64_to_i64)
}

fn create_private_directory(path: &Path) -> Result<(), String> {
    let existed = path.exists();
    if !existed {
        std::fs::create_dir_all(path).map_err(|_| "control_store_unavailable")?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let metadata = std::fs::symlink_metadata(path).map_err(|_| "control_store_unavailable")?;
        if !metadata.file_type().is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err("control_store_unavailable".into());
        }
        if existed && metadata.permissions().mode() & 0o777 != 0o700 {
            return Err("control_store_unavailable".into());
        }
        if !existed {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| "control_store_unavailable")?;
        }
    }
    if !existed {
        if let Some(parent) = path.parent() {
            sync_directory(parent).map_err(|_| "control_store_unavailable".to_string())?;
        }
    }
    Ok(())
}

fn reject_symlink_or_insecure_file(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => Err("control_store_unavailable".into()),
        Ok(metadata) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
                if metadata.uid() != unsafe { libc::geteuid() }
                    || metadata.permissions().mode() & 0o777 != 0o600
                {
                    return Err("control_store_unavailable".into());
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("control_store_unavailable".into()),
    }
}

fn secure_file_mode(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let metadata = std::fs::symlink_metadata(path).map_err(|_| "control_store_unavailable")?;
        if !metadata.file_type().is_file() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err("control_store_unavailable".into());
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| "control_store_unavailable")?;
    }
    Ok(())
}

#[cfg(feature = "encrypted-store")]
fn apply_sqlcipher_key(connection: &Connection, secret: &[u8]) -> Result<(), String> {
    let length = i32::try_from(secret.len()).map_err(|_| "control_store_key_unavailable")?;
    let result =
        unsafe { rusqlite::ffi::sqlite3_key(connection.handle(), secret.as_ptr().cast(), length) };
    (result == rusqlite::ffi::SQLITE_OK)
        .then_some(())
        .ok_or_else(|| "control_store_key_unavailable".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use isyncyou_agent::{
        parse_action, CredentialStoreConfig, CredentialStoreResolver, PendingOwnerBinding,
        PendingRegistry, SessionId,
    };
    use serde_json::json;
    use std::sync::Arc;

    const INSTALLATION_PRINCIPAL: &str = "AAAAAAAAAAAAAAAAAAAAAA";

    fn temp_root(label: &str) -> PathBuf {
        let suffix = URL_SAFE_NO_PAD.encode(ring::digest::digest(
            &ring::digest::SHA256,
            format!(
                "{label}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
            )
            .as_bytes(),
        ));
        let root = std::env::temp_dir().join(format!("isyncyou-628-control-{suffix}"));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn credential_store(root: &Path) -> AgentCredentialStore {
        CredentialStoreResolver::new(CredentialStoreConfig::new(root))
            .with_provided_key([42u8; 32])
            .resolve()
            .unwrap()
    }

    fn backup_action() -> ToolAction {
        parse_action(&json!({
            "op": "backup",
            "account": "controlled",
            "services": ["mail"]
        }))
        .unwrap()
    }

    fn owner() -> PendingOwnerBinding {
        PendingOwnerBinding {
            account: "controlled".into(),
            session_id: "session-v2".into(),
            request_id: "019f0000-0000-4000-8000-000000000001".into(),
            turn_id: "turn-v2".into(),
        }
    }

    #[test]
    fn product_request_receipt_survives_restart_replays_and_rejects_uuid_reuse() {
        let root = temp_root("product-request-receipt");
        let credential_store = credential_store(&root);
        let request_id = "019f0000-0000-4000-8000-000000000301";
        let route = "post:/api/v1/mail/send";
        let digest = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let response = StoredProductResponseV1 {
            status: 200,
            content_type: "application/json".into(),
            body: br#"{"ok":true}"#.to_vec(),
            headers: vec![("Cache-Control".into(), "no-store".into())],
        };
        {
            let control =
                AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1)
                    .unwrap();
            assert!(matches!(
                control
                    .begin_product_request(request_id, route, digest)
                    .unwrap(),
                ProductRequestBegin::Execute
            ));
            assert!(matches!(
                control
                    .begin_product_request(request_id, route, digest)
                    .unwrap(),
                ProductRequestBegin::OutcomeUnknown
            ));
            control
                .complete_product_request(request_id, route, digest, &response)
                .unwrap();
        }
        let control =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        match control
            .begin_product_request(request_id, route, digest)
            .unwrap()
        {
            ProductRequestBegin::Replay(stored) => assert_eq!(stored, response),
            _ => panic!("completed receipt must replay"),
        }
        assert!(matches!(
            control
                .begin_product_request(request_id, "post:/api/v1/calendar/create", digest,)
                .unwrap(),
            ProductRequestBegin::Conflict
        ));
    }

    #[test]
    fn product_request_uuid_binding_survives_across_route_families_without_receipt() {
        let root = temp_root("product-request-global-binding");
        let credential_store = credential_store(&root);
        let control =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let request_id = "019f0000-0000-4000-8000-000000000302";
        let route = "post:/api/v1/agent/session/create";
        let digest = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let identity = isyncyou_webui::ProductRequestIdentity {
            request_id: request_id.into(),
            route_domain: route,
            request_scope: "installation".into(),
            payload_digest: digest.into(),
        };
        assert_eq!(
            control.bind_product_request(&identity).unwrap(),
            ProductRequestBinding::Inserted
        );
        assert_eq!(
            control.bind_product_request(&identity).unwrap(),
            ProductRequestBinding::Existing
        );
        let mut cross_route = identity;
        cross_route.route_domain = "post:/api/v1/mutation-intent/create";
        assert_eq!(
            control.bind_product_request(&cross_route).unwrap(),
            ProductRequestBinding::Conflict
        );
    }

    #[test]
    fn product_request_binding_without_receipt_survives_reopen_and_checks_scope() {
        let root = temp_root("product-request-binding-only");
        let credential_store = credential_store(&root);
        let identity = isyncyou_webui::ProductRequestIdentity {
            request_id: "019f0000-0000-4000-8000-000000000305".into(),
            route_domain: "post:/api/v1/agent/session/select",
            request_scope: "session_id:01JSESSION00000000000000000".into(),
            payload_digest: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
                .into(),
        };
        {
            let control =
                AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1)
                    .unwrap();
            assert_eq!(
                control.bind_product_request(&identity).unwrap(),
                ProductRequestBinding::Inserted
            );
            let receipt_count: i64 = control
                .connection
                .lock()
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM product_request_receipts WHERE request_id=?1",
                    params![identity.request_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(receipt_count, 0);
        }

        let control =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        assert_eq!(
            control.bind_product_request(&identity).unwrap(),
            ProductRequestBinding::Existing
        );
        let mut cross_scope = identity.clone();
        cross_scope.request_scope = "session_id:01JSESSION00000000000000001".into();
        assert_eq!(
            control.bind_product_request(&cross_scope).unwrap(),
            ProductRequestBinding::Conflict
        );
    }

    #[test]
    fn product_request_key_is_domain_scope_request_and_installation_principal_bound() {
        let root = temp_root("product-request-key");
        let credential_store = credential_store(&root);
        let control =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let identity = isyncyou_webui::ProductRequestIdentity {
            request_id: "019f0000-0000-4000-8000-000000000309".into(),
            route_domain: "post:/api/v1/settings",
            request_scope: "installation".into(),
            payload_digest: "abababababababababababababababababababababababababababababababab"
                .into(),
        };
        assert_eq!(
            control.bind_product_request(&identity).unwrap(),
            ProductRequestBinding::Inserted
        );
        let stored_key: String = control
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT request_key FROM product_request_bindings WHERE request_id=?1",
                params![identity.request_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stored_key,
            product_request_key(&identity, INSTALLATION_PRINCIPAL).unwrap()
        );
        assert!(valid_sha256(&stored_key));
        assert_ne!(
            stored_key,
            product_request_key_components(
                identity.route_domain,
                "session_id:01JSESSION00000000000000000",
                &identity.request_id,
                INSTALLATION_PRINCIPAL,
            )
            .unwrap()
        );
        assert_ne!(
            stored_key,
            product_request_key_components(
                "post:/api/v1/backup",
                &identity.request_scope,
                &identity.request_id,
                INSTALLATION_PRINCIPAL,
            )
            .unwrap()
        );
        assert_ne!(
            stored_key,
            product_request_key_components(
                identity.route_domain,
                &identity.request_scope,
                "019f0000-0000-4000-8000-000000000310",
                INSTALLATION_PRINCIPAL,
            )
            .unwrap()
        );
        assert_ne!(
            stored_key,
            product_request_key(&identity, "BBBBBBBBBBBBBBBBBBBBBB").unwrap()
        );
    }

    #[test]
    fn product_request_v6_scopes_preserve_session_and_operation_bindings() {
        let root = temp_root("product-request-scope-v7");
        let credential_store = credential_store(&root);
        let receipt = isyncyou_webui::ProductRequestIdentity {
            request_id: "019f0000-0000-4000-8000-000000000320".into(),
            route_domain: "post:/api/v1/settings",
            request_scope: "account:device-local-alias".into(),
            payload_digest: "abababababababababababababababababababababababababababababababab"
                .into(),
        };
        let operation = isyncyou_webui::ProductRequestIdentity {
            request_id: "019f0000-0000-4000-8000-000000000321".into(),
            route_domain: "post:/api/v1/agent/session/pairing/reveal",
            request_scope: "operation_id:01JOPERATION00000000000000".into(),
            payload_digest: "bcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbc"
                .into(),
        };
        let session = isyncyou_webui::ProductRequestIdentity {
            request_id: "019f0000-0000-4000-8000-000000000322".into(),
            route_domain: "post:/api/v1/agent/session/select",
            request_scope: "session_id:01JSESSION00000000000000000".into(),
            payload_digest: "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"
                .into(),
        };
        {
            let control =
                AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1)
                    .unwrap();
            assert_eq!(
                control.begin_product_request_identity(&receipt).unwrap(),
                ProductRequestBegin::Execute
            );
            assert_eq!(
                control.bind_product_request(&operation).unwrap(),
                ProductRequestBinding::Inserted
            );
            assert_eq!(
                control.bind_product_request(&session).unwrap(),
                ProductRequestBinding::Inserted
            );
            control
                .connection
                .lock()
                .unwrap()
                .execute(
                    "UPDATE control_metadata SET value='6' WHERE key='schema_version'",
                    [],
                )
                .unwrap();
        }

        let control =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        for (identity, expected_scope) in [
            (&receipt, "installation"),
            (&operation, operation.request_scope.as_str()),
            (&session, session.request_scope.as_str()),
        ] {
            let (scope, key): (String, String) = control
                .connection
                .lock()
                .unwrap()
                .query_row(
                    "SELECT request_scope,request_key FROM product_request_bindings
                     WHERE request_id=?1",
                    params![identity.request_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(scope, expected_scope);
            let mut migrated = identity.clone();
            migrated.request_scope = expected_scope.into();
            assert_eq!(
                key,
                product_request_key(&migrated, INSTALLATION_PRINCIPAL).unwrap()
            );
        }
        let receipt_scope: String = control
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT request_scope FROM product_request_receipts WHERE request_id=?1",
                params![receipt.request_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(receipt_scope, "installation");
        let schema: String = control
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM control_metadata WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(schema, SCHEMA_VERSION.to_string());
        drop(control);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn product_request_v5_rows_backfill_request_key_and_preserve_scope() {
        let root = temp_root("product-request-key-migration");
        let credential_store = credential_store(&root);
        let identity = isyncyou_webui::ProductRequestIdentity {
            request_id: "019f0000-0000-4000-8000-000000000311".into(),
            route_domain: "post:/api/v1/calendar/create",
            request_scope: "session_id:01JSESSION00000000000000000".into(),
            payload_digest: "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"
                .into(),
        };
        {
            let control =
                AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1)
                    .unwrap();
            assert_eq!(
                control.begin_product_request_identity(&identity).unwrap(),
                ProductRequestBegin::Execute
            );
            let connection = control.connection.lock().unwrap();
            connection
                .execute_batch(
                    "DROP INDEX product_request_binding_key;
                     DROP INDEX product_request_receipt_key;
                     UPDATE product_request_bindings
                       SET request_key='',request_scope='installation',logical_bytes=logical_bytes-64;
                     UPDATE control_metadata SET value='5' WHERE key='schema_version';",
                )
                .unwrap();
        }

        let control =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let (request_key, request_scope): (String, String) = control
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT request_key,request_scope FROM product_request_bindings
                 WHERE request_id=?1",
                params![identity.request_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(request_scope, identity.request_scope);
        assert_eq!(
            request_key,
            product_request_key(&identity, INSTALLATION_PRINCIPAL).unwrap()
        );
        let schema: String = control
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM control_metadata WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(schema, SCHEMA_VERSION.to_string());
    }

    #[test]
    fn product_request_store_rejects_sensitive_route_response_persistence() {
        let root = temp_root("product-request-sensitive-response");
        let credential_store = credential_store(&root);
        let control =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let identity = isyncyou_webui::ProductRequestIdentity {
            request_id: "019f0000-0000-4000-8000-000000000306".into(),
            route_domain: "post:/api/v1/agent/session/pairing/reveal",
            request_scope: "operation_id:01JOPERATION00000000000000".into(),
            payload_digest: "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
                .into(),
        };
        let response = StoredProductResponseV1 {
            status: 200,
            content_type: "application/json".into(),
            body: br#"{"pairing_code":"must-not-be-stored"}"#.to_vec(),
            headers: Vec::new(),
        };
        assert_eq!(
            control
                .begin_product_request_identity(&identity)
                .unwrap_err(),
            "request_response_policy_violation"
        );
        assert_eq!(
            control
                .complete_product_request_identity(&identity, &response)
                .unwrap_err(),
            "request_response_policy_violation"
        );
        let share_identity = isyncyou_webui::ProductRequestIdentity {
            request_id: "019f0000-0000-4000-8000-000000000307".into(),
            route_domain: "post:/api/v1/share",
            request_scope: "account:a".into(),
            payload_digest: "abababababababababababababababababababababababababababababababab"
                .into(),
        };
        let share_response = StoredProductResponseV1 {
            status: 200,
            content_type: "application/json".into(),
            body: br#"{"webUrl":"must-not-be-stored","invited":["must-not-be-stored"]}"#.to_vec(),
            headers: Vec::new(),
        };
        assert_eq!(
            control
                .begin_product_request_identity(&share_identity)
                .unwrap_err(),
            "request_response_policy_violation"
        );
        assert_eq!(
            control
                .complete_product_request_identity(&share_identity, &share_response)
                .unwrap_err(),
            "request_response_policy_violation"
        );
        let receipt_count: i64 = control
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM product_request_receipts
                 WHERE request_id IN (?1, ?2)",
                params![identity.request_id, share_identity.request_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(receipt_count, 0);
    }

    #[test]
    fn agent_turn_admission_is_encrypted_restart_recoverable_and_conflict_safe() {
        let root = temp_root("agent-turn-admission");
        let credential_store = credential_store(&root);
        let request = isyncyou_webui::AgentTurnRequest {
            request_id: "019f0000-0000-4000-8000-000000000303".into(),
            session_id: "01JSESSION00000000000000000".into(),
            account: "controlled".into(),
            prompt: "Read one controlled item".into(),
        };
        let turn_id = "01JTURN0000000000000000000";
        let digest = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
        {
            let control =
                AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1)
                    .unwrap();
            assert_eq!(
                control
                    .begin_agent_turn_admission(&request, turn_id, digest)
                    .unwrap(),
                AgentTurnAdmissionBegin::Inserted
            );
            assert_eq!(
                control
                    .begin_agent_turn_admission(&request, turn_id, digest)
                    .unwrap(),
                AgentTurnAdmissionBegin::Existing
            );
            let mut changed = request.clone();
            changed.prompt = "Different request".into();
            assert_eq!(
                control
                    .begin_agent_turn_admission(
                        &changed,
                        turn_id,
                        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                    )
                    .unwrap_err(),
                "request_id_conflict"
            );
        }

        let control =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        assert_eq!(
            control.recover_agent_turn_admissions(8).unwrap(),
            vec![RecoveredAgentTurnAdmission {
                request: request.clone(),
                turn_id: turn_id.into(),
                identity: isyncyou_webui::ProductRequestIdentity {
                    request_id: request.request_id.clone(),
                    route_domain: AGENT_TURN_ROUTE_DOMAIN,
                    request_scope: format!("session_id:{}", request.session_id),
                    payload_digest: digest.into(),
                },
            }]
        );
        control
            .complete_agent_turn_admission(&request.request_id)
            .unwrap();
        assert!(control.recover_agent_turn_admissions(8).unwrap().is_empty());
    }

    #[test]
    fn cancelled_turn_admission_scrubs_request_and_is_not_recovered_after_restart() {
        let root = temp_root("agent-turn-admission-cancelled");
        let credential_store = credential_store(&root);
        let request = isyncyou_webui::AgentTurnRequest {
            request_id: "019f0000-0000-4000-8000-000000000312".into(),
            session_id: "01JSESSION00000000000000012".into(),
            account: "controlled".into(),
            prompt: "Sensitive prompt that must be scrubbed after cancellation".into(),
        };
        let turn_id = "01JTURN0000000000000000012";
        let digest = "1212121212121212121212121212121212121212121212121212121212121212";
        {
            let control =
                AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1)
                    .unwrap();
            assert_eq!(
                control
                    .begin_agent_turn_admission(&request, turn_id, digest)
                    .unwrap(),
                AgentTurnAdmissionBegin::Inserted
            );
            control.cancel_agent_turn_admission(turn_id).unwrap();
            control.cancel_agent_turn_admission(turn_id).unwrap();
            assert_eq!(
                control
                    .agent_turn_admission_terminal(
                        &request.request_id,
                        &format!("session_id:{}", request.session_id),
                    )
                    .unwrap(),
                Some((isyncyou_agent::RequestPhase::Cancelled, None))
            );
            assert!(control.recover_agent_turn_admissions(8).unwrap().is_empty());
            let (state, sealed_bytes): (String, i64) = control
                .connection
                .lock()
                .unwrap()
                .query_row(
                    "SELECT state,length(sealed_request) FROM agent_turn_admissions
                     WHERE request_id=?1",
                    params![request.request_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(state, "cancelled");
            assert_eq!(sealed_bytes, 0);
        }

        let control =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        assert!(control.recover_agent_turn_admissions(8).unwrap().is_empty());
        assert_eq!(
            control
                .begin_agent_turn_admission(&request, turn_id, digest)
                .unwrap(),
            AgentTurnAdmissionBegin::Cancelled
        );
        drop(control);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_turn_admission_is_terminal_scrubbed_and_restart_replayable() {
        let root = temp_root("agent-turn-admission-failed");
        let credential_store = credential_store(&root);
        let request = isyncyou_webui::AgentTurnRequest {
            request_id: "019f0000-0000-4000-8000-000000000313".into(),
            session_id: "01JSESSION00000000000000013".into(),
            account: "controlled".into(),
            prompt: "Sensitive prompt that must be scrubbed after startup failure".into(),
        };
        let turn_id = "01JTURN0000000000000000013";
        let digest = "1313131313131313131313131313131313131313131313131313131313131313";
        {
            let control =
                AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1)
                    .unwrap();
            assert_eq!(
                control
                    .begin_agent_turn_admission(&request, turn_id, digest)
                    .unwrap(),
                AgentTurnAdmissionBegin::Inserted
            );
            assert_eq!(
                control
                    .fail_agent_turn_admission(turn_id, "session_transport_unavailable")
                    .unwrap(),
                AgentTurnAdmissionFailure::Failed
            );
            assert_eq!(
                control
                    .fail_agent_turn_admission(turn_id, "session_transport_unavailable")
                    .unwrap(),
                AgentTurnAdmissionFailure::Failed
            );
            assert_eq!(
                control
                    .fail_agent_turn_admission(turn_id, "raw transport detail")
                    .unwrap_err(),
                "turn_admission_unavailable"
            );
            assert_eq!(
                control
                    .agent_turn_admission_terminal(
                        &request.request_id,
                        &format!("session_id:{}", request.session_id),
                    )
                    .unwrap(),
                Some((
                    isyncyou_agent::RequestPhase::Failed,
                    Some("session_transport_unavailable".into()),
                ))
            );
            assert!(control.recover_agent_turn_admissions(8).unwrap().is_empty());
            let (state, terminal_code, sealed_bytes): (String, String, i64) = control
                .connection
                .lock()
                .unwrap()
                .query_row(
                    "SELECT state,terminal_code,length(sealed_request)
                     FROM agent_turn_admissions WHERE request_id=?1",
                    params![request.request_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!(state, "active");
            assert_eq!(terminal_code, "session_transport_unavailable");
            assert_eq!(sealed_bytes, 0);
            assert_eq!(
                control.cancel_agent_turn_admission(turn_id).unwrap_err(),
                "turn_admission_unavailable"
            );
        }

        let control =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        assert!(control.recover_agent_turn_admissions(8).unwrap().is_empty());
        assert_eq!(
            control
                .begin_agent_turn_admission(&request, turn_id, digest)
                .unwrap(),
            AgentTurnAdmissionBegin::Failed("session_transport_unavailable".into())
        );
        drop(control);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn agent_turn_admission_v10_migration_adds_terminal_code_without_losing_active_turn() {
        let root = temp_root("agent-turn-admission-v10-migration");
        let credential_store = credential_store(&root);
        let request = isyncyou_webui::AgentTurnRequest {
            request_id: "019f0000-0000-4000-8000-000000000309".into(),
            session_id: "01JSESSION00000000000000005".into(),
            account: "controlled".into(),
            prompt: "Read one controlled item".into(),
        };
        let turn_id = "01JTURN0000000000000000005";
        let digest = "efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef";
        {
            let control =
                AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1)
                    .unwrap();
            assert_eq!(
                control
                    .begin_agent_turn_admission(&request, turn_id, digest)
                    .unwrap(),
                AgentTurnAdmissionBegin::Inserted
            );
            control
                .connection
                .lock()
                .unwrap()
                .execute_batch(
                    "ALTER TABLE agent_turn_admissions DROP COLUMN terminal_code;
                     UPDATE control_metadata SET value='10' WHERE key='schema_version';",
                )
                .unwrap();
        }

        let control =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let recovered = control.recover_agent_turn_admissions(8).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].turn_id, turn_id);
        assert_eq!(recovered[0].request, request);
        let (terminal_code, schema): (Option<String>, String) = control
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT a.terminal_code,m.value
                 FROM agent_turn_admissions a
                 JOIN control_metadata m ON m.key='schema_version'
                 WHERE a.request_id=?1",
                params![request.request_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(terminal_code, None);
        assert_eq!(schema, SCHEMA_VERSION.to_string());
        drop(control);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_turn_admission_migrates_to_scoped_v2_binding_on_reopen() {
        let root = temp_root("agent-turn-admission-v1-migration");
        let credential_store = credential_store(&root);
        let request = isyncyou_webui::AgentTurnRequest {
            request_id: "019f0000-0000-4000-8000-000000000308".into(),
            session_id: "01JSESSION00000000000000004".into(),
            account: "controlled".into(),
            prompt: "Read one controlled item".into(),
        };
        let turn_id = "01JTURN0000000000000000004";
        let digest = "abababababababababababababababababababababababababababababababab";
        {
            let control =
                AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1)
                    .unwrap();
            control
                .begin_agent_turn_admission(&request, turn_id, digest)
                .unwrap();
            let legacy = LegacyAgentTurnAdmissionV1 {
                version: 1,
                turn_id: turn_id.into(),
                payload_digest: digest.into(),
                request: request.clone(),
            };
            let plaintext = serde_json::to_vec(&legacy).unwrap();
            let sealed = seal_row(
                &control.row_wrap_key,
                "agent-turn-admission",
                &request.request_id,
                &plaintext,
            )
            .unwrap();
            let connection = control.connection.lock().unwrap();
            connection
                .execute(
                    "UPDATE agent_turn_admissions
                     SET request_scope='installation',sealed_request=?1
                     WHERE request_id=?2",
                    params![sealed, request.request_id],
                )
                .unwrap();
            connection
                .execute(
                    "UPDATE product_request_bindings
                     SET request_scope='installation',expires_at_ms=1
                     WHERE request_id=?1",
                    params![request.request_id],
                )
                .unwrap();
            connection
                .execute(
                    "UPDATE control_metadata SET value='4' WHERE key='schema_version'",
                    [],
                )
                .unwrap();
        }

        let control =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let recovered = control.recover_agent_turn_admissions(8).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].identity.request_scope,
            format!("session_id:{}", request.session_id)
        );
        let (scope, expires_at_ms): (String, i64) = control
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT request_scope,expires_at_ms FROM product_request_bindings
                 WHERE request_id=?1",
                params![request.request_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(scope, format!("session_id:{}", request.session_id));
        assert_eq!(expires_at_ms, i64::MAX);
    }

    #[test]
    fn agent_turn_binding_and_admission_rollback_together_on_insert_failure() {
        let root = temp_root("agent-turn-admission-atomic-rollback");
        let credential_store = credential_store(&root);
        let control =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let request = isyncyou_webui::AgentTurnRequest {
            request_id: "019f0000-0000-4000-8000-000000000304".into(),
            session_id: "01JSESSION00000000000000001".into(),
            account: "controlled".into(),
            prompt: "Read one controlled item".into(),
        };
        let identity = isyncyou_webui::ProductRequestIdentity {
            request_id: request.request_id.clone(),
            route_domain: AGENT_TURN_ROUTE_DOMAIN,
            request_scope: format!("session_id:{}", request.session_id),
            payload_digest: "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                .into(),
        };
        control
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_agent_turn_admission
                 BEFORE INSERT ON agent_turn_admissions
                 BEGIN SELECT RAISE(ABORT, 'controlled failure'); END;",
            )
            .unwrap();

        assert_eq!(
            control
                .begin_agent_turn_admission_identity(
                    &request,
                    "01JTURN0000000000000000001",
                    &identity
                )
                .unwrap_err(),
            "turn_admission_unavailable"
        );
        let connection = control.connection.lock().unwrap();
        let binding_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM product_request_bindings WHERE request_id=?1",
                params![request.request_id],
                |row| row.get(0),
            )
            .unwrap();
        let admission_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM agent_turn_admissions WHERE request_id=?1",
                params![request.request_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!((binding_count, admission_count), (0, 0));
    }

    #[test]
    fn product_request_validation_abort_allows_corrected_retry() {
        let root = temp_root("product-request-abort");
        let credential_store = credential_store(&root);
        let control =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let request_id = "019f0000-0000-4000-8000-000000000302";
        let route = "post:/api/v1/onedrive/create";
        let digest = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        assert!(matches!(
            control
                .begin_product_request(request_id, route, digest)
                .unwrap(),
            ProductRequestBegin::Execute
        ));
        control
            .abort_product_request(request_id, route, digest)
            .unwrap();
        assert!(matches!(
            control
                .begin_product_request(request_id, route, digest)
                .unwrap(),
            ProductRequestBegin::Execute
        ));
    }

    #[test]
    fn product_request_receipt_reaper_applies_bounded_thirty_day_retention() {
        let root = temp_root("product-request-retention");
        let credential_store = credential_store(&root);
        let control =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let request_id = "019f0000-0000-4000-8000-000000000303";
        let route = "post:/api/v1/calendar/create";
        let digest = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
        assert!(matches!(
            control
                .begin_product_request(request_id, route, digest)
                .unwrap(),
            ProductRequestBegin::Execute
        ));
        control
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE product_request_receipts SET expires_at_ms=1 WHERE request_id=?1",
                params![request_id],
            )
            .unwrap();

        assert_eq!(control.reap_expired(2, 1).unwrap(), 1);
        assert!(matches!(
            control
                .begin_product_request(request_id, route, digest)
                .unwrap(),
            ProductRequestBegin::Execute
        ));
    }

    #[test]
    fn control_reaper_limit_is_global_across_all_record_classes() {
        let root = temp_root("control-reaper-global-limit");
        let credential_store = credential_store(&root);
        let control =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        {
            let connection = control.connection.lock().unwrap();
            connection
                .execute(
                    "INSERT INTO confirmation_intents(
                       intent_id,account_id,session_id,request_id,turn_id,owner_binding,
                       action_hash,expires_at_ms,state,sealed_payload,logical_bytes
                     ) VALUES('expired-confirmation','account','session','request','turn','owner',
                       'hash',1,'pending',X'01',1)",
                    [],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO product_request_receipts(
                       request_id,request_key,route_domain,request_scope,payload_digest,state,
                       sealed_response,expires_at_ms,logical_bytes
                     ) VALUES('expired-receipt','key','post:/api/v1/items','installation','digest',
                       'completed',NULL,1,1)",
                    [],
                )
                .unwrap();
        }

        assert_eq!(control.reap_expired(2, 1).unwrap(), 1);
        let connection = control.connection.lock().unwrap();
        let confirmation_state: String = connection
            .query_row(
                "SELECT state FROM confirmation_intents WHERE intent_id='expired-confirmation'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let receipt_exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM product_request_receipts
                   WHERE request_id='expired-receipt')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(confirmation_state, "expired");
        assert!(receipt_exists);
        drop(connection);

        assert_eq!(control.reap_expired(2, 1).unwrap(), 1);
        let receipt_exists: bool = control
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM product_request_receipts
                   WHERE request_id='expired-receipt')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!receipt_exists);
        drop(control);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_maintenance_cursor_is_persistent_bounded_and_wraps() {
        let root = temp_root("session-maintenance-cursor");
        let credential_store = credential_store(&root);
        let control =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        assert_eq!(control.claim_session_maintenance_offset(10, 4).unwrap(), 0);
        assert_eq!(control.claim_session_maintenance_offset(10, 4).unwrap(), 4);
        drop(control);

        let reopened =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        assert_eq!(reopened.claim_session_maintenance_offset(10, 4).unwrap(), 8);
        assert_eq!(reopened.claim_session_maintenance_offset(10, 4).unwrap(), 2);
        assert_eq!(reopened.claim_session_maintenance_offset(0, 4).unwrap(), 0);
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn agent_turn_request_binding_is_not_removed_by_time_based_reaping() {
        let root = temp_root("agent-turn-request-retention");
        let credential_store = credential_store(&root);
        let control =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let request_id = "019f0000-0000-4000-8000-000000000304";
        let digest = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
        let identity = isyncyou_webui::ProductRequestIdentity {
            request_id: request_id.into(),
            route_domain: AGENT_TURN_ROUTE_DOMAIN,
            request_scope: "session_id:01JSESSION00000000000000002".into(),
            payload_digest: digest.into(),
        };
        assert_eq!(
            control.bind_product_request(&identity).unwrap(),
            ProductRequestBinding::Inserted
        );

        control.reap_expired(i64::MAX as u64 - 1, 256).unwrap();
        assert_eq!(
            control.bind_product_request(&identity).unwrap(),
            ProductRequestBinding::Existing
        );
        let mut changed = identity;
        changed.payload_digest =
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into();
        assert_eq!(
            control.bind_product_request(&changed).unwrap(),
            ProductRequestBinding::Conflict
        );
    }

    #[test]
    fn confirmation_store_survives_restart_and_consumes_exactly_once() {
        let root = temp_root("restart");
        let store = credential_store(&root);
        let persistence =
            AgentControlStore::open(&root, &store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let registry = PendingRegistry::with_persistence(Arc::new(persistence));
        let (pending, token) = registry
            .register_bound(backup_action(), "backup", 1_000, 60_000, owner())
            .unwrap();
        assert_eq!(
            registry.confirm(&pending.id, "wrong", &pending.action_hash, 2_000),
            Err(ConfirmError::BadToken)
        );
        drop(registry);

        let persistence =
            AgentControlStore::open(&root, &store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let registry = PendingRegistry::with_persistence(Arc::new(persistence));
        assert_eq!(
            registry
                .confirm(&pending.id, &token, &pending.action_hash, 2_001)
                .unwrap(),
            backup_action()
        );
        assert_eq!(
            registry.confirm(&pending.id, &token, &pending.action_hash, 2_002),
            Err(ConfirmError::NotFound)
        );
        drop(registry);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn agent_control_store_rejects_second_writer_process_lock() {
        let root = temp_root("lock");
        let store = credential_store(&root);
        let first = AgentControlStore::open(&root, &store, INSTALLATION_PRINCIPAL, 1).unwrap();
        assert_eq!(
            AgentControlStore::open(&root, &store, INSTALLATION_PRINCIPAL, 1).unwrap_err(),
            "control_store_busy"
        );
        drop(first);
        assert!(AgentControlStore::open(&root, &store, INSTALLATION_PRINCIPAL, 1).is_ok());
    }

    #[test]
    fn agent_control_store_child_lock_probe() {
        let Some(root) = std::env::var_os("ISY_CONTROL_STORE_CHILD_LOCK_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let store = credential_store(&root);
        assert_eq!(
            AgentControlStore::open(&root, &store, INSTALLATION_PRINCIPAL, 1).unwrap_err(),
            "control_store_busy"
        );
    }

    #[test]
    fn agent_control_store_real_child_process_cannot_acquire_second_writer() {
        let root = temp_root("child-process-lock");
        let store = credential_store(&root);
        let first = AgentControlStore::open(&root, &store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "agent_control_store::tests::agent_control_store_child_lock_probe",
                "--nocapture",
            ])
            .env("ISY_CONTROL_STORE_CHILD_LOCK_ROOT", &root)
            .status()
            .unwrap();
        assert!(status.success());
        drop(first);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn agent_control_store_rejects_symlink_non_owner_or_permissive_files() {
        use std::os::unix::fs::PermissionsExt as _;

        let permissive_root = temp_root("permissive-file");
        let control_root = permissive_root.join("agent-control");
        std::fs::create_dir(&control_root).unwrap();
        std::fs::set_permissions(&control_root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let db_path = control_root.join(".isyncyou-agent-control.db");
        std::fs::write(&db_path, b"not-a-database").unwrap();
        std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let store = credential_store(&permissive_root);
        assert_eq!(
            AgentControlStore::open(&permissive_root, &store, INSTALLATION_PRINCIPAL, 1)
                .unwrap_err(),
            "control_store_unavailable"
        );

        let symlink_root = temp_root("symlink-file");
        let control_root = symlink_root.join("agent-control");
        std::fs::create_dir(&control_root).unwrap();
        std::fs::set_permissions(&control_root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let target = symlink_root.join("target.db");
        std::fs::write(&target, b"target").unwrap();
        std::os::unix::fs::symlink(&target, control_root.join(".isyncyou-agent-control.db"))
            .unwrap();
        let store = credential_store(&symlink_root);
        assert_eq!(
            AgentControlStore::open(&symlink_root, &store, INSTALLATION_PRINCIPAL, 1).unwrap_err(),
            "control_store_unavailable"
        );

        std::fs::remove_dir_all(permissive_root).unwrap();
        std::fs::remove_dir_all(symlink_root).unwrap();
    }

    #[test]
    fn agent_control_store_key_version_mismatch_or_unimplemented_rotation_fails_closed() {
        let root = temp_root("key-version");
        let store = credential_store(&root);
        drop(AgentControlStore::open(&root, &store, INSTALLATION_PRINCIPAL, 1).unwrap());
        assert_eq!(
            AgentControlStore::open(&root, &store, INSTALLATION_PRINCIPAL, 2).unwrap_err(),
            "control_store_database_config_failed"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn agent_control_store_key_uses_domain_hmac_without_master_key_export() {
        let root = temp_root("key-root");
        let store = credential_store(&root);
        let mut message = Vec::new();
        append_len_prefixed(&mut message, INSTALLATION_PRINCIPAL.as_bytes()).unwrap();
        message.extend_from_slice(&1u32.to_be_bytes());
        let first = store.domain_hmac(CONTROL_KEY_DOMAIN, &message).unwrap();
        let second = store.domain_hmac(CONTROL_KEY_DOMAIN, &message).unwrap();
        assert_eq!(first, second);
        message.pop();
        message.push(2);
        assert_ne!(
            first,
            store.domain_hmac(CONTROL_KEY_DOMAIN, &message).unwrap()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn agent_control_store_separates_sqlcipher_and_row_wrap_subkeys() {
        let control_root = [42u8; 32];
        let (sqlcipher_key, row_wrap_key) = derive_control_subkeys(&control_root).unwrap();
        assert_ne!(sqlcipher_key, row_wrap_key);
        let same = derive_control_subkeys(&control_root).unwrap();
        assert_eq!(sqlcipher_key[..], same.0[..]);
        assert_eq!(row_wrap_key[..], same.1[..]);
        let different = derive_control_subkeys(&[43u8; 32]).unwrap();
        assert_ne!(sqlcipher_key[..], different.0[..]);
        assert_ne!(row_wrap_key[..], different.1[..]);
    }

    #[test]
    fn agent_control_store_path_is_app_wide_not_account_archive() {
        let root = temp_root("app-wide-path");
        let account_archive = root.join("accounts").join("controlled");
        std::fs::create_dir_all(&account_archive).unwrap();
        let store = credential_store(&root);
        let control = AgentControlStore::open(&root, &store, INSTALLATION_PRINCIPAL, 1).unwrap();
        assert_eq!(control.root, root.join("agent-control"));
        assert!(!control.root.starts_with(&account_archive));
        drop(control);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn control_store_principal_mismatch_fails_closed() {
        let root = temp_root("principal-mismatch");
        let store = credential_store(&root);
        drop(AgentControlStore::open(&root, &store, INSTALLATION_PRINCIPAL, 1).unwrap());
        let error =
            AgentControlStore::open(&root, &store, "BBBBBBBBBBBBBBBBBBBBBB", 1).unwrap_err();
        assert!(matches!(
            error.as_str(),
            "control_store_database_config_failed" | "control_store_identity_mismatch"
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn agent_control_store_migration_rolls_back_atomically() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE control_metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO control_metadata VALUES('installation_binding','wrong');
                 INSERT INTO control_metadata VALUES('key_version','1');
                 INSERT INTO control_metadata VALUES('schema_version','1');",
            )
            .unwrap();
        let migration = connection.unchecked_transaction().unwrap();
        migration
            .execute_batch("CREATE TABLE partially_migrated(value TEXT NOT NULL);")
            .unwrap();
        assert_eq!(
            initialize_metadata(&migration, "expected", 1).unwrap_err(),
            "control_store_identity_mismatch"
        );
        drop(migration);
        let table_exists: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='partially_migrated'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_exists, 0);
    }

    #[test]
    fn agent_control_store_migrates_v1_metadata_to_current_schema_transactionally() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE control_metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO control_metadata VALUES('installation_binding','expected');
                 INSERT INTO control_metadata VALUES('key_version','1');
                 INSERT INTO control_metadata VALUES('schema_version','1');",
            )
            .unwrap();
        let migration = connection.unchecked_transaction().unwrap();
        initialize_metadata(&migration, "expected", 1).unwrap();
        migration.commit().unwrap();
        let schema: String = connection
            .query_row(
                "SELECT value FROM control_metadata WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(schema, SCHEMA_VERSION.to_string());
    }

    #[test]
    fn confirmation_reaper_erases_sensitive_fields_and_keeps_only_bounded_tombstone() {
        let root = temp_root("reaper");
        let store = credential_store(&root);
        let persistence =
            Arc::new(AgentControlStore::open(&root, &store, INSTALLATION_PRINCIPAL, 1).unwrap());
        let registry = PendingRegistry::with_persistence(persistence.clone());
        let (pending, token) = registry
            .register_bound(backup_action(), "backup", 1_000, 60_000, owner())
            .unwrap();
        assert_eq!(persistence.reap_expired(61_001, 16).unwrap(), 1);
        assert_eq!(
            registry.confirm(&pending.id, &token, &pending.action_hash, 61_001),
            Err(ConfirmError::NotFound)
        );
        let connection = persistence.connection.lock().unwrap();
        let (state, payload, bytes): (String, Option<Vec<u8>>, i64) = connection
            .query_row(
                "SELECT state,sealed_payload,logical_bytes FROM confirmation_intents WHERE intent_id=?1",
                params![pending.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, "expired");
        assert!(payload.is_none());
        assert_eq!(bytes, 0);
    }

    #[test]
    fn control_store_terminal_erasure_removes_wrapped_row_key_and_truncates_wal() {
        let root = temp_root("terminal-erasure");
        let credential_store = credential_store(&root);
        let persistence = Arc::new(
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap(),
        );
        let registry = PendingRegistry::with_persistence(persistence.clone());
        let (pending, _) = registry
            .register_bound(backup_action(), "backup", 1_000, 60_000, owner())
            .unwrap();
        {
            let connection = persistence.connection.lock().unwrap();
            let sealed: Vec<u8> = connection
                .query_row(
                    "SELECT sealed_payload FROM confirmation_intents WHERE intent_id=?1",
                    params![&pending.id],
                    |row| row.get(0),
                )
                .unwrap();
            let envelope: SealedRowV1 = serde_json::from_slice(&sealed).unwrap();
            assert!(!envelope.wrapped_key.is_empty());
        }

        persistence.reap_expired(61_001, 16).unwrap();
        let connection = persistence.connection.lock().unwrap();
        let sealed: Option<Vec<u8>> = connection
            .query_row(
                "SELECT sealed_payload FROM confirmation_intents WHERE intent_id=?1",
                params![&pending.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(sealed.is_none());
        drop(connection);

        let wal = root
            .join("agent-control")
            .join(".isyncyou-agent-control.db-wal");
        assert!(!wal.exists() || std::fs::metadata(&wal).unwrap().len() == 0);
        drop(registry);
        drop(persistence);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pending_cancel_retry_returns_same_owner_without_restoring_authority() {
        let root = temp_root("pending-cancel-retry");
        let store = credential_store(&root);
        let persistence =
            Arc::new(AgentControlStore::open(&root, &store, INSTALLATION_PRINCIPAL, 1).unwrap());
        let registry = PendingRegistry::with_persistence(persistence.clone());
        let expected_owner = owner();
        let (pending, token) = registry
            .register_bound(
                backup_action(),
                "backup",
                1_000,
                60_000,
                expected_owner.clone(),
            )
            .unwrap();

        let first = registry
            .cancel(&pending.id, &pending.action_hash, 2_000)
            .unwrap();
        let retry = registry
            .cancel(&pending.id, &pending.action_hash, 2_001)
            .unwrap();
        assert_eq!(first, expected_owner);
        assert_eq!(retry, expected_owner);
        assert_eq!(
            registry.confirm(&pending.id, &token, &pending.action_hash, 2_001),
            Err(ConfirmError::NotFound)
        );
        assert_eq!(
            registry.binding(&pending.id, &pending.action_hash, 2_001),
            Err(ConfirmError::NotFound)
        );

        let connection = persistence.connection.lock().unwrap();
        let (state, payload, bytes): (String, Option<Vec<u8>>, i64) = connection
            .query_row(
                "SELECT state,sealed_payload,logical_bytes FROM confirmation_intents WHERE intent_id=?1",
                params![pending.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, "cancelled");
        assert!(payload.is_none());
        assert_eq!(bytes, 0);
        drop(connection);
        let projections = persistence.pending_cancel_projections(8).unwrap();
        assert_eq!(projections.len(), 1);
        assert_eq!(projections[0].pending_id, pending.id);
        assert_eq!(projections[0].owner, expected_owner);
        assert_eq!(projections[0].created_at_ms, 2_000);
        drop(registry);
        drop(persistence);

        let reopened = AgentControlStore::open(&root, &store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let projections = reopened.pending_cancel_projections(8).unwrap();
        assert_eq!(projections.len(), 1);
        reopened
            .complete_pending_cancel_projection(&pending.id)
            .unwrap();
        assert!(reopened.pending_cancel_projections(8).unwrap().is_empty());
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pending_cancel_v1_migration_backfills_unprojected_cancelled_authority() {
        let root = temp_root("pending-cancel-v1-backfill");
        let credential_store = credential_store(&root);
        let persistence = Arc::new(
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap(),
        );
        let registry = PendingRegistry::with_persistence(persistence.clone());
        let (pending, _) = registry
            .register_bound(backup_action(), "backup", 1_000, 60_000, owner())
            .unwrap();
        registry
            .cancel(&pending.id, &pending.action_hash, 2_000)
            .unwrap();
        {
            let connection = persistence.connection.lock().unwrap();
            connection
                .execute("DELETE FROM pending_cancel_projections", [])
                .unwrap();
            connection
                .execute(
                    "UPDATE control_metadata SET value='1' WHERE key='schema_version'",
                    [],
                )
                .unwrap();
        }
        drop(registry);
        drop(persistence);

        let reopened =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let projections = reopened.pending_cancel_projections(8).unwrap();
        assert_eq!(projections.len(), 1);
        assert_eq!(projections[0].pending_id, pending.id);
        assert_eq!(projections[0].owner, owner());
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn control_store_create_paths_reap_expired_sensitive_rows() {
        let root = temp_root("create-path-reaper");
        let credential_store = credential_store(&root);
        let persistence = Arc::new(
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap(),
        );
        let registry = PendingRegistry::with_persistence(persistence.clone());
        let (pending, _) = registry
            .register_bound(backup_action(), "backup", 1_000, 10, owner())
            .unwrap();

        persistence
            .start_user_presence(
                "019f0000-0000-4000-8000-000000000001",
                UserPresenceBinding::Archive {
                    session_id: "session-v2".into(),
                },
                1_011,
            )
            .unwrap();

        let connection = persistence.connection.lock().unwrap();
        let (state, payload, bytes): (String, Option<Vec<u8>>, i64) = connection
            .query_row(
                "SELECT state,sealed_payload,logical_bytes FROM confirmation_intents WHERE intent_id=?1",
                params![pending.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, "expired");
        assert!(payload.is_none());
        assert_eq!(bytes, 0);
        drop(connection);
        drop(registry);
        drop(persistence);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn confirmation_hash_domain_separates_agent_tool_and_user_presence_classes() {
        let root = temp_root("presence-domain");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let challenge = store
            .start_user_presence(
                "019f0000-0000-4000-8000-000000000002",
                UserPresenceBinding::Archive {
                    session_id: "session-v2".into(),
                },
                1_000,
            )
            .unwrap();
        assert_eq!(challenge.action_hash.len(), 64);
        assert_ne!(
            challenge.action_hash,
            isyncyou_agent::action_hash(&backup_action(), challenge.expires_at_ms).unwrap()
        );
    }

    #[test]
    fn confirmation_store_atomically_consumes_token_and_authorizes_reveal() {
        let root = temp_root("presence-reveal");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let binding = UserPresenceBinding::PairingReveal {
            session_id: "session-v2".into(),
            pair_id: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
        };
        let challenge = store
            .start_user_presence(
                "019f0000-0000-4000-8000-000000000003",
                binding.clone(),
                1_000,
            )
            .unwrap();
        assert_eq!(
            store.confirm_user_presence(
                &challenge.operation_id,
                &challenge.intent_id,
                "wrong",
                &challenge.action_hash,
                2_000,
            ),
            Err("presence_token_invalid".into())
        );
        store
            .confirm_user_presence(
                &challenge.operation_id,
                &challenge.intent_id,
                &challenge.token,
                &challenge.action_hash,
                2_001,
            )
            .unwrap();
        assert_eq!(
            store.confirm_user_presence(
                &challenge.operation_id,
                &challenge.intent_id,
                "wrong",
                &challenge.action_hash,
                2_002,
            ),
            Err("presence_token_invalid".into())
        );
        assert_eq!(
            store
                .consume_user_presence(
                    &challenge.operation_id,
                    "session_pairing_reveal",
                    "019f0000-0000-4000-8000-000000000030",
                    2_002,
                )
                .unwrap(),
            binding
        );
        assert_eq!(
            store.consume_user_presence(
                &challenge.operation_id,
                "session_pairing_reveal",
                "019f0000-0000-4000-8000-000000000031",
                2_003,
            ),
            Err("request_id_conflict".into())
        );
    }

    #[test]
    fn pairing_reveal_is_authorized_atomic_and_restart_replayable() {
        let root = temp_root("pairing-source");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let payload = PairingPayload::generate(SessionId::new("session-v2").unwrap()).unwrap();
        let source = store
            .create_pairing_source(
                "019f0000-0000-4000-8000-000000000005",
                "session-v2",
                &payload,
                1_000,
            )
            .unwrap();
        let challenge = store
            .start_user_presence(
                "019f0000-0000-4000-8000-000000000004",
                UserPresenceBinding::PairingReveal {
                    session_id: "session-v2".into(),
                    pair_id: source.pair_id.clone(),
                },
                1_001,
            )
            .unwrap();
        assert_eq!(
            store
                .consume_pairing_reveal(
                    &challenge.operation_id,
                    "019f0000-0000-4000-8000-000000000006",
                    1_002,
                )
                .unwrap_err(),
            "presence_not_authorized"
        );
        store
            .confirm_user_presence(
                &challenge.operation_id,
                &challenge.intent_id,
                &challenge.token,
                &challenge.action_hash,
                1_003,
            )
            .unwrap();
        let first = store
            .consume_pairing_reveal(
                &challenge.operation_id,
                "019f0000-0000-4000-8000-000000000006",
                1_004,
            )
            .unwrap();
        let first_code = first.reveal_code();
        drop(store);

        let reopened =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        assert_eq!(
            reopened
                .consume_pairing_reveal(
                    &challenge.operation_id,
                    "019f0000-0000-4000-8000-000000000006",
                    1_005,
                )
                .unwrap()
                .reveal_code(),
            first_code
        );
        assert_eq!(
            reopened
                .consume_pairing_reveal(
                    &challenge.operation_id,
                    "019f0000-0000-4000-8000-000000000007",
                    1_006,
                )
                .unwrap_err(),
            "request_id_conflict"
        );
        assert_eq!(reopened.reap_expired(302_000, 16).unwrap(), 2);
        assert_eq!(
            reopened
                .consume_pairing_reveal(
                    &challenge.operation_id,
                    "019f0000-0000-4000-8000-000000000006",
                    302_001,
                )
                .unwrap_err(),
            "presence_expired"
        );
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pairing_revoke_retains_authority_until_remote_completion_and_replays_exact_request() {
        let root = temp_root("pairing-revoke-resume");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let payload = PairingPayload::generate(SessionId::new("session-v2").unwrap()).unwrap();
        let source = store
            .create_pairing_source(
                "019f0000-0000-4000-8000-000000000040",
                "session-v2",
                &payload,
                1_000,
            )
            .unwrap();
        let challenge = store
            .start_user_presence(
                "019f0000-0000-4000-8000-000000000041",
                UserPresenceBinding::PairingReveal {
                    session_id: "session-v2".into(),
                    pair_id: source.pair_id,
                },
                1_001,
            )
            .unwrap();
        store
            .confirm_user_presence(
                &challenge.operation_id,
                &challenge.intent_id,
                &challenge.token,
                &challenge.action_hash,
                1_002,
            )
            .unwrap();
        let revealed = store
            .consume_pairing_reveal(
                &challenge.operation_id,
                "019f0000-0000-4000-8000-000000000041",
                1_003,
            )
            .unwrap();
        let request_id = "019f0000-0000-4000-8000-000000000042";
        assert_eq!(
            store
                .begin_pairing_revoke(&challenge.operation_id, request_id)
                .unwrap()
                .reveal_code(),
            revealed.reveal_code()
        );
        drop(store);

        let reopened =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        assert_eq!(
            reopened
                .begin_pairing_revoke(&challenge.operation_id, request_id)
                .unwrap()
                .reveal_code(),
            revealed.reveal_code()
        );
        assert_eq!(
            reopened
                .begin_pairing_revoke(
                    &challenge.operation_id,
                    "019f0000-0000-4000-8000-000000000043",
                )
                .unwrap_err(),
            "request_id_conflict"
        );
        reopened
            .complete_pairing_revoke(&challenge.operation_id, request_id, 302_001)
            .unwrap();
        assert_eq!(
            reopened
                .begin_pairing_revoke(&challenge.operation_id, request_id)
                .unwrap_err(),
            "pairing_revoked"
        );
        let retained: i64 = reopened
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM pairing_sources WHERE sealed_source IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, 0);
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pairing_reaper_preserves_inflight_revoke_and_indexes_terminal_remote_cleanup() {
        let root = temp_root("pairing-revoke-reaper");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let payload = PairingPayload::generate(SessionId::new("session-v2").unwrap()).unwrap();
        let source = store
            .create_pairing_source(
                "019f0000-0000-4000-8000-000000000044",
                "session-v2",
                &payload,
                1_000,
            )
            .unwrap();
        let challenge = store
            .start_user_presence(
                "019f0000-0000-4000-8000-000000000045",
                UserPresenceBinding::PairingReveal {
                    session_id: "session-v2".into(),
                    pair_id: source.pair_id.clone(),
                },
                1_001,
            )
            .unwrap();
        store
            .confirm_user_presence(
                &challenge.operation_id,
                &challenge.intent_id,
                &challenge.token,
                &challenge.action_hash,
                1_002,
            )
            .unwrap();
        store
            .consume_pairing_reveal(
                &challenge.operation_id,
                "019f0000-0000-4000-8000-000000000046",
                1_003,
            )
            .unwrap();
        let revoke_request = "019f0000-0000-4000-8000-000000000047";
        store
            .begin_pairing_revoke(&challenge.operation_id, revoke_request)
            .unwrap();

        store.reap_expired(302_000, 256).unwrap();
        assert!(store
            .begin_pairing_revoke(&challenge.operation_id, revoke_request)
            .is_ok());
        assert!(store
            .pairing_descriptor_cleanup_targets(8)
            .unwrap()
            .is_empty());
        let (state, has_source): (String, bool) = store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT state,sealed_source IS NOT NULL FROM pairing_sources WHERE pair_id=?1",
                params![source.pair_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "revealed");
        assert!(has_source);

        store
            .complete_pairing_revoke(&challenge.operation_id, revoke_request, 302_001)
            .unwrap();
        assert!(store
            .pairing_descriptor_cleanup_targets(0)
            .unwrap()
            .is_empty());
        assert_eq!(
            store.pairing_descriptor_cleanup_targets(8).unwrap(),
            vec![PairingDescriptorCleanupTarget {
                pair_id: source.pair_id,
                session_id: "session-v2".into(),
            }]
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pairing_claim_persists_before_remote_mutation_and_resumes_after_restart() {
        let root = temp_root("pairing-claim-restart");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let payload = PairingPayload::generate(SessionId::new("session-v2").unwrap()).unwrap();
        let source = PairingSourceSecretV2::create(&payload, 1_000).unwrap();
        let challenge = store
            .start_user_presence(
                "019f0000-0000-4000-8000-000000000006",
                UserPresenceBinding::PairingImport {
                    pairing_code: source.reveal_code(),
                },
                1_001,
            )
            .unwrap();
        store
            .confirm_user_presence(
                &challenge.operation_id,
                &challenge.intent_id,
                &challenge.token,
                &challenge.action_hash,
                1_002,
            )
            .unwrap();
        let request_id = "019f0000-0000-4000-8000-000000000007";
        let (_, first_claim) = store
            .begin_pairing_claim(
                request_id,
                &challenge.operation_id,
                source.descriptor(),
                INSTALLATION_PRINCIPAL,
                1_003,
            )
            .unwrap();
        assert_eq!(
            store
                .pairing_claim_state(&challenge.operation_id, Some(request_id))
                .unwrap(),
            Some("claimed".into())
        );
        drop(store);

        let reopened =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let (_, resumed) = reopened
            .load_pairing_claim_for_request(
                &challenge.operation_id,
                request_id,
                INSTALLATION_PRINCIPAL,
                1_004,
            )
            .unwrap();
        assert_eq!(resumed.descriptor, first_claim.descriptor);
        assert_eq!(resumed.payload.session_id, payload.session_id);
        assert_eq!(
            reopened
                .load_pairing_claim_for_request(
                    &challenge.operation_id,
                    "019f0000-0000-4000-8000-000000000008",
                    INSTALLATION_PRINCIPAL,
                    1_004,
                )
                .unwrap_err(),
            "request_id_conflict"
        );
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pairing_claim_install_and_finalize_are_restart_safe_and_idempotent() {
        let root = temp_root("pairing-claim-finalize");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let payload = PairingPayload::generate(SessionId::new("session-v2").unwrap()).unwrap();
        let source = PairingSourceSecretV2::create(&payload, 1_000).unwrap();
        let challenge = store
            .start_user_presence(
                "019f0000-0000-4000-8000-000000000009",
                UserPresenceBinding::PairingImport {
                    pairing_code: source.reveal_code(),
                },
                1_001,
            )
            .unwrap();
        store
            .confirm_user_presence(
                &challenge.operation_id,
                &challenge.intent_id,
                &challenge.token,
                &challenge.action_hash,
                1_002,
            )
            .unwrap();
        store
            .begin_pairing_claim(
                "019f0000-0000-4000-8000-000000000010",
                &challenge.operation_id,
                source.descriptor(),
                INSTALLATION_PRINCIPAL,
                1_003,
            )
            .unwrap();
        store
            .mark_pairing_claim_installed(&challenge.operation_id, "session-v2")
            .unwrap();
        drop(store);

        let reopened =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        assert_eq!(
            reopened
                .pairing_claim_result_session(
                    &challenge.operation_id,
                    "019f0000-0000-4000-8000-000000000010",
                )
                .unwrap(),
            Some("session-v2".into())
        );
        let finalize_request = "019f0000-0000-4000-8000-000000000011";
        assert!(!reopened
            .pairing_claim_remote_consumed(&challenge.operation_id, finalize_request)
            .unwrap());
        assert_eq!(
            reopened.complete_pairing_claim(&challenge.operation_id, finalize_request, 1_004),
            Err("pairing_outcome_unknown".into())
        );
        reopened
            .mark_pairing_claim_remote_consumed(&challenge.operation_id, finalize_request)
            .unwrap();
        assert!(reopened
            .pairing_claim_remote_consumed(&challenge.operation_id, finalize_request)
            .unwrap());
        assert_eq!(
            reopened.mark_pairing_claim_remote_consumed(
                &challenge.operation_id,
                "019f0000-0000-4000-8000-000000000012",
            ),
            Err("request_id_conflict".into())
        );
        reopened
            .complete_pairing_claim(&challenge.operation_id, finalize_request, 1_004)
            .unwrap();
        reopened
            .complete_pairing_claim(&challenge.operation_id, finalize_request, 1_004)
            .unwrap();
        assert_eq!(
            reopened.complete_pairing_claim(
                &challenge.operation_id,
                "019f0000-0000-4000-8000-000000000012",
                1_004,
            ),
            Err("request_id_conflict".into())
        );
        let connection = reopened.connection.lock().unwrap();
        let (state, sealed, bytes): (String, Option<Vec<u8>>, i64) = connection
            .query_row(
                "SELECT state,sealed_resume,logical_bytes FROM pairing_claims WHERE operation_id=?1",
                params![challenge.operation_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, "consumed");
        assert!(sealed.is_none());
        assert_eq!(bytes, 0);
        drop(connection);
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pairing_reaper_never_removes_active_claim_and_erases_expired_resume_secret() {
        let root = temp_root("pairing-claim-reaper");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let payload = PairingPayload::generate(SessionId::new("session-v2").unwrap()).unwrap();
        let source = PairingSourceSecretV2::create(&payload, 1_000).unwrap();
        let challenge = store
            .start_user_presence(
                "019f0000-0000-4000-8000-000000000013",
                UserPresenceBinding::PairingImport {
                    pairing_code: source.reveal_code(),
                },
                1_001,
            )
            .unwrap();
        store
            .confirm_user_presence(
                &challenge.operation_id,
                &challenge.intent_id,
                &challenge.token,
                &challenge.action_hash,
                1_002,
            )
            .unwrap();
        store
            .begin_pairing_claim(
                "019f0000-0000-4000-8000-000000000014",
                &challenge.operation_id,
                source.descriptor(),
                INSTALLATION_PRINCIPAL,
                1_003,
            )
            .unwrap();
        store
            .reap_expired(1_003 + PAIRING_CLAIM_RESUME_TTL_MS, 16)
            .unwrap();
        assert!(store
            .load_pairing_claim(
                &challenge.operation_id,
                INSTALLATION_PRINCIPAL,
                1_003 + PAIRING_CLAIM_RESUME_TTL_MS,
            )
            .is_ok());
        store
            .reap_expired(1_004 + PAIRING_CLAIM_RESUME_TTL_MS, 16)
            .unwrap();
        assert_eq!(
            store
                .pairing_claim_state(&challenge.operation_id, None)
                .unwrap(),
            Some("claimed_expired".into())
        );
        assert_eq!(
            store
                .load_pairing_claim(
                    &challenge.operation_id,
                    INSTALLATION_PRINCIPAL,
                    1_004 + PAIRING_CLAIM_RESUME_TTL_MS,
                )
                .unwrap_err(),
            "pairing_not_found"
        );
        let connection = store.connection.lock().unwrap();
        let (sealed, bytes): (Option<Vec<u8>>, i64) = connection
            .query_row(
                "SELECT sealed_resume,logical_bytes FROM pairing_claims WHERE operation_id=?1",
                params![challenge.operation_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(sealed.is_none());
        assert_eq!(bytes, 0);
        drop(connection);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn control_tombstones_are_retained_for_seven_days_then_reaped() {
        let root = temp_root("control-tombstone-retention");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let terminal_expiry = 1_000 + CONTROL_TOMBSTONE_TTL_MS;
        {
            let connection = store.connection.lock().unwrap();
            connection
                .execute(
                    "INSERT INTO confirmation_intents(
                       intent_id,account_id,session_id,request_id,turn_id,owner_binding,
                       action_hash,expires_at_ms,state,sealed_payload,terminal_expires_at_ms,
                       logical_bytes
                     ) VALUES('terminal-confirm','account','session','request','turn','owner',
                       'hash',1,'cancelled',NULL,?1,0)",
                    params![terminal_expiry as i64],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO pending_cancel_projections(
                       pending_id,account_id,session_id,request_id,turn_id,owner_binding,
                       created_at_ms,logical_bytes
                     ) VALUES('terminal-confirm','account','session','request','turn','owner',1,0)",
                    [],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO user_presence_intents(
                       operation_id,intent_id,request_id,owner_binding,kind,action_hash,
                       expires_at_ms,state,sealed_payload,sealed_response,
                       terminal_expires_at_ms,logical_bytes
                     ) VALUES('terminal-presence','intent','request','owner','session_archive',
                       'hash',1,'consumed',NULL,NULL,?1,0)",
                    params![terminal_expiry as i64],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO pairing_sources(
                       pair_id,request_id,session_id,owner_binding,expires_at_ms,state,
                       sealed_source,terminal_expires_at_ms,logical_bytes
                     ) VALUES('terminal-source','source-request','session','owner',1,'revoked',
                       NULL,?1,0)",
                    params![terminal_expiry as i64],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO pairing_claims(
                       operation_id,request_id,pair_id,owner_binding,state,resume_expires_at_ms,
                       installed_session_id,finalize_request_id,sealed_resume,
                       terminal_expires_at_ms,logical_bytes
                     ) VALUES('terminal-claim','claim-request','pair','owner','consumed',1,
                       'session','finalize',NULL,?1,0)",
                    params![terminal_expiry as i64],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO pairing_revocations(
                       operation_id,request_id,pair_id,owner_binding,state,
                       terminal_expires_at_ms,logical_bytes
                     ) VALUES('terminal-revoke','revoke-request','pair','owner','completed',?1,0)",
                    params![terminal_expiry as i64],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO pairing_revocations(
                       operation_id,request_id,pair_id,owner_binding,state,
                       terminal_expires_at_ms,logical_bytes
                     ) VALUES('active-revoke','active-revoke-request','pair','owner','in_flight',0,0)",
                    [],
                )
                .unwrap();
        }

        store.reap_expired(terminal_expiry, 256).unwrap();
        let connection = store.connection.lock().unwrap();
        for (table, column, id) in [
            ("confirmation_intents", "intent_id", "terminal-confirm"),
            ("user_presence_intents", "operation_id", "terminal-presence"),
            ("pairing_sources", "pair_id", "terminal-source"),
            ("pairing_claims", "operation_id", "terminal-claim"),
            ("pairing_revocations", "operation_id", "terminal-revoke"),
        ] {
            let exists: bool = connection
                .query_row(
                    &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE {column}=?1)"),
                    params![id],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(exists, "{table} tombstone was reaped at its boundary");
        }
        drop(connection);

        store.reap_expired(terminal_expiry + 1, 256).unwrap();
        let connection = store.connection.lock().unwrap();
        for (table, column, id) in [
            ("confirmation_intents", "intent_id", "terminal-confirm"),
            ("user_presence_intents", "operation_id", "terminal-presence"),
            ("pairing_sources", "pair_id", "terminal-source"),
            ("pairing_claims", "operation_id", "terminal-claim"),
            ("pairing_revocations", "operation_id", "terminal-revoke"),
        ] {
            let exists: bool = connection
                .query_row(
                    &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE {column}=?1)"),
                    params![id],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(!exists, "{table} tombstone exceeded its retention");
        }
        let projection_exists: bool = connection
            .query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM pending_cancel_projections WHERE pending_id='terminal-confirm'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!projection_exists);
        let active_revoke_exists: bool = connection
            .query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM pairing_revocations WHERE operation_id='active-revoke'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(active_revoke_exists);
        drop(connection);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn control_tombstone_v10_migration_is_bounded_and_runs_once() {
        let root = temp_root("control-tombstone-v10-migration");
        let credential_store = credential_store(&root);
        {
            let store =
                AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1)
                    .unwrap();
            let connection = store.connection.lock().unwrap();
            connection
                .execute_batch(
                    "INSERT INTO confirmation_intents VALUES(
                       'terminal-confirm','account','session','request','turn','owner','hash',1,
                       'consumed',NULL,0,0);
                     INSERT INTO user_presence_intents VALUES(
                       'terminal-presence','intent','presence-request','owner','session_archive',
                       'hash',1,'expired',NULL,NULL,0,0);
                     INSERT INTO pairing_sources VALUES(
                       'terminal-source','source-request','session','owner',1,'expired',NULL,0,0);
                     INSERT INTO pairing_claims VALUES(
                       'terminal-claim','claim-request','pair','owner','claimed_expired',1,NULL,NULL,
                       NULL,0,0);
                     INSERT INTO pairing_revocations VALUES(
                       'terminal-revoke','revoke-request','pair','owner','completed',0,0);
                     UPDATE control_metadata SET value='9' WHERE key='schema_version';",
                )
                .unwrap();
        }

        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let expiries = {
            let connection = store.connection.lock().unwrap();
            let schema: String = connection
                .query_row(
                    "SELECT value FROM control_metadata WHERE key='schema_version'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(schema, SCHEMA_VERSION.to_string());
            [
                ("confirmation_intents", "intent_id", "terminal-confirm"),
                ("user_presence_intents", "operation_id", "terminal-presence"),
                ("pairing_sources", "pair_id", "terminal-source"),
                ("pairing_claims", "operation_id", "terminal-claim"),
                ("pairing_revocations", "operation_id", "terminal-revoke"),
            ]
            .map(|(table, column, id)| {
                connection
                    .query_row(
                        &format!("SELECT terminal_expires_at_ms FROM {table} WHERE {column}=?1"),
                        params![id],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap()
            })
        };
        assert!(expiries.iter().all(|expiry| *expiry > 0));
        drop(store);

        let reopened =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let retained_expiry: i64 = reopened
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT terminal_expires_at_ms FROM pairing_revocations
                 WHERE operation_id='terminal-revoke'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained_expiry, expiries[4]);
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn mutation_create(
        request_id: &str,
        owner: &str,
        bytes: &[u8],
    ) -> isyncyou_webui::MutationIntentCreate {
        isyncyou_webui::MutationIntentCreate {
            request_id: request_id.into(),
            owner: owner.into(),
            purpose: isyncyou_webui::MutationPurpose::OnedriveUpload {
                account: "controlled".into(),
                parent: "root".into(),
                name: "fixture.bin".into(),
            },
            total_bytes: bytes.len() as u64,
            sha256: sha256_hex(bytes),
        }
    }

    #[test]
    fn mutation_intent_create_replay_requires_identical_semantic_payload() {
        let root = temp_root("mutation-create-replay");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let request_id = "019f0000-0000-4000-8000-000000000101";
        let first = mutation_create(request_id, "owner-a", b"first");
        let info = store.create_mutation_intent(&first, 1_000).unwrap();
        assert_eq!(
            store
                .create_mutation_intent(&first, 1_001)
                .unwrap()
                .intent_id,
            info.intent_id
        );
        let changed = mutation_create(request_id, "owner-a", b"changed");
        assert_eq!(
            store.create_mutation_intent(&changed, 1_002),
            Err("request_id_conflict".into())
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mutation_intent_staging_is_sealed_and_commit_replays_result() {
        let root = temp_root("mutation-sealed-replay");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let mut payload = vec![b'a'; isyncyou_webui::MUTATION_CHUNK_BYTES];
        payload.extend_from_slice(b"tail-marker");
        let create = mutation_create("019f0000-0000-4000-8000-000000000102", "owner-a", &payload);
        let info = store.create_mutation_intent(&create, 1_000).unwrap();
        let first = &payload[..isyncyou_webui::MUTATION_CHUNK_BYTES];
        let second = &payload[isyncyou_webui::MUTATION_CHUNK_BYTES..];
        let first_request = "019f0000-0000-4000-8000-000000000103";
        store
            .put_mutation_chunk(
                "owner-a",
                first_request,
                &info.intent_id,
                0,
                0,
                &sha256_hex(first),
                first,
                1_001,
            )
            .unwrap();
        store
            .put_mutation_chunk(
                "owner-a",
                first_request,
                &info.intent_id,
                0,
                0,
                &sha256_hex(first),
                first,
                1_002,
            )
            .unwrap();
        store
            .put_mutation_chunk(
                "owner-a",
                "019f0000-0000-4000-8000-000000000104",
                &info.intent_id,
                1,
                isyncyou_webui::MUTATION_CHUNK_BYTES as u64,
                &sha256_hex(second),
                second,
                1_003,
            )
            .unwrap();
        let sealed = std::fs::read(
            root.join("agent-control/mutation-staging")
                .join(&info.intent_id)
                .join("1.chunk"),
        )
        .unwrap();
        assert!(!sealed.windows(second.len()).any(|window| window == second));
        let commit_request = "019f0000-0000-4000-8000-000000000105";
        match store
            .begin_mutation_commit(
                "owner-a",
                commit_request,
                &info.intent_id,
                payload.len() as u64,
                &sha256_hex(&payload),
                1_004,
            )
            .unwrap()
        {
            MutationCommitStart::Execute { source, .. } => {
                assert_eq!(source.read_range(0, payload.len()).unwrap(), payload);
                let boundary = isyncyou_webui::MUTATION_CHUNK_BYTES - 4;
                assert_eq!(
                    source.read_range(boundary as u64, 12).unwrap(),
                    payload[boundary..boundary + 12]
                );
            }
            MutationCommitStart::Replay(_) => panic!("first commit unexpectedly replayed"),
        }
        let result = json!({"ok": true, "id": "opaque"});
        store
            .complete_mutation_commit("owner-a", commit_request, &info.intent_id, &result, 1_005)
            .unwrap();
        assert_eq!(
            store
                .begin_mutation_commit(
                    "owner-a",
                    commit_request,
                    &info.intent_id,
                    payload.len() as u64,
                    &sha256_hex(&payload),
                    1_005,
                )
                .unwrap(),
            MutationCommitStart::Replay(result)
        );
        assert!(!root
            .join("agent-control/mutation-staging")
            .join(&info.intent_id)
            .exists());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mutation_cleanup_failure_is_retried_before_commit_replay() {
        let root = temp_root("mutation-cleanup-retry");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let payload = b"sealed payload";
        let create = mutation_create("019f0000-0000-4000-8000-000000000130", "owner-a", payload);
        let info = store.create_mutation_intent(&create, 1_000).unwrap();
        store
            .put_mutation_chunk(
                "owner-a",
                "019f0000-0000-4000-8000-000000000131",
                &info.intent_id,
                0,
                0,
                &sha256_hex(payload),
                payload,
                1_001,
            )
            .unwrap();
        let commit_request = "019f0000-0000-4000-8000-000000000132";
        assert!(matches!(
            store
                .begin_mutation_commit(
                    "owner-a",
                    commit_request,
                    &info.intent_id,
                    payload.len() as u64,
                    &sha256_hex(payload),
                    1_002,
                )
                .unwrap(),
            MutationCommitStart::Execute { .. }
        ));
        let intent_dir = root
            .join("agent-control/mutation-staging")
            .join(&info.intent_id);
        let blocker = intent_dir.join("unexpected");
        std::fs::write(&blocker, b"block cleanup").unwrap();
        let result = json!({"ok": true});
        assert_eq!(
            store.complete_mutation_commit(
                "owner-a",
                commit_request,
                &info.intent_id,
                &result,
                1_003,
            ),
            Err("mutation_intent_outcome_unknown".into())
        );
        std::fs::remove_file(blocker).unwrap();
        assert_eq!(
            store
                .begin_mutation_commit(
                    "owner-a",
                    commit_request,
                    &info.intent_id,
                    payload.len() as u64,
                    &sha256_hex(payload),
                    1_004,
                )
                .unwrap(),
            MutationCommitStart::Replay(result)
        );
        assert!(!intent_dir.exists());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mutation_terminal_commit_state_is_not_time_resolved_by_interactive_expiry() {
        let root = temp_root("mutation-terminal-after-interactive-expiry");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let payload = b"sealed payload";
        let sha = sha256_hex(payload);

        let committed = store
            .create_mutation_intent(
                &mutation_create("019f0000-0000-4000-8000-000000000143", "owner-a", payload),
                1_000,
            )
            .unwrap();
        store
            .put_mutation_chunk(
                "owner-a",
                "019f0000-0000-4000-8000-000000000144",
                &committed.intent_id,
                0,
                0,
                &sha,
                payload,
                1_001,
            )
            .unwrap();
        let committed_request = "019f0000-0000-4000-8000-000000000145";
        assert!(matches!(
            store
                .begin_mutation_commit(
                    "owner-a",
                    committed_request,
                    &committed.intent_id,
                    payload.len() as u64,
                    &sha,
                    1_002,
                )
                .unwrap(),
            MutationCommitStart::Execute { .. }
        ));
        let result = json!({"ok": true});
        store
            .complete_mutation_commit(
                "owner-a",
                committed_request,
                &committed.intent_id,
                &result,
                1_003,
            )
            .unwrap();
        assert_eq!(
            store
                .begin_mutation_commit(
                    "owner-a",
                    committed_request,
                    &committed.intent_id,
                    payload.len() as u64,
                    &sha,
                    1_000 + MUTATION_INTENT_TTL_MS + 1,
                )
                .unwrap(),
            MutationCommitStart::Replay(result)
        );

        let ambiguous = store
            .create_mutation_intent(
                &mutation_create("019f0000-0000-4000-8000-000000000146", "owner-a", payload),
                2_000,
            )
            .unwrap();
        store
            .put_mutation_chunk(
                "owner-a",
                "019f0000-0000-4000-8000-000000000147",
                &ambiguous.intent_id,
                0,
                0,
                &sha,
                payload,
                2_001,
            )
            .unwrap();
        let ambiguous_request = "019f0000-0000-4000-8000-000000000148";
        assert!(matches!(
            store
                .begin_mutation_commit(
                    "owner-a",
                    ambiguous_request,
                    &ambiguous.intent_id,
                    payload.len() as u64,
                    &sha,
                    2_002,
                )
                .unwrap(),
            MutationCommitStart::Execute { .. }
        ));
        assert_eq!(
            store.begin_mutation_commit(
                "owner-a",
                ambiguous_request,
                &ambiguous.intent_id,
                payload.len() as u64,
                &sha,
                2_000 + MUTATION_INTENT_TTL_MS + 1,
            ),
            Err("mutation_intent_outcome_unknown".into())
        );

        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mutation_chunk_retry_replaces_only_a_private_unindexed_crash_orphan() {
        let root = temp_root("mutation-chunk-orphan-retry");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let payload = b"sealed payload";
        let info = store
            .create_mutation_intent(
                &mutation_create("019f0000-0000-4000-8000-000000000138", "owner-a", payload),
                1_000,
            )
            .unwrap();
        let intent_dir = root
            .join("agent-control/mutation-staging")
            .join(&info.intent_id);
        create_private_directory(&intent_dir).unwrap();
        let orphan = intent_dir.join("0.chunk");
        std::fs::write(&orphan, b"crash orphan").unwrap();
        secure_file_mode(&orphan).unwrap();

        store
            .put_mutation_chunk(
                "owner-a",
                "019f0000-0000-4000-8000-000000000139",
                &info.intent_id,
                0,
                0,
                &sha256_hex(payload),
                payload,
                1_001,
            )
            .unwrap();
        assert!(matches!(
            store
                .begin_mutation_commit(
                    "owner-a",
                    "019f0000-0000-4000-8000-000000000140",
                    &info.intent_id,
                    payload.len() as u64,
                    &sha256_hex(payload),
                    1_002,
                )
                .unwrap(),
            MutationCommitStart::Execute { .. }
        ));
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mutation_commit_rejects_noncanonical_staged_chunk_path() {
        let root = temp_root("mutation-chunk-path-binding");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let payload = b"sealed payload";
        let info = store
            .create_mutation_intent(
                &mutation_create("019f0000-0000-4000-8000-000000000141", "owner-a", payload),
                1_000,
            )
            .unwrap();
        store
            .put_mutation_chunk(
                "owner-a",
                "019f0000-0000-4000-8000-000000000142",
                &info.intent_id,
                0,
                0,
                &sha256_hex(payload),
                payload,
                1_001,
            )
            .unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE mutation_chunks SET sealed_path='../foreign.chunk'
                 WHERE intent_id=?1 AND chunk_index=0",
                params![info.intent_id],
            )
            .unwrap();

        assert_eq!(
            store.begin_mutation_commit(
                "owner-a",
                "019f0000-0000-4000-8000-000000000143",
                &info.intent_id,
                payload.len() as u64,
                &sha256_hex(payload),
                1_002,
            ),
            Err("mutation_intent_conflict".into())
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mutation_cancel_retries_incomplete_staging_cleanup() {
        let root = temp_root("mutation-cancel-cleanup-retry");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let payload = b"sealed payload";
        let create = mutation_create("019f0000-0000-4000-8000-000000000133", "owner-a", payload);
        let info = store.create_mutation_intent(&create, 1_000).unwrap();
        store
            .put_mutation_chunk(
                "owner-a",
                "019f0000-0000-4000-8000-000000000134",
                &info.intent_id,
                0,
                0,
                &sha256_hex(payload),
                payload,
                1_001,
            )
            .unwrap();
        let intent_dir = root
            .join("agent-control/mutation-staging")
            .join(&info.intent_id);
        let blocker = intent_dir.join("unexpected");
        std::fs::write(&blocker, b"block cleanup").unwrap();
        let cancel_request = "019f0000-0000-4000-8000-000000000135";
        assert_eq!(
            store.cancel_mutation_intent("owner-a", cancel_request, &info.intent_id),
            Err("mutation_intent_failed".into())
        );
        std::fs::remove_file(blocker).unwrap();
        store
            .cancel_mutation_intent("owner-a", cancel_request, &info.intent_id)
            .unwrap();
        assert!(!intent_dir.exists());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mutation_reaper_keeps_intent_active_until_staging_cleanup_succeeds() {
        let root = temp_root("mutation-reaper-cleanup-retry");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let payload = b"sealed payload";
        let info = store
            .create_mutation_intent(
                &mutation_create("019f0000-0000-4000-8000-000000000136", "owner-a", payload),
                1_000,
            )
            .unwrap();
        store
            .put_mutation_chunk(
                "owner-a",
                "019f0000-0000-4000-8000-000000000137",
                &info.intent_id,
                0,
                0,
                &sha256_hex(payload),
                payload,
                1_001,
            )
            .unwrap();
        let intent_dir = root
            .join("agent-control/mutation-staging")
            .join(&info.intent_id);
        let blocker = intent_dir.join("unexpected");
        std::fs::write(&blocker, b"block cleanup").unwrap();
        let expired_at = 1_000 + MUTATION_INTENT_TTL_MS + 1;

        assert_eq!(
            store.reap_expired(expired_at, 16),
            Err("mutation_intent_failed".into())
        );
        let state: String = store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT state FROM mutation_intents WHERE intent_id=?1",
                params![info.intent_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "active");

        std::fs::remove_file(blocker).unwrap();
        assert_eq!(store.reap_expired(expired_at, 16).unwrap(), 1);
        let state: String = store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT state FROM mutation_intents WHERE intent_id=?1",
                params![info.intent_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "expired");
        assert!(!intent_dir.exists());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mutation_terminal_retention_compacts_secrets_and_preserves_request_binding() {
        let root = temp_root("mutation-terminal-retention");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let create_request = "019f0000-0000-4000-8000-000000000106";
        let create = mutation_create(create_request, "owner-a", b"");
        let info = store.create_mutation_intent(&create, 1_000).unwrap();
        let commit_request = "019f0000-0000-4000-8000-000000000107";
        assert!(matches!(
            store
                .begin_mutation_commit(
                    "owner-a",
                    commit_request,
                    &info.intent_id,
                    0,
                    &sha256_hex(b""),
                    1_001,
                )
                .unwrap(),
            MutationCommitStart::Execute { .. }
        ));
        let result = json!({"ok": true, "id": "opaque"});
        let result_bytes = serde_json::to_vec(&result).unwrap();
        let completed_at = 1_002;
        store
            .complete_mutation_commit(
                "owner-a",
                commit_request,
                &info.intent_id,
                &result,
                completed_at,
            )
            .unwrap();

        let terminal_expiry = completed_at + MUTATION_TERMINAL_TTL_MS;
        let (purpose, sealed_result, stored_expiry, logical_bytes): (Vec<u8>, Vec<u8>, i64, i64) =
            store
                .connection
                .lock()
                .unwrap()
                .query_row(
                    "SELECT purpose_json,result_json,terminal_expires_at_ms,logical_bytes
                 FROM mutation_intents WHERE intent_id=?1",
                    params![info.intent_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .unwrap();
        assert!(purpose.is_empty());
        assert_ne!(sealed_result, result_bytes);
        assert_eq!(
            open_row(
                &store.row_wrap_key,
                "mutation-result",
                &info.intent_id,
                &sealed_result,
            )
            .unwrap(),
            result_bytes
        );
        assert_eq!(stored_expiry, terminal_expiry as i64);
        assert!(logical_bytes > 512);

        store.reap_expired(terminal_expiry, 256).unwrap();
        let retained: bool = store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM mutation_intents WHERE intent_id=?1)",
                params![info.intent_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(retained);
        store.reap_expired(terminal_expiry + 1, 256).unwrap();
        assert_eq!(
            store.create_mutation_intent(&create, terminal_expiry + 2),
            Err("mutation_intent_outcome_unknown".into())
        );

        let binding_expiry = 1_000 + MUTATION_REQUEST_BINDING_TTL_MS;
        store.reap_expired(binding_expiry + 1, 256).unwrap();
        let replacement = store
            .create_mutation_intent(&create, binding_expiry + 2)
            .unwrap();
        assert_ne!(replacement.intent_id, info.intent_id);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mutation_retention_v9_migrates_legacy_json_and_expiries_once() {
        let root = temp_root("mutation-retention-v9-migration");
        let credential_store = credential_store(&root);
        let active_request = "019f0000-0000-4000-8000-000000000108";
        let committed_request = "019f0000-0000-4000-8000-000000000109";
        let active = mutation_create(active_request, "owner-a", b"active");
        let committed = mutation_create(committed_request, "owner-b", b"committed");
        let active_purpose = serde_json::to_vec(&active.purpose).unwrap();
        let committed_purpose = serde_json::to_vec(&committed.purpose).unwrap();
        let committed_result = serde_json::to_vec(&json!({"ok": true})).unwrap();
        let (active_id, committed_id, committed_source_expiry) = {
            let store =
                AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1)
                    .unwrap();
            let active_id = store
                .create_mutation_intent(&active, 2_000)
                .unwrap()
                .intent_id;
            let committed_info = store.create_mutation_intent(&committed, 3_000).unwrap();
            let committed_source_expiry = 3_000 + MUTATION_INTENT_TTL_MS;
            {
                let connection = store.connection.lock().unwrap();
                connection
                    .execute(
                        "UPDATE mutation_intents SET purpose_json=?1,logical_bytes=0
                         WHERE intent_id=?2",
                        params![active_purpose, active_id],
                    )
                    .unwrap();
                connection
                    .execute(
                        "UPDATE mutation_intents
                         SET state='committed',purpose_json=?1,result_json=?2,
                             terminal_expires_at_ms=0,logical_bytes=0
                         WHERE intent_id=?3",
                        params![
                            committed_purpose,
                            committed_result,
                            committed_info.intent_id
                        ],
                    )
                    .unwrap();
                connection
                    .execute("UPDATE mutation_request_bindings SET expires_at_ms=0", [])
                    .unwrap();
                connection
                    .execute(
                        "UPDATE control_metadata SET value='8' WHERE key='schema_version'",
                        [],
                    )
                    .unwrap();
            }
            (active_id, committed_info.intent_id, committed_source_expiry)
        };

        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let (active_sealed, committed_purpose_after, committed_sealed, terminal_expiry): (
            Vec<u8>,
            Vec<u8>,
            Vec<u8>,
            i64,
        ) = store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT a.purpose_json,c.purpose_json,c.result_json,c.terminal_expires_at_ms
                 FROM mutation_intents a, mutation_intents c
                 WHERE a.intent_id=?1 AND c.intent_id=?2",
                params![active_id, committed_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            open_row(
                &store.row_wrap_key,
                "mutation-purpose",
                &active_id,
                &active_sealed,
            )
            .unwrap(),
            serde_json::to_vec(&active.purpose).unwrap()
        );
        assert!(committed_purpose_after.is_empty());
        assert_eq!(
            open_row(
                &store.row_wrap_key,
                "mutation-result",
                &committed_id,
                &committed_sealed,
            )
            .unwrap(),
            serde_json::to_vec(&json!({"ok": true})).unwrap()
        );
        assert_eq!(
            terminal_expiry,
            (committed_source_expiry + MUTATION_TERMINAL_TTL_MS) as i64
        );
        let (schema, expired_bindings): (String, i64) = store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT
                   (SELECT value FROM control_metadata WHERE key='schema_version'),
                   (SELECT COUNT(*) FROM mutation_request_bindings WHERE expires_at_ms=0)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(schema, SCHEMA_VERSION.to_string());
        assert_eq!(expired_bindings, 0);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mutation_intent_request_ids_bind_route_and_semantic_payload() {
        let root = temp_root("mutation-request-binding");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let payload = b"two chunks";
        let create_request = "019f0000-0000-4000-8000-000000000120";
        let info = store
            .create_mutation_intent(&mutation_create(create_request, "owner-a", payload), 1_000)
            .unwrap();
        let chunk_request = "019f0000-0000-4000-8000-000000000121";
        store
            .put_mutation_chunk(
                "owner-a",
                chunk_request,
                &info.intent_id,
                0,
                0,
                &sha256_hex(payload),
                payload,
                1_001,
            )
            .unwrap();
        assert_eq!(
            store.put_mutation_chunk(
                "owner-a",
                chunk_request,
                &info.intent_id,
                1,
                isyncyou_webui::MUTATION_CHUNK_BYTES as u64,
                &sha256_hex(b"different"),
                b"different",
                1_002,
            ),
            Err("request_id_conflict".into())
        );
        assert_eq!(
            store.cancel_mutation_intent("owner-a", chunk_request, &info.intent_id),
            Err("request_id_conflict".into())
        );
        let cancel_request = "019f0000-0000-4000-8000-000000000122";
        store
            .cancel_mutation_intent("owner-a", cancel_request, &info.intent_id)
            .unwrap();
        store
            .cancel_mutation_intent("owner-a", cancel_request, &info.intent_id)
            .unwrap();
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mutation_intent_enforces_owner_quota_and_reaps_expired_staging() {
        let root = temp_root("mutation-quota-reap");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let payload = b"bounded";
        let mut first = None;
        for suffix in 110..114 {
            let request_id = format!("019f0000-0000-4000-8000-000000000{suffix}");
            let info = store
                .create_mutation_intent(&mutation_create(&request_id, "owner-a", payload), 1_000)
                .unwrap();
            first.get_or_insert(info);
        }
        let fifth = mutation_create("019f0000-0000-4000-8000-000000000114", "owner-a", payload);
        assert_eq!(
            store.create_mutation_intent(&fifth, 1_001),
            Err("mutation_intent_quota_exceeded".into())
        );
        let first = first.unwrap();
        store
            .put_mutation_chunk(
                "owner-a",
                "019f0000-0000-4000-8000-000000000115",
                &first.intent_id,
                0,
                0,
                &sha256_hex(payload),
                payload,
                1_002,
            )
            .unwrap();
        store
            .reap_expired(1_000 + MUTATION_INTENT_TTL_MS + 1, 16)
            .unwrap();
        assert!(!root
            .join("agent-control/mutation-staging")
            .join(&first.intent_id)
            .exists());
        assert!(store
            .create_mutation_intent(&fifth, 1_000 + MUTATION_INTENT_TTL_MS + 2)
            .is_ok());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mutation_intent_process_quota_is_shared_across_accounts() {
        let root = temp_root("mutation-process-quota");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        for index in 0..MAX_MUTATION_INTENTS_PROCESS {
            let owner = if index < MAX_MUTATION_INTENTS_PER_OWNER {
                "owner-a"
            } else {
                "owner-b"
            };
            let request_id = format!("019f0000-0000-4000-8000-{index:012}");
            store
                .create_mutation_intent(&mutation_create(&request_id, owner, b"bounded"), 1_000)
                .unwrap();
        }
        assert_eq!(
            store.create_mutation_intent(
                &mutation_create(
                    "019f0000-0000-4000-8000-000000000999",
                    "owner-c",
                    b"bounded",
                ),
                1_001,
            ),
            Err("mutation_intent_quota_exceeded".into())
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn agent_control_store_reconstructs_quota_after_unclean_exit() {
        let root = temp_root("mutation-restart-quota");
        let credential_store = credential_store(&root);
        let payload = b"reserved";
        {
            let store =
                AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1)
                    .unwrap();
            for suffix in 200..204 {
                let request_id = format!("019f0000-0000-4000-8000-000000000{suffix}");
                store
                    .create_mutation_intent(
                        &mutation_create(&request_id, "owner-restart", payload),
                        1_000,
                    )
                    .unwrap();
            }
        }
        let reopened =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        assert_eq!(
            reopened.create_mutation_intent(
                &mutation_create(
                    "019f0000-0000-4000-8000-000000000204",
                    "owner-restart",
                    payload,
                ),
                1_001,
            ),
            Err("mutation_intent_quota_exceeded".into())
        );
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mutation_intent_startup_reaper_reconstructs_and_releases_quota() {
        let root = temp_root("mutation-startup-reaper");
        let credential_store = credential_store(&root);
        let payload = b"staged";
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let info = store
            .create_mutation_intent(
                &mutation_create(
                    "019f0000-0000-4000-8000-000000000210",
                    "owner-reaper",
                    payload,
                ),
                1_000,
            )
            .unwrap();
        store
            .put_mutation_chunk(
                "owner-reaper",
                "019f0000-0000-4000-8000-000000000211",
                &info.intent_id,
                0,
                0,
                &sha256_hex(payload),
                payload,
                1_001,
            )
            .unwrap();
        let orphan = root.join("agent-control/mutation-staging/orphan-intent");
        std::fs::create_dir(&orphan).unwrap();
        std::fs::write(orphan.join("0.chunk"), b"orphan").unwrap();
        drop(store);

        let reopened =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        reopened.reap_expired(1_002, 256).unwrap();
        assert!(!orphan.exists());
        assert!(root
            .join("agent-control/mutation-staging")
            .join(&info.intent_id)
            .exists());
        reopened
            .reap_expired(1_000 + MUTATION_INTENT_TTL_MS + 1, 256)
            .unwrap();
        assert!(!root
            .join("agent-control/mutation-staging")
            .join(&info.intent_id)
            .exists());
        assert!(reopened
            .create_mutation_intent(
                &mutation_create(
                    "019f0000-0000-4000-8000-000000000212",
                    "owner-reaper",
                    payload,
                ),
                1_000 + MUTATION_INTENT_TTL_MS + 2,
            )
            .is_ok());
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn agent_control_store_requires_derived_agent_credential_key() {
        agent_control_store_key_uses_domain_hmac_without_master_key_export();
        agent_control_store_separates_sqlcipher_and_row_wrap_subkeys();
    }

    #[test]
    fn agent_control_store_enforces_control_quota_across_multiple_accounts() {
        mutation_intent_process_quota_is_shared_across_accounts();
    }

    #[test]
    fn agent_control_store_rejects_metadata_growth_beyond_global_control_quota() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE confirmation_intents(logical_bytes INTEGER NOT NULL);
                 CREATE TABLE pending_cancel_projections(logical_bytes INTEGER NOT NULL);
                 CREATE TABLE user_presence_intents(logical_bytes INTEGER NOT NULL);
                 CREATE TABLE pairing_sources(logical_bytes INTEGER NOT NULL);
                 CREATE TABLE pairing_claims(logical_bytes INTEGER NOT NULL);
                 CREATE TABLE pairing_revocations(logical_bytes INTEGER NOT NULL);
                 CREATE TABLE mutation_request_bindings(logical_bytes INTEGER NOT NULL);
                 CREATE TABLE product_request_bindings(logical_bytes INTEGER NOT NULL);
                 CREATE TABLE mutation_intents(logical_bytes INTEGER NOT NULL);
                 CREATE TABLE product_request_receipts(logical_bytes INTEGER NOT NULL);
                 CREATE TABLE agent_turn_admissions(logical_bytes INTEGER NOT NULL);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO mutation_request_bindings(logical_bytes) VALUES(?1)",
                params![MAX_CONTROL_BYTES],
            )
            .unwrap();
        assert_eq!(
            enforce_control_quota(&connection, 1),
            Err("control_store_quota_exceeded".into())
        );
    }

    #[test]
    fn agent_control_store_lookup_verifies_account_session_route_and_owner_binding() {
        let root = temp_root("owner-binding");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let payload = b"owner-bound";
        let info = store
            .create_mutation_intent(
                &mutation_create("019f0000-0000-4000-8000-000000000220", "owner-a", payload),
                1_000,
            )
            .unwrap();
        assert_eq!(
            store.put_mutation_chunk(
                "owner-b",
                "019f0000-0000-4000-8000-000000000221",
                &info.intent_id,
                0,
                0,
                &sha256_hex(payload),
                payload,
                1_001,
            ),
            Err("mutation_intent_not_found".into())
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn confirmation_hash_separates_pairing_reveal_import_and_archive_variants() {
        let root = temp_root("presence-variant-domain");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let bindings = [
            UserPresenceBinding::Archive {
                session_id: "session-v2".into(),
            },
            UserPresenceBinding::PairingReveal {
                session_id: "session-v2".into(),
                pair_id: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            },
            UserPresenceBinding::PairingImport {
                pairing_code: format!("isy2.{}.{}", "A".repeat(32), "B".repeat(43)),
            },
        ];
        let hashes = bindings
            .into_iter()
            .enumerate()
            .map(|(index, binding)| {
                store
                    .start_user_presence(
                        &format!("019f0000-0000-4000-8000-00000000023{index}"),
                        binding,
                        1_000,
                    )
                    .unwrap()
                    .action_hash
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(hashes.len(), 3);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pairing_transfer_requires_confirmed_user_presence_intent() {
        let root = temp_root("pairing-confirm-required");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let payload = PairingPayload::generate(SessionId::new("session-v2").unwrap()).unwrap();
        let source = store
            .create_pairing_source(
                "019f0000-0000-4000-8000-000000000340",
                "session-v2",
                &payload,
                1_000,
            )
            .unwrap();
        let challenge = store
            .start_user_presence(
                "019f0000-0000-4000-8000-000000000341",
                UserPresenceBinding::PairingReveal {
                    session_id: "session-v2".into(),
                    pair_id: source.pair_id,
                },
                1_001,
            )
            .unwrap();
        assert_eq!(
            store
                .consume_pairing_reveal(
                    &challenge.operation_id,
                    "019f0000-0000-4000-8000-000000000342",
                    1_002,
                )
                .unwrap_err(),
            "presence_not_authorized"
        );
        store
            .confirm_user_presence(
                &challenge.operation_id,
                &challenge.intent_id,
                &challenge.token,
                &challenge.action_hash,
                1_003,
            )
            .unwrap();
        assert!(store
            .consume_pairing_reveal(
                &challenge.operation_id,
                "019f0000-0000-4000-8000-000000000342",
                1_004,
            )
            .is_ok());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pairing_create_seals_locally_without_graph_write_or_secret_reveal() {
        let root = temp_root("pairing-create-local-only");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let payload = PairingPayload::generate(SessionId::new("session-v2").unwrap()).unwrap();
        let source = store
            .create_pairing_source(
                "019f0000-0000-4000-8000-000000000350",
                "session-v2",
                &payload,
                1_000,
            )
            .unwrap();
        assert!(!source.pair_id.is_empty());
        let (state, sealed): (String, Vec<u8>) = store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT state,sealed_source FROM pairing_sources WHERE pair_id=?1",
                params![source.pair_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "local");
        assert!(!sealed
            .windows("session-v2".len())
            .any(|window| window == b"session-v2"));
        let production = include_str!("agent_control_store.rs")
            .split("pub(crate) fn create_pairing_source")
            .nth(1)
            .unwrap()
            .split("pub(crate) fn consume_pairing_reveal")
            .next()
            .unwrap();
        assert!(!production.contains("OneDrivePairingTransportV2"));
        assert!(!production.contains("reveal_code"));
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pairing_reveal_creates_remote_descriptor_only_after_authorization() {
        let host = include_str!("lib.rs");
        let reveal = host
            .split("fn session_pairing_reveal(")
            .nth(1)
            .unwrap()
            .split("fn session_pairing_claim(")
            .next()
            .unwrap();
        let consume = reveal.find("consume_pairing_reveal").unwrap();
        let transport = reveal.find("OneDrivePairingTransportV2::new").unwrap();
        let create = reveal.find(".create_or_adopt(&source)").unwrap();
        let response = reveal.find("\"pairing_code\"").unwrap();
        assert!(consume < transport && transport < create && create < response);

        let root = temp_root("pairing-remote-after-authorization");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let payload = PairingPayload::generate(SessionId::new("session-v2").unwrap()).unwrap();
        let source = store
            .create_pairing_source(
                "019f0000-0000-4000-8000-000000000360",
                "session-v2",
                &payload,
                1_000,
            )
            .unwrap();
        let challenge = store
            .start_user_presence(
                "019f0000-0000-4000-8000-000000000361",
                UserPresenceBinding::PairingReveal {
                    session_id: "session-v2".into(),
                    pair_id: source.pair_id.clone(),
                },
                1_001,
            )
            .unwrap();
        assert_eq!(
            store
                .consume_pairing_reveal(
                    &challenge.operation_id,
                    "019f0000-0000-4000-8000-000000000362",
                    1_002,
                )
                .unwrap_err(),
            "presence_not_authorized"
        );
        let state: String = store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT state FROM pairing_sources WHERE pair_id=?1",
                params![source.pair_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "local");
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pairing_reveal_crash_before_authorization_keeps_server_token_retryable() {
        let root = temp_root("pairing-crash-before-authorization");
        let credential_store = credential_store(&root);
        let payload = PairingPayload::generate(SessionId::new("session-v2").unwrap()).unwrap();
        let challenge = {
            let store =
                AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1)
                    .unwrap();
            let source = store
                .create_pairing_source(
                    "019f0000-0000-4000-8000-000000000370",
                    "session-v2",
                    &payload,
                    1_000,
                )
                .unwrap();
            store
                .start_user_presence(
                    "019f0000-0000-4000-8000-000000000371",
                    UserPresenceBinding::PairingReveal {
                        session_id: "session-v2".into(),
                        pair_id: source.pair_id,
                    },
                    1_001,
                )
                .unwrap()
        };
        let reopened =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        reopened
            .confirm_user_presence(
                &challenge.operation_id,
                &challenge.intent_id,
                &challenge.token,
                &challenge.action_hash,
                1_002,
            )
            .unwrap();
        assert!(reopened
            .consume_pairing_reveal(
                &challenge.operation_id,
                "019f0000-0000-4000-8000-000000000372",
                1_003,
            )
            .is_ok());
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pairing_reveal_crash_after_authorization_replays_same_code() {
        let root = temp_root("pairing-crash-after-authorization");
        let credential_store = credential_store(&root);
        let payload = PairingPayload::generate(SessionId::new("session-v2").unwrap()).unwrap();
        let (challenge, first_digest) = {
            let store =
                AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1)
                    .unwrap();
            let source = store
                .create_pairing_source(
                    "019f0000-0000-4000-8000-000000000380",
                    "session-v2",
                    &payload,
                    1_000,
                )
                .unwrap();
            let challenge = store
                .start_user_presence(
                    "019f0000-0000-4000-8000-000000000381",
                    UserPresenceBinding::PairingReveal {
                        session_id: "session-v2".into(),
                        pair_id: source.pair_id,
                    },
                    1_001,
                )
                .unwrap();
            store
                .confirm_user_presence(
                    &challenge.operation_id,
                    &challenge.intent_id,
                    &challenge.token,
                    &challenge.action_hash,
                    1_002,
                )
                .unwrap();
            let revealed = store
                .consume_pairing_reveal(
                    &challenge.operation_id,
                    "019f0000-0000-4000-8000-000000000382",
                    1_003,
                )
                .unwrap();
            (challenge, sha256_hex(revealed.reveal_code().as_bytes()))
        };
        let reopened =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let replayed = reopened
            .consume_pairing_reveal(
                &challenge.operation_id,
                "019f0000-0000-4000-8000-000000000382",
                1_004,
            )
            .unwrap();
        assert_eq!(sha256_hex(replayed.reveal_code().as_bytes()), first_digest);
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pairing_crash_after_claim_resumes_from_encrypted_local_state() {
        pairing_claim_persists_before_remote_mutation_and_resumes_after_restart();
    }

    #[test]
    fn pairing_reveal_secret_is_erased_on_expiry_consumed_or_revoke() {
        pairing_revoke_retains_authority_until_remote_completion_and_replays_exact_request();
        pairing_reaper_never_removes_active_claim_and_erases_expired_resume_secret();
    }

    #[test]
    fn pairing_descriptor_and_local_resume_reaper_preserve_no_usable_transfer_key() {
        pairing_reaper_never_removes_active_claim_and_erases_expired_resume_secret();
    }

    #[test]
    fn pairing_transfer_revoke_prevents_redeem() {
        pairing_revoke_retains_authority_until_remote_completion_and_replays_exact_request();
    }

    #[test]
    fn mutation_intent_requires_storage_not_low_and_reserved_free_space() {
        let total_bytes = 64 * 1024 * 1024;
        let required = total_bytes + MUTATION_FREE_SPACE_RESERVE;
        assert_eq!(require_mutation_free_space(required, required), Ok(()));
        assert_eq!(
            require_mutation_free_space(required, required - 1),
            Err("mutation_intent_insufficient_storage".into())
        );
        assert_eq!(
            total_bytes.checked_add(MUTATION_FREE_SPACE_RESERVE),
            Some(required)
        );
    }

    #[test]
    fn mutation_intent_rejects_missing_overlapping_or_conflicting_chunks() {
        let root = temp_root("mutation-chunk-conflicts");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let mut payload = vec![b'a'; isyncyou_webui::MUTATION_CHUNK_BYTES];
        payload.extend_from_slice(b"tail");
        let info = store
            .create_mutation_intent(
                &mutation_create("019f0000-0000-4000-8000-000000000300", "owner-a", &payload),
                1_000,
            )
            .unwrap();
        let first = &payload[..isyncyou_webui::MUTATION_CHUNK_BYTES];
        store
            .put_mutation_chunk(
                "owner-a",
                "019f0000-0000-4000-8000-000000000301",
                &info.intent_id,
                0,
                0,
                &sha256_hex(first),
                first,
                1_001,
            )
            .unwrap();
        assert_eq!(
            store.begin_mutation_commit(
                "owner-a",
                "019f0000-0000-4000-8000-000000000302",
                &info.intent_id,
                payload.len() as u64,
                &sha256_hex(&payload),
                1_002,
            ),
            Err("mutation_intent_conflict".into())
        );
        assert_eq!(
            store.put_mutation_chunk(
                "owner-a",
                "019f0000-0000-4000-8000-000000000303",
                &info.intent_id,
                1,
                0,
                &sha256_hex(b"tail"),
                b"tail",
                1_003,
            ),
            Err("mutation_intent_invalid".into())
        );
        assert_eq!(
            store.put_mutation_chunk(
                "owner-a",
                "019f0000-0000-4000-8000-000000000304",
                &info.intent_id,
                0,
                0,
                &sha256_hex(b"changed"),
                b"changed",
                1_004,
            ),
            Err("mutation_intent_conflict".into())
        );
        store
            .put_mutation_chunk(
                "owner-a",
                "019f0000-0000-4000-8000-000000000305",
                &info.intent_id,
                1,
                isyncyou_webui::MUTATION_CHUNK_BYTES as u64,
                &sha256_hex(b"tail"),
                b"tail",
                1_005,
            )
            .unwrap();
        assert!(matches!(
            store
                .begin_mutation_commit(
                    "owner-a",
                    "019f0000-0000-4000-8000-000000000306",
                    &info.intent_id,
                    payload.len() as u64,
                    &sha256_hex(&payload),
                    1_006,
                )
                .unwrap(),
            MutationCommitStart::Execute { .. }
        ));
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mutation_intent_commit_verifies_total_hash_length_expiry_and_binding() {
        let root = temp_root("mutation-commit-validation");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let payload = b"verified-payload";
        let info = store
            .create_mutation_intent(
                &mutation_create("019f0000-0000-4000-8000-000000000310", "owner-a", payload),
                1_000,
            )
            .unwrap();
        store
            .put_mutation_chunk(
                "owner-a",
                "019f0000-0000-4000-8000-000000000311",
                &info.intent_id,
                0,
                0,
                &sha256_hex(payload),
                payload,
                1_001,
            )
            .unwrap();
        assert_eq!(
            store.begin_mutation_commit(
                "owner-b",
                "019f0000-0000-4000-8000-000000000312",
                &info.intent_id,
                payload.len() as u64,
                &sha256_hex(payload),
                1_002,
            ),
            Err("mutation_intent_not_found".into())
        );
        assert_eq!(
            store.begin_mutation_commit(
                "owner-a",
                "019f0000-0000-4000-8000-000000000313",
                &info.intent_id,
                payload.len() as u64 + 1,
                &sha256_hex(payload),
                1_003,
            ),
            Err("mutation_intent_conflict".into())
        );
        assert_eq!(
            store.begin_mutation_commit(
                "owner-a",
                "019f0000-0000-4000-8000-000000000314",
                &info.intent_id,
                payload.len() as u64,
                &sha256_hex(b"different"),
                1_004,
            ),
            Err("mutation_intent_conflict".into())
        );
        assert_eq!(
            store.begin_mutation_commit(
                "owner-a",
                "019f0000-0000-4000-8000-000000000315",
                &info.intent_id,
                payload.len() as u64,
                &sha256_hex(payload),
                1_000 + MUTATION_INTENT_TTL_MS + 1,
            ),
            Err("mutation_intent_expired".into())
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mutation_intent_enforces_per_session_process_chunk_and_byte_quotas() {
        let root = temp_root("mutation-combined-quotas");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let mut first_intent = None;
        for index in 0..MAX_MUTATION_INTENTS_PROCESS {
            let owner = if index < MAX_MUTATION_INTENTS_PER_OWNER {
                "owner-a"
            } else {
                "owner-b"
            };
            let request_id = format!("019f0000-0000-4000-8000-{index:012}");
            let info = store
                .create_mutation_intent(&mutation_create(&request_id, owner, b"bounded"), 1_000)
                .unwrap();
            first_intent.get_or_insert(info);
        }
        assert_eq!(
            store.create_mutation_intent(
                &mutation_create(
                    "019f0000-0000-4000-8000-000000000320",
                    "owner-c",
                    b"bounded",
                ),
                1_001,
            ),
            Err("mutation_intent_quota_exceeded".into())
        );
        assert_eq!(
            store.put_mutation_chunk(
                "owner-a",
                "019f0000-0000-4000-8000-000000000321",
                &first_intent.unwrap().intent_id,
                MAX_MUTATION_CHUNKS,
                u64::from(MAX_MUTATION_CHUNKS) * isyncyou_webui::MUTATION_CHUNK_BYTES as u64,
                &sha256_hex(b"x"),
                b"x",
                1_002,
            ),
            Err("mutation_intent_invalid".into())
        );
        {
            let connection = store.connection.lock().unwrap();
            connection
                .execute(
                    "UPDATE mutation_intents SET total_bytes=?1 WHERE owner_binding=?2",
                    params![
                        MAX_MUTATION_STAGED_BYTES + 1,
                        store.mutation_owner_binding("owner-a").unwrap()
                    ],
                )
                .unwrap();
            assert_eq!(
                enforce_mutation_reservation_quota(
                    &connection,
                    &store.mutation_owner_binding("owner-a").unwrap()
                ),
                Err("mutation_intent_quota_exceeded".into())
            );
        }
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mutation_intent_staging_is_sealed_and_cleanup_is_idempotent() {
        let root = temp_root("mutation-cleanup-idempotent");
        let credential_store = credential_store(&root);
        let store =
            AgentControlStore::open(&root, &credential_store, INSTALLATION_PRINCIPAL, 1).unwrap();
        let payload = b"sealed-cleanup";
        let info = store
            .create_mutation_intent(
                &mutation_create("019f0000-0000-4000-8000-000000000330", "owner-a", payload),
                1_000,
            )
            .unwrap();
        store
            .put_mutation_chunk(
                "owner-a",
                "019f0000-0000-4000-8000-000000000331",
                &info.intent_id,
                0,
                0,
                &sha256_hex(payload),
                payload,
                1_001,
            )
            .unwrap();
        let staging = root
            .join("agent-control/mutation-staging")
            .join(&info.intent_id);
        let sealed = std::fs::read(staging.join("0.chunk")).unwrap();
        assert!(!sealed
            .windows(payload.len())
            .any(|window| window == payload));
        let cancel_request = "019f0000-0000-4000-8000-000000000332";
        store
            .cancel_mutation_intent("owner-a", cancel_request, &info.intent_id)
            .unwrap();
        store
            .cancel_mutation_intent("owner-a", cancel_request, &info.intent_id)
            .unwrap();
        assert!(!staging.exists());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn upload_replace_stream_from_staging_without_64m_allocation() {
        let production = include_str!("lib.rs");
        let commit = production
            .split("impl isyncyou_webui::MutationIntentHandler for DaemonMutationIntents")
            .nth(1)
            .unwrap()
            .split("fn cancel(")
            .next()
            .unwrap();
        assert!(commit.contains("upload_streaming_via_ledger"));
        assert!(commit.contains("replace_streaming_via_ledger"));
        assert!(
            commit
                .matches("|offset, len| source.read_range(offset, len)")
                .count()
                >= 2
        );
        let upload = commit
            .split("MutationPurpose::OnedriveUpload")
            .nth(1)
            .unwrap()
            .split("MutationPurpose::OnedriveReplace")
            .next()
            .unwrap();
        let replace = commit
            .split("MutationPurpose::OnedriveReplace")
            .nth(1)
            .unwrap()
            .split("MutationPurpose::MailBody")
            .next()
            .unwrap();
        assert!(!upload.contains("source.read_range(0"));
        assert!(!replace.contains("source.read_range(0"));
    }

    #[test]
    fn no_agent_control_transaction_spans_provider_or_graph_network_io() {
        let production = include_str!("agent_control_store.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for forbidden in [
            "GraphClient",
            "reqwest",
            "OneDrivePairingTransportV2",
            "ProviderTransport",
        ] {
            assert!(
                !production.contains(forbidden),
                "control-store transaction layer contains network type {forbidden}"
            );
        }
    }
}
